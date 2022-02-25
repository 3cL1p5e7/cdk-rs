mod rc_bytes;

use crate::rc_bytes::RcBytes;
use ic_cdk::api::{caller, data_certificate, set_certified_data, time, trap};
use ic_cdk::export::candid::{CandidType, Deserialize, Func, Int, Nat, Principal};
use ic_cdk_macros::{query, update};
use ic_cdk::{print};
use ic_certified_map::{AsHashTree, Hash, HashTree, RbTree};
use num_traits::ToPrimitive;
use serde::Serialize;
use serde_bytes::ByteBuf;
use sha2::Digest;
use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::TryInto;
use std::fmt;

/// The amount of time a batch is kept alive. Modifying the batch
/// delays the expiry further.
const BATCH_EXPIRY_NANOS: u64 = 300_000_000_000;

/// The order in which we pick encodings for certification.
const ENCODING_CERTIFICATION_ORDER: &[&str] = &["identity", "gzip", "compress", "deflate", "br"];

/// The file to serve if the requested file wasn't found.
const INDEX_FILE: &str = "/index.html";

thread_local! {
    static STATE: State = State::default();
    static ASSET_HASHES: RefCell<AssetHashes> = RefCell::new(RbTree::new());
    static CHUNK_HASHES: RefCell<HashMap<Key, ChunkHashes>> = RefCell::new(HashMap::new());
}

type AssetHashes = RbTree<Key, Hash>;
type ChunkHashes = RbTree<Key, Hash>;

#[derive(Default)]
struct State {
    assets: RefCell<HashMap<Key, Asset>>,

    chunks: RefCell<HashMap<ChunkId, Chunk>>,
    next_chunk_id: RefCell<ChunkId>,

    batches: RefCell<HashMap<BatchId, Batch>>,
    next_batch_id: RefCell<BatchId>,

    authorized: RefCell<Vec<Principal>>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
pub struct StableState {
    authorized: Vec<Principal>,
    stable_assets: HashMap<String, Asset>,
}

#[derive(Default, Clone, Debug, CandidType, Deserialize)]
struct AssetEncoding {
    modified: Timestamp,
    content_chunks: Vec<ContentChunk>,
    total_length: usize,
    certified: bool,
    sha256: [u8; 32],
}

// Thanks https://github.com/dfinity/cdk-rs/pull/199
#[derive(Clone, Debug, CandidType, Deserialize)]
struct ContentChunk {
    content: RcBytes,
    start_byte: u64,
    end_byte: u64,
    sha256: [u8; 32],
}

#[derive(Default, Clone, Debug, CandidType, Deserialize)]
struct Asset {
    content_type: String,
    encodings: HashMap<String, AssetEncoding>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct EncodedAsset {
    content: RcBytes,
    content_type: String,
    content_encoding: String,
    total_length: Nat,
    sha256: Option<ByteBuf>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct AssetDetails {
    key: String,
    content_type: String,
    encodings: Vec<AssetEncodingDetails>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct AssetEncodingDetails {
    content_encoding: String,
    sha256: Option<ByteBuf>,
    length: Nat,
    modified: Timestamp,
}

struct Chunk {
    batch_id: BatchId,
    content: RcBytes,
    sha256: [u8; 32],
}

struct Batch {
    expires_at: Timestamp,
}

type Timestamp = Int;
type BatchId = Nat;
type ChunkId = Nat;
type Key = String;

// IDL Types

#[derive(Clone, Debug, CandidType, Deserialize)]
struct CreateAssetArguments {
    key: Key,
    content_type: String,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct SetAssetContentArguments {
    key: Key,
    content_encoding: String,
    chunk_ids: Vec<ChunkId>,
    sha256: Option<ByteBuf>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct UnsetAssetContentArguments {
    key: Key,
    content_encoding: String,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct DeleteAssetArguments {
    key: Key,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct ClearArguments {}

#[derive(Clone, Debug, CandidType, Deserialize)]
enum BatchOperation {
    CreateAsset(CreateAssetArguments),
    SetAssetContent(SetAssetContentArguments),
    UnsetAssetContent(UnsetAssetContentArguments),
    DeleteAsset(DeleteAssetArguments),
    Clear(ClearArguments),
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct CommitBatchArguments {
    batch_id: BatchId,
    operations: Vec<BatchOperation>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct StoreArg {
    key: Key,
    content_type: String,
    content_encoding: String,
    content: ByteBuf,
    sha256: Option<ByteBuf>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct GetArg {
    key: Key,
    accept_encodings: Vec<String>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct GetChunksInfoArg {
    key: Key,
    content_encoding: String,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct GetChunkArg {
    key: Key,
    content_encoding: String,
    index: Nat,
    sha256: Option<ByteBuf>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct GetChunkResponse {
    content: RcBytes,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct CreateBatchResponse {
    batch_id: BatchId,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct CreateChunkArg {
    batch_id: BatchId,
    content: ByteBuf,
    sha256: Option<ByteBuf>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct CreateChunkResponse {
    chunk_id: ChunkId,
}
// HTTP interface

type HeaderField = (String, String);

#[derive(Clone, Debug, CandidType, Deserialize)]
struct HttpRequest {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: ByteBuf,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct ChunkInfo {
    chunk_id: ChunkId,
    chunk_length: u64,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct ChunksInfoReponse {
    total_length: u64,
    chunks: Vec<ChunkInfo>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct HttpResponse {
    status_code: u16,
    headers: Vec<HeaderField>,
    body: RcBytes,
    streaming_strategy: Option<StreamingStrategy>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct StreamingCallbackToken {
    key: String,
    content_encoding: String,
    index: Nat,
    // We don't care about the sha, we just want to be backward compatible.
    sha256: Option<ByteBuf>,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
enum StreamingStrategy {
    Callback {
        callback: Func,
        token: StreamingCallbackToken,
    },
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct StreamingCallbackHttpResponse {
    body: RcBytes,
    token: Option<StreamingCallbackToken>,
    chunk_tree: String,
}

#[update]
fn authorize(other: Principal) {
    let caller = caller();
    STATE.with(|s| {
        let caller_autorized = s.authorized.borrow().iter().any(|p| *p == caller);
        if caller_autorized {
            s.authorized.borrow_mut().push(other);
        }
    })
}

#[query]
fn retrieve(key: Key) -> RcBytes {
    STATE.with(|s| {
        let assets = s.assets.borrow();
        let asset = assets.get(&key).unwrap_or_else(|| trap("asset not found"));
        let id_enc = asset
            .encodings
            .get("identity")
            .unwrap_or_else(|| trap("no identity encoding"));
        if id_enc.content_chunks.len() > 1 {
            trap("Asset too large. Use get() and get_chunk() instead.");
        }
        id_enc.content_chunks[0].content.clone()
    })
}

#[update(guard = "is_authorized")]
fn store(arg: StoreArg) {
    STATE.with(move |s| {
        let mut assets = s.assets.borrow_mut();
        let asset = assets.entry(arg.key.clone()).or_default();
        asset.content_type = arg.content_type;

        let hash = hash_bytes(&arg.content);
        if let Some(provided_hash) = arg.sha256 {
            if hash != provided_hash.as_ref() {
                trap("sha256 mismatch");
            }
        }

        let encoding = asset.encodings.entry(arg.content_encoding).or_default();
        encoding.total_length = arg.content.len();
        encoding.content_chunks = vec![
            ContentChunk {
                content: RcBytes::from(arg.content),
                start_byte: 0,
                end_byte: (encoding.total_length - 1) as u64,
                sha256: hash
            }
        ];
        encoding.modified = Int::from(time() as u64);
        encoding.sha256 = hash;

        on_asset_change(&arg.key, asset);
    });
}

#[update(guard = "is_authorized")]
fn create_batch() -> CreateBatchResponse {
    STATE.with(|s| {
        let batch_id = s.next_batch_id.borrow().clone();
        *s.next_batch_id.borrow_mut() += 1;

        let now = time() as u64;

        let mut batches = s.batches.borrow_mut();
        batches.insert(
            batch_id.clone(),
            Batch {
                expires_at: Int::from(now + BATCH_EXPIRY_NANOS),
            },
        );
        s.chunks.borrow_mut().retain(|_, c| {
            batches
                .get(&c.batch_id)
                .map(|b| b.expires_at > now)
                .unwrap_or(false)
        });
        batches.retain(|_, b| b.expires_at > now);

        CreateBatchResponse { batch_id }
    })
}

#[update(guard = "is_authorized")]
fn create_chunk(arg: CreateChunkArg) -> CreateChunkResponse {
    STATE.with(|s| {
        let mut batches = s.batches.borrow_mut();
        let now = time() as u64;
        let mut batch = batches
            .get_mut(&arg.batch_id)
            .unwrap_or_else(|| trap("batch not found"));
        batch.expires_at = Int::from(now + BATCH_EXPIRY_NANOS);

        let chunk_id = s.next_chunk_id.borrow().clone();
        *s.next_chunk_id.borrow_mut() += 1;

        let sha256: [u8; 32] = match arg.sha256 {
            Some(bytes) => bytes
                .into_vec()
                .try_into()
                .unwrap_or_else(|_| trap("invalid SHA-256")),
            None => {
                hash_bytes(&arg.content)
            }
        };

        s.chunks.borrow_mut().insert(
            chunk_id.clone(),
            Chunk {
                batch_id: arg.batch_id,
                content: RcBytes::from(arg.content),
                sha256,
            },
        );

        CreateChunkResponse { chunk_id }
    })
}

#[update(guard = "is_authorized")]
fn create_asset(arg: CreateAssetArguments) {
    do_create_asset(arg);
}

#[update(guard = "is_authorized")]
fn set_asset_content(arg: SetAssetContentArguments) {
    do_set_asset_content(arg);
}

#[update(guard = "is_authorized")]
fn unset_asset_content(arg: UnsetAssetContentArguments) {
    do_unset_asset_content(arg);
}

#[update(guard = "is_authorized")]
fn delete_content(arg: DeleteAssetArguments) {
    do_delete_asset(arg);
}

#[update(guard = "is_authorized")]
fn clear() {
    do_clear();
}

#[update(guard = "is_authorized")]
fn commit_batch(arg: CommitBatchArguments) {
    let batch_id = arg.batch_id;
    for op in arg.operations {
        match op {
            BatchOperation::CreateAsset(arg) => do_create_asset(arg),
            BatchOperation::SetAssetContent(arg) => do_set_asset_content(arg),
            BatchOperation::UnsetAssetContent(arg) => do_unset_asset_content(arg),
            BatchOperation::DeleteAsset(arg) => do_delete_asset(arg),
            BatchOperation::Clear(_) => do_clear(),
        }
    }
    STATE.with(|s| {
        s.batches.borrow_mut().remove(&batch_id);
    })
}

#[query]
fn get(arg: GetArg) -> EncodedAsset {
    STATE.with(|s| {
        let assets = s.assets.borrow();
        let asset = assets.get(&arg.key).unwrap_or_else(|| {
            trap("asset not found");
        });

        for enc in arg.accept_encodings.iter() {
            if let Some(asset_enc) = asset.encodings.get(enc) {
                return EncodedAsset {
                    content: asset_enc.content_chunks[0].content.clone(),
                    content_type: asset.content_type.clone(),
                    content_encoding: enc.clone(),
                    total_length: Nat::from(asset_enc.total_length as u64),
                    sha256: Some(ByteBuf::from(asset_enc.sha256)),
                };
            }
        }
        trap("no such encoding");
    })
}

#[query]
fn get_chunks_info(arg: GetChunksInfoArg) -> ChunksInfoReponse {
    STATE.with(|s| {
        let assets = s.assets.borrow();
        let asset = assets.get(&arg.key).unwrap_or_else(|| {
            trap("asset not found");
        });

        let mut result = ChunksInfoReponse {
            total_length: 0,
            chunks: vec![],
        };

        let enc = arg.content_encoding;
        if let Some(asset_enc) = asset.encodings.get(&enc) {
            for (i, chunk) in asset_enc.content_chunks.iter().enumerate() {
                let chunk_length = chunk.content.len() as u64;
                result.total_length += chunk_length;
                result.chunks.push(ChunkInfo {
                    chunk_id: Nat::from(i),
                    chunk_length,
                });
            }
        }
        result
    })
}

#[query]
fn get_chunk(arg: GetChunkArg) -> GetChunkResponse {
    STATE.with(|s| {
        let assets = s.assets.borrow();
        let asset = assets
            .get(&arg.key)
            .unwrap_or_else(|| trap("asset not found"));

        let enc = asset
            .encodings
            .get(&arg.content_encoding)
            .unwrap_or_else(|| trap("no such encoding"));

        if let Some(expected_hash) = arg.sha256 {
            if expected_hash != enc.sha256 {
                trap("sha256 mismatch")
            }
        }
        if arg.index >= enc.content_chunks.len() {
            trap("chunk index out of bounds");
        }
        let index: usize = arg.index.0.to_usize().unwrap();

        GetChunkResponse {
            content: enc.content_chunks[index].content.clone(),
        }
    })
}

#[query]
fn list() -> Vec<AssetDetails> {
    STATE.with(|s| {
        s.assets
            .borrow()
            .iter()
            .map(|(key, asset)| {
                let mut encodings: Vec<_> = asset
                    .encodings
                    .iter()
                    .map(|(enc_name, enc)| AssetEncodingDetails {
                        content_encoding: enc_name.clone(),
                        sha256: Some(ByteBuf::from(enc.sha256)),
                        length: Nat::from(enc.total_length),
                        modified: enc.modified.clone(),
                    })
                    .collect();
                encodings.sort_by(|l, r| l.content_encoding.cmp(&r.content_encoding));

                AssetDetails {
                    key: key.clone(),
                    content_type: asset.content_type.clone(),
                    encodings,
                }
            })
            .collect::<Vec<_>>()
    })
}

fn create_token(
    _asset: &Asset,
    enc_name: &str,
    enc: &AssetEncoding,
    key: &str,
    chunk_index: usize,
) -> Option<StreamingCallbackToken> {
    if chunk_index + 1 >= enc.content_chunks.len() {
        None
    } else {
        Some(StreamingCallbackToken {
            key: key.to_string(),
            content_encoding: enc_name.to_string(),
            index: Nat::from(chunk_index + 1),
            sha256: Some(ByteBuf::from(enc.sha256)),
        })
    }
}

fn create_strategy(
    asset: &Asset,
    enc_name: &str,
    enc: &AssetEncoding,
    key: &str,
    chunk_index: usize,
) -> Option<StreamingStrategy> {
    create_token(asset, enc_name, enc, key, chunk_index).map(|token| StreamingStrategy::Callback {
        callback: ic_cdk::export::candid::Func {
            method: "http_request_streaming_callback".to_string(),
            principal: ic_cdk::id(),
        },
        token,
    })
}

fn build_200(
    asset: &Asset,
    enc_name: &str,
    enc: &AssetEncoding,
    key: &str,
    chunk_index: usize,
    certificate_header: Option<HeaderField>,
) -> HttpResponse {
    let mut headers = vec![("Content-Type".to_string(), asset.content_type.to_string())];
    if enc_name != "identity" {
        headers.push(("Content-Encoding".to_string(), enc_name.to_string()));
    }
    if let Some(head) = certificate_header {
        headers.push(head);
    }

    let streaming_strategy = create_strategy(asset, enc_name, enc, key, chunk_index);

    HttpResponse {
        status_code: 200,
        headers,
        body: enc.content_chunks[chunk_index].content.clone(),
        streaming_strategy,
    }
}

fn build_206(
    asset: &Asset,
    enc_name: &str,
    enc: &AssetEncoding,
    key: &str,
    range: ContentRange,
    certificate_header: Option<HeaderField>,
) -> HttpResponse {
    let mut headers = vec![("Content-Type".to_string(), asset.content_type.to_string())];
    if enc_name != "identity" {
        headers.push(("Content-Encoding".to_string(), enc_name.to_string()));
    }
    if let Some(head) = certificate_header {
        headers.push(head);
    }
    headers.push(("Content-Range".to_string(), format!("bytes {}-{}/{}", range.start_byte, range.end_byte, range.total)));
    headers.push(("Accept-Ranges".to_string(), "bytes".to_string()));

    let streaming_strategy = create_strategy(asset, enc_name, enc, key, range.index);

    HttpResponse {
        status_code: 206,
        headers,
        body: enc.content_chunks[range.index].content.clone(),
        streaming_strategy,
    }
}

fn build_404(certificate_header: HeaderField) -> HttpResponse {
    HttpResponse {
        status_code: 404,
        headers: vec![certificate_header],
        body: RcBytes::from(ByteBuf::from("not found")),
        streaming_strategy: None,
    }
}

fn get_chunk_index_by_range(range: &Option<Range>, encodings: &Vec<String>, asset: Option<&Asset>) -> ContentRange {
    match (range, asset) {
        (Some(range), Some(asset)) => {
            let enc = encodings
                .iter()
                .find(|enc_name| {
                    if let Some(enc) = asset.encodings.get(*enc_name) {
                        if enc.certified {
                            true
                        } else {
                            // Find if identity is certified, if it's not.
                            if let Some(id_enc) = asset.encodings.get("identity") {
                                id_enc.certified
                            } else {
                                false
                            }
                        }
                    } else {
                        false
                    }
                });
            match asset.encodings.get(enc.unwrap_or(&"".to_string())) {
                Some(asset) => {
                    match asset.content_chunks
                        .iter()
                        .position(|chunk| {
                            (range.start_byte - chunk.start_byte) < (chunk.content.len() as u64)
                        }) {
                            Some(index) => ContentRange {
                                start_byte: asset.content_chunks[index].start_byte,
                                end_byte: asset.content_chunks[index].start_byte + (asset.content_chunks[index].content.len() as u64) - 1,
                                index,
                                total: asset.total_length,
                            },
                            None => match asset.content_chunks.first() {
                                Some(first) => ContentRange { // FIXME
                                    start_byte: first.start_byte,
                                    end_byte: first.end_byte,
                                    index: 0,
                                    total: asset.total_length,
                                },
                                None => ContentRange { // FIXME
                                    start_byte: 0,
                                    end_byte: 0,
                                    index: 0,
                                    total: 0,
                                }
                            }
                        }
                },
                None => ContentRange { // FIXME
                    start_byte: 0,
                    end_byte: 0,
                    index: 0,
                    total: 0,
                },
            }
        },
        _ => ContentRange { // FIXME
            start_byte: 0,
            end_byte: 0,
            index: 0,
            total: 0,
        }
    }
}

fn build_http_response(path: &str, encodings: Vec<String>, range: Option<Range>) -> HttpResponse {
    STATE.with(|s| {
        let assets = s.assets.borrow();

        let mut content_range = get_chunk_index_by_range(&range, &encodings, assets.get(INDEX_FILE));
        print(format!("Found INDEX_FILE index {}", content_range.index));
        
        let index_redirect_certificate = ASSET_HASHES.with(|t| {
            let tree = t.borrow();
            if tree.get(path.as_bytes()).is_none() && tree.get(INDEX_FILE.as_bytes()).is_some() {
                let chunk_tree = get_serialized_chunk_witness(path, content_range.index);

                let absence_proof = tree.witness(path.as_bytes());
                let index_proof = tree.witness(INDEX_FILE.as_bytes());
                let combined_proof = merge_hash_trees(absence_proof, index_proof);
                Some(witness_to_header(combined_proof, chunk_tree.clone(), content_range.index))
            } else {
                None
            }
        });

        if let Some(certificate_header) = index_redirect_certificate {
            if let Some(asset) = assets.get(INDEX_FILE) {
                for enc_name in encodings.iter() {
                    if let Some(enc) = asset.encodings.get(enc_name) {
                        if enc.certified {
                            if let Some(_) = range {
                                return build_206(
                                    asset,
                                    enc_name,
                                    enc,
                                    path,
                                    content_range,
                                    Some(certificate_header),
                                );
                            } else {
                                return build_200(
                                    asset,
                                    enc_name,
                                    enc,
                                    INDEX_FILE,
                                    content_range.index,
                                    Some(certificate_header),
                                );
                            }
                        }
                    }
                }
            }
        }

        content_range = get_chunk_index_by_range(&range, &encodings, assets.get(path));
        print(format!("Found SOME index {}", content_range.index));
        let chunk_tree = get_serialized_chunk_witness(path, content_range.index);
        let certificate_header =
            ASSET_HASHES.with(|t| witness_to_header(t.borrow().witness(path.as_bytes()), chunk_tree.clone(), content_range.index));

        if let Some(asset) = assets.get(path) {
            for enc_name in encodings.iter() {
                if let Some(enc) = asset.encodings.get(enc_name) {
                    if enc.certified {
                        if let Some(_) = range {
                            return build_206(
                                asset,
                                enc_name,
                                enc,
                                path,
                                content_range,
                                Some(certificate_header),
                            );
                        } else {
                            return build_200(
                                asset,
                                enc_name,
                                enc,
                                path,
                                content_range.index,
                                Some(certificate_header),
                            );
                        }
                    } else {
                        // Find if identity is certified, if it's not.
                        if let Some(id_enc) = asset.encodings.get("identity") {
                            if id_enc.certified {
                                if let Some(_) = range {
                                    return build_206(
                                        asset,
                                        enc_name,
                                        enc,
                                        path,
                                        content_range,
                                        Some(certificate_header),
                                    );
                                } else {
                                    return build_200(
                                        asset,
                                        enc_name,
                                        enc,
                                        path,
                                        content_range.index,
                                        Some(certificate_header),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        build_404(certificate_header)
    })
}

/// An iterator-like structure that decode a URL.
struct UrlDecode<'a> {
    bytes: std::slice::Iter<'a, u8>,
}

fn convert_percent(iter: &mut std::slice::Iter<u8>) -> Option<u8> {
    let mut cloned_iter = iter.clone();
    let result = match cloned_iter.next()? {
        b'%' => b'%',
        h => {
            let h = char::from(*h).to_digit(16)?;
            let l = char::from(*cloned_iter.next()?).to_digit(16)?;
            h as u8 * 0x10 + l as u8
        }
    };
    // Update this if we make it this far, otherwise "reset" the iterator.
    *iter = cloned_iter;
    Some(result)
}

#[derive(Debug, PartialEq)]
pub enum UrlDecodeError {
    InvalidPercentEncoding,
}

impl fmt::Display for UrlDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPercentEncoding => write!(f, "invalid percent encoding"),
        }
    }
}

impl<'a> Iterator for UrlDecode<'a> {
    type Item = Result<char, UrlDecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        let b = self.bytes.next()?;
        match b {
            b'%' => Some(
                convert_percent(&mut self.bytes)
                    .map(char::from)
                    .ok_or(UrlDecodeError::InvalidPercentEncoding),
            ),
            b'+' => Some(Ok(' ')),
            x => Some(Ok(char::from(*x))),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let bytes = self.bytes.len();
        (bytes / 3, Some(bytes))
    }
}

fn url_decode(url: &str) -> Result<String, UrlDecodeError> {
    UrlDecode {
        bytes: url.as_bytes().iter(),
    }
    .collect()
}

#[derive(Debug)]
struct Range {
    start_byte: u64,
    end_byte: Option<u64>,
}

#[derive(Debug)]
struct ContentRange {
    start_byte: u64,
    end_byte: u64,
    index: usize,
    total: usize,
}

fn get_ranges(range_header_value: &str) -> Option<Vec<Range>> {
    let range_strings = range_header_value.split(",");

    range_strings
        .map(|range_string| {
            let replaced_range_string = range_string.replace("bytes=", "");
            let bytes_string = replaced_range_string
                .split("-")
                .map(|s| s.trim())
                .collect::<Vec<&str>>();

            match (bytes_string.get(0), bytes_string.get(1)) {
                (Some(start_byte_string), Some(end_byte_string)) => {
                    match (start_byte_string.parse::<u64>(), end_byte_string.parse::<u64>()) {
                        (Ok(start_byte), Ok(end_byte)) => Some(Range {
                            start_byte,
                            end_byte: Some(end_byte),
                        }),
                        (Ok(start_byte), _) => Some(Range {
                            start_byte,
                            end_byte: None,
                        }),
                        _ => None
                    }
                },
                _ => None
            }
        })
        .collect::<Option<Vec<Range>>>()
}

#[test]
fn check_get_ranges() {
    let empty = get_ranges("").unwrap_or(vec![]);
    assert_eq!(empty.len(), 0);

    let mut range = get_ranges("bytes=0-").unwrap_or_else(|| panic!("Unable to parse range"));
    assert_eq!(range[0].start_byte, 0);
    assert_eq!(range[0].end_byte, None);

    range = get_ranges("bytes=10-11").unwrap_or_else(|| panic!("Unable to parse range"));
    assert_eq!(range[0].start_byte, 10);
    assert_eq!(range[0].end_byte.unwrap_or(0), 11);

    range = get_ranges("bytes=10-11, 100-101").unwrap_or_else(|| panic!("Unable to parse range"));
    assert_eq!(range[0].start_byte, 10);
    assert_eq!(range[0].end_byte.unwrap_or(0), 11);
    assert_eq!(range[1].start_byte, 100);
    assert_eq!(range[1].end_byte.unwrap_or(0), 101);

    range = get_ranges("bytes=10-11, bytes=100-101").unwrap_or_else(|| panic!("Unable to parse range"));
    assert_eq!(range[0].start_byte, 10);
    assert_eq!(range[0].end_byte.unwrap_or(0), 11);
    assert_eq!(range[1].start_byte, 100);
    assert_eq!(range[1].end_byte.unwrap_or(0), 101);
}

#[test]
fn check_url_decode() {
    assert_eq!(
        url_decode("/%"),
        Err(UrlDecodeError::InvalidPercentEncoding)
    );
    assert_eq!(url_decode("/%%"), Ok("/%".to_string()));
    assert_eq!(url_decode("/%20a"), Ok("/ a".to_string()));
    assert_eq!(
        url_decode("/%%+a%20+%@"),
        Err(UrlDecodeError::InvalidPercentEncoding)
    );
    assert_eq!(
        url_decode("/has%percent.txt"),
        Err(UrlDecodeError::InvalidPercentEncoding)
    );
    assert_eq!(url_decode("/%e6"), Ok("/æ".to_string()));
}

#[query]
fn http_request(req: HttpRequest) -> HttpResponse {
    let mut encodings = vec![];
    let mut range_header_value = "";

    for (name, value) in req.headers.iter() {
        if name.eq_ignore_ascii_case("Accept-Encoding") {
            for v in value.split(',') {
                encodings.push(v.trim().to_string());
            }
        }
        if name.eq_ignore_ascii_case("Range") {
            range_header_value = value;
        }
    }
    
    let range = if let Some(ranges) = get_ranges(range_header_value) {
        // FIXME REMOVE
        print(format!("range_header_value {}", range_header_value));
        print(format!("Range {}-{}", ranges[0].start_byte, ranges[0].end_byte.unwrap_or(0)));
        Some(Range {
            start_byte: ranges[0].start_byte,
            end_byte: ranges[0].end_byte,
        })
    } else {
        None
    };
    encodings.push("identity".to_string());

    let path = match req.url.find('?') {
        Some(i) => &req.url[..i],
        None => &req.url[..],
    };
    match url_decode(path) {
        Ok(path) => build_http_response(&path, encodings, range),
        Err(err) => HttpResponse {
            status_code: 400,
            headers: vec![],
            body: RcBytes::from(ByteBuf::from(format!(
                "failed to decode path '{}': {}",
                path, err
            ))),
            streaming_strategy: None,
        },
    }
}

#[query]
fn http_request_streaming_callback(
    StreamingCallbackToken {
        key,
        content_encoding,
        index,
        sha256,
    }: StreamingCallbackToken,
) -> StreamingCallbackHttpResponse {
    STATE.with(|s| {
        let assets = s.assets.borrow();
        let asset = assets
            .get(&key)
            .expect("Invalid token on streaming: key not found.");
        let enc = asset
            .encodings
            .get(&content_encoding)
            .expect("Invalid token on streaming: encoding not found.");

        if let Some(expected_hash) = sha256 {
            if expected_hash != enc.sha256 {
                trap("sha256 mismatch");
            }
        }

        // MAX is good enough. This means a chunk would be above 64-bits, which is impossible...
        let chunk_index = index.0.to_usize().unwrap_or(usize::MAX);
        let chunk_tree = get_serialized_chunk_witness(&key, chunk_index);

        StreamingCallbackHttpResponse {
            body: enc.content_chunks[chunk_index].content.clone(),
            token: create_token(asset, &content_encoding, enc, &key, chunk_index),
            chunk_tree: chunk_tree.clone(),
        }
    })
}

fn do_create_asset(arg: CreateAssetArguments) {
    STATE.with(|s| {
        let mut assets = s.assets.borrow_mut();
        if let Some(asset) = assets.get(&arg.key) {
            if asset.content_type != arg.content_type {
                trap("create_asset: content type mismatch");
            }
        } else {
            assets.insert(
                arg.key,
                Asset {
                    content_type: arg.content_type,
                    encodings: HashMap::new(),
                },
            );
        }
    })
}

fn do_set_asset_content(arg: SetAssetContentArguments) {
    STATE.with(|s| {
        if arg.chunk_ids.is_empty() {
            trap("encoding must have at least one chunk");
        }

        let mut assets = s.assets.borrow_mut();
        let asset = assets
            .get_mut(&arg.key)
            .unwrap_or_else(|| trap("asset not found"));
        let now = Int::from(time() as u64);

        let mut chunks = s.chunks.borrow_mut();

        let mut content_chunks: Vec<ContentChunk> = vec![];
        let mut reduced_total: u64 = 0;
        for chunk_id in arg.chunk_ids.iter() {
            let chunk = chunks.remove(chunk_id).expect("chunk not found");
            let len = chunk.content.len() as u64;
            content_chunks.push(ContentChunk {
                content: chunk.content,
                start_byte: reduced_total.clone(),
                end_byte: reduced_total + len - 1,
                sha256: chunk.sha256,
            });
            reduced_total += len;
        }

        let sha256: [u8; 32] = match arg.sha256 {
            Some(bytes) => bytes
            .into_vec()
            .try_into()
            .unwrap_or_else(|_| trap("invalid SHA-256")),
            None => {
                set_chunks_to_tree(&arg.key, &content_chunks);
                CHUNK_HASHES.with(|t| {
                    let chunks_map = t.borrow_mut();
                    let tree = chunks_map.get(&arg.key).unwrap_or_else(|| trap("asset not found in chunks map"));
                    tree.root_hash()
                })
            }
        };

        let enc = AssetEncoding {
            modified: now,
            content_chunks,
            certified: false,
            total_length: reduced_total as usize,
            sha256,
        };
        asset.encodings.insert(arg.content_encoding, enc);

        on_asset_change(&arg.key, asset);
    })
}

fn do_unset_asset_content(arg: UnsetAssetContentArguments) {
    STATE.with(|s| {
        let mut assets = s.assets.borrow_mut();
        let asset = assets
            .get_mut(&arg.key)
            .unwrap_or_else(|| trap("asset not found"));

        if asset.encodings.remove(&arg.content_encoding).is_some() {
            on_asset_change(&arg.key, asset);
        }
    })
}

fn do_delete_asset(arg: DeleteAssetArguments) {
    STATE.with(|s| {
        let mut assets = s.assets.borrow_mut();
        assets.remove(&arg.key);
    });
    delete_asset_hash(&arg.key);
}

fn do_clear() {
    STATE.with(|s| {
        s.assets.borrow_mut().clear();
        s.batches.borrow_mut().clear();
        s.chunks.borrow_mut().clear();
        *s.next_batch_id.borrow_mut() = Nat::from(1);
        *s.next_chunk_id.borrow_mut() = Nat::from(1);
    })
}

pub fn is_authorized() -> Result<(), String> {
    STATE.with(|s| {
        s.authorized
            .borrow()
            .contains(&caller())
            .then(|| ())
            .ok_or_else(|| "Caller is not authorized".to_string())
    })
}

fn on_asset_change(key: &str, asset: &mut Asset) {
    // If the most preferred encoding is present and certified,
    // there is nothing to do.
    for enc_name in ENCODING_CERTIFICATION_ORDER.iter() {
        if let Some(enc) = asset.encodings.get(*enc_name) {
            if enc.certified {
                return;
            } else {
                break;
            }
        }
    }

    if asset.encodings.is_empty() {
        delete_asset_hash(key);
        delete_chunks(key);
        return;
    }

    // An encoding with a higher priority was added, let's certify it
    // instead.

    for enc in asset.encodings.values_mut() {
        enc.certified = false;
    }

    for enc_name in ENCODING_CERTIFICATION_ORDER.iter() {
        if let Some(enc) = asset.encodings.get_mut(*enc_name) {
            certify_asset(key.to_string(), &enc.sha256);
            enc.certified = true;
            // Run twice when saving asset (do_set_asset_content method), but not overwrited here
            set_chunks_to_tree(&key.to_string(), &enc.content_chunks);
            return;
        }
    }

    // No known encodings found. Just pick the first one. The exact
    // order is hard to predict because we use a hash map. Should
    // almost never happen anyway.
    if let Some(enc) = asset.encodings.values_mut().next() {
        certify_asset(key.to_string(), &enc.sha256);
        enc.certified = true;
        // Run twice when saving asset (do_set_asset_content method), but not overwrited here
        set_chunks_to_tree(&key.to_string(), &enc.content_chunks);
    }
}

fn certify_asset(key: Key, content_hash: &Hash) {
    ASSET_HASHES.with(|t| {
        let mut tree = t.borrow_mut();
        tree.insert(key, *content_hash);
        set_root_hash(&*tree);
    });
}

fn delete_asset_hash(key: &str) {
    ASSET_HASHES.with(|t| {
        let mut tree = t.borrow_mut();
        tree.delete(key.as_bytes());
        set_root_hash(&*tree);
    });
}

fn set_root_hash(tree: &AssetHashes) {
    use ic_certified_map::labeled_hash;
    let full_tree_hash = labeled_hash(b"http_assets", &tree.root_hash());
    set_certified_data(&full_tree_hash);
}

fn witness_to_header(witness: HashTree, chunk_serialized_tree: String, chunk_index: usize) -> HeaderField {
    use ic_certified_map::labeled;

    let hash_tree = labeled(b"http_assets", witness);
    let tree = serialize_tree(hash_tree);
    let certificate = data_certificate().unwrap_or_else(|| trap("no data certificate available"));

    (
        "IC-Certificate".to_string(),
        String::from("certificate=:")
            + &base64::encode(&certificate)
            + ":, tree=:"
            + &tree
            + ":, chunk_tree=:"
            + &chunk_serialized_tree
            + ":, chunk_index=:"
            + &chunk_index.to_string()
            + ":",
    )
}

fn get_serialized_chunk_witness(key: &str, index: usize) -> String {
    CHUNK_HASHES.with(|t| {
        let chunks_map = t.borrow();
        let tree = chunks_map.get(key).unwrap_or_else(|| trap("asset not found in chunks map"));

        if tree.get(index.to_string().as_bytes()).is_some() {
            let witness = tree.witness(index.to_string().as_bytes());
            serialize_tree(witness)
        } else {
            String::new()
        }
    })
}

fn set_chunks_to_tree(key: &String, content_chunks: &Vec<ContentChunk>) {
    CHUNK_HASHES.with(|t| {
        let mut chunks_map = t.borrow_mut();

        let tree = chunks_map.entry(key.clone()).or_insert(RbTree::new());

        for (i, chunk) in content_chunks.iter().enumerate() {
            if !tree.get(i.to_string().as_bytes()).is_some() {
                tree.insert(i.to_string(), chunk.sha256);
            }
        }
    });
}

fn delete_chunks(key: &str) {
    CHUNK_HASHES.with(|t| (t.borrow_mut().remove(key)));
}

fn serialize_tree(tree: HashTree) -> String {
    let mut serializer = serde_cbor::ser::Serializer::new(vec![]);
    serializer.self_describe().unwrap();
    tree.serialize(&mut serializer).unwrap();
    base64::encode(&serializer.into_inner())
}

fn merge_hash_trees<'a>(lhs: HashTree<'a>, rhs: HashTree<'a>) -> HashTree<'a> {
    use HashTree::{Empty, Fork, Labeled, Leaf, Pruned};

    match (lhs, rhs) {
        (Pruned(l), Pruned(r)) => {
            if l != r {
                trap("merge_hash_trees: inconsistent hashes");
            }
            Pruned(l)
        }
        (Pruned(_), r) => r,
        (l, Pruned(_)) => l,
        (Fork(l), Fork(r)) => Fork(Box::new((
            merge_hash_trees(l.0, r.0),
            merge_hash_trees(l.1, r.1),
        ))),
        (Labeled(l_label, l), Labeled(r_label, r)) => {
            if l_label != r_label {
                trap("merge_hash_trees: inconsistent hash tree labels");
            }
            Labeled(l_label, Box::new(merge_hash_trees(*l, *r)))
        }
        (Empty, Empty) => Empty,
        (Leaf(l), Leaf(r)) => {
            if l != r {
                trap("merge_hash_trees: inconsistent leaves");
            }
            Leaf(l)
        }
        (_l, _r) => {
            trap("merge_hash_trees: inconsistent tree structure");
        }
    }
}

fn hash_bytes(bytes: &[u8]) -> Hash {
    let mut hash = sha2::Sha256::new();
    hash.update(bytes);
    hash.finalize().into()
}

pub fn init() {
    do_clear();
    STATE.with(|s| s.authorized.borrow_mut().push(caller()));
}

pub fn pre_upgrade() -> StableState {
    STATE.with(|s| StableState {
        authorized: s.authorized.take(),
        stable_assets: s.assets.take(),
    })
}

pub fn post_upgrade(stable_state: StableState) {
    do_clear();
    STATE.with(|s| {
        s.authorized.replace(stable_state.authorized);
        s.assets.replace(stable_state.stable_assets);

        for (asset_name, asset) in s.assets.borrow_mut().iter_mut() {
            for enc in asset.encodings.values_mut() {
                enc.certified = false;
            }
            on_asset_change(asset_name, asset);
        }
    });
}
