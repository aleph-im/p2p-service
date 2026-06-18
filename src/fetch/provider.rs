//! Provider side of the `/aleph/fetch/1.0.0` protocol.
//!
//! Inbound fetch requests are resolved from local sources in this order:
//!
//! 1. **Filesystem** (`content_dir`): pyaleph stores content as flat files
//!    named by their item hash directly in its storage folder. If the file
//!    exists, is a regular file and does not exceed `max_size_bytes`, it is
//!    streamed back to the requester.
//!
//! 2. **Local Kubo** (`ipfs_api_url`): if the item hash parses as a CID, the
//!    local Kubo RPC is queried with `offline=true` on every call. This
//!    parameter instructs Kubo to return an error instead of fetching the
//!    block from the public IPFS network. Only blocks already resident in the
//!    local blockstore are served. An error from Kubo (non-2xx, or the size
//!    field missing) falls through to not-found.
//!
//! 3. **Not found**: the requester falls through to its own IPFS path. This
//!    is the intended behavior for content that is not locally available.
//!
//! Serving is disabled when both `content_dir` and `ipfs_api_url` are `None`;
//! every request is then answered with `found: false`.
//!
//! # Kubo locality guarantee
//!
//! The non-negotiable invariant is that an inbound fetch request must NEVER
//! cause the local Kubo daemon to pull blocks off the public IPFS network.
//!
//! Both Kubo RPC calls in this module include `&offline=true`:
//!   - `POST /api/v0/files/stat?arg=/ipfs/{cid}&offline=true` - checks the
//!     size of the content. Returns an error when the block is not local.
//!   - `POST /api/v0/cat?arg={cid}&offline=true` - streams the content bytes.
//!
//! The `offline=true` flag was introduced in Kubo 0.12 (released 2021-04-27)
//! and is supported on all modern Kubo releases used in aleph.im deployments.
//! When `offline=true` is set on `/api/v0/files/stat`, Kubo returns HTTP 500
//! with a JSON error body if any referenced block is not in the local
//! blockstore, rather than attempting a network fetch. We gate the `cat` call
//! behind a successful `files/stat` response to ensure we never stream
//! partially-local content. Only if `files/stat` succeeds (2xx + a `Size`
//! field) do we proceed to `cat` (also with `offline=true`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use libp2p::PeerId;
use log::{debug, warn};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, Semaphore};
use tokio::time::{timeout, Instant};
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tokio_util::io::ReaderStream;

use crate::fetch::is_valid_item_hash;
use crate::fetch::wire::{read_frame, write_frame, FetchRequest, FetchResponseHeader};
use crate::metrics::Metrics;

const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-write timeout towards the remote peer. Without it, a requester that
/// stops reading stalls write_all forever once the mux send window fills,
/// holding its serving slot indefinitely.
const SERVE_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const BACKEND_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Inactivity timeout on backend reads. A total request timeout would cap
/// large transfers, since the body relay is paced by the remote peer.
const BACKEND_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Read buffer capacity for filesystem streaming.
const FS_STREAM_CHUNK: usize = 64 * 1024;

/// Wraps a write towards the remote peer in SERVE_WRITE_TIMEOUT.
async fn timed<T>(
    fut: impl std::future::Future<Output = std::io::Result<T>>,
) -> std::io::Result<T> {
    timeout(SERVE_WRITE_TIMEOUT, fut)
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "peer write timeout"))?
}

#[derive(Clone)]
pub struct ProviderSettings {
    /// `None` disables filesystem serving.
    pub content_dir: Option<PathBuf>,
    /// `None` disables Kubo/IPFS serving.
    pub ipfs_api_url: Option<String>,
    pub max_size_bytes: u64,
    pub max_inbound_streams: usize,
    pub max_inbound_streams_per_peer: usize,
    /// 0 disables rate limiting.
    pub serve_bytes_per_sec: u64,
}

/// Minimal token bucket over served bytes. Capacity is one second of budget.
pub struct ByteRateLimiter {
    bytes_per_sec: u64,
    available: f64,
    last_refill: Instant,
}

impl ByteRateLimiter {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            bytes_per_sec,
            available: bytes_per_sec as f64,
            last_refill: Instant::now(),
        }
    }

    /// Waits until `n` bytes of budget are available, then consumes them.
    pub async fn acquire(&mut self, n: usize) {
        if self.bytes_per_sec == 0 {
            return;
        }
        // The bucket capacity is one second of budget; clamp the cost so a
        // chunk larger than the capacity throttles instead of spinning forever.
        let n = n.min(self.bytes_per_sec as usize);
        loop {
            let elapsed = self.last_refill.elapsed().as_secs_f64();
            self.available = (self.available + elapsed * self.bytes_per_sec as f64)
                .min(self.bytes_per_sec as f64);
            self.last_refill = Instant::now();
            if self.available >= n as f64 {
                self.available -= n as f64;
                return;
            }
            let deficit = n as f64 - self.available;
            tokio::time::sleep(Duration::from_secs_f64(deficit / self.bytes_per_sec as f64)).await;
        }
    }
}

/// Resolved content: a known byte size and a stream of raw bytes.
type ContentStream = std::pin::Pin<Box<dyn futures::Stream<Item = std::io::Result<Bytes>> + Send>>;

/// Try to resolve the content from the local filesystem.
///
/// Returns `Some((size, stream))` if the file exists, is a regular file, and
/// does not exceed `max_size_bytes`. Returns `None` when the file is not
/// found. Returns `Err` when the file exists but exceeds the size limit
/// (caller should record a refused metric).
async fn resolve_from_fs(
    content_dir: &Path,
    item_hash: &str,
    max_size_bytes: u64,
) -> std::io::Result<Option<(u64, ContentStream)>> {
    let path = content_dir.join(item_hash);
    let meta = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !meta.is_file() {
        return Ok(None);
    }
    let size = meta.len();
    if size > max_size_bytes {
        // Signal "exists but too large" with a specific error kind.
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!(
                "content file {} is {} bytes, exceeds max {}",
                item_hash, size, max_size_bytes
            ),
        ));
    }
    let file = tokio::fs::File::open(&path).await?;
    let stream: ContentStream = Box::pin(ReaderStream::with_capacity(file, FS_STREAM_CHUNK));
    Ok(Some((size, stream)))
}

/// Query Kubo for file size via `files/stat` with `offline=true`.
///
/// Returns the `Size` field on success, or `None` when Kubo returns an error
/// (non-2xx) or the block is not local.
async fn kubo_stat_local(http: &reqwest::Client, ipfs_api_url: &str, cid_str: &str) -> Option<u64> {
    let url = format!(
        "{}/api/v0/files/stat?arg=/ipfs/{}&offline=true",
        ipfs_api_url.trim_end_matches('/'),
        cid_str
    );
    let resp = http.post(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    json["Size"].as_u64()
}

/// Stream content from Kubo via `cat` with `offline=true`.
async fn kubo_cat_local(
    http: &reqwest::Client,
    ipfs_api_url: &str,
    cid_str: &str,
) -> std::io::Result<ContentStream> {
    let url = format!(
        "{}/api/v0/cat?arg={}&offline=true",
        ipfs_api_url.trim_end_matches('/'),
        cid_str
    );
    let resp = http
        .post(&url)
        .send()
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, e))?;
    if !resp.status().is_success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Kubo cat returned {}", resp.status()),
        ));
    }
    let stream: ContentStream = Box::pin(
        resp.bytes_stream()
            .map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, e))),
    );
    Ok(stream)
}

/// Resolve content from Kubo (local-only).
///
/// Returns `Some((size, stream))` if the CID is locally available in Kubo and
/// does not exceed `max_size_bytes`. Returns `None` when the block is not
/// local, Kubo is unavailable, or the size exceeds the limit.
async fn resolve_from_ipfs(
    http: &reqwest::Client,
    ipfs_api_url: &str,
    item_hash: &str,
    max_size_bytes: u64,
) -> Option<(u64, ContentStream)> {
    // Only proceed if the hash parses as a CID.
    if item_hash.parse::<cid::Cid>().is_err() {
        return None;
    }

    // Query Kubo for the local size. `offline=true` ensures this errors
    // instead of fetching from the network when the block is not local.
    let size = kubo_stat_local(http, ipfs_api_url, item_hash).await?;
    if size > max_size_bytes {
        debug!(
            "fetch: IPFS CID {} is {} bytes, exceeds max {}; refusing",
            item_hash, size, max_size_bytes
        );
        return None;
    }

    // Size is acceptable; stream the content. Also with `offline=true` to
    // ensure locality throughout.
    match kubo_cat_local(http, ipfs_api_url, item_hash).await {
        Ok(stream) => Some((size, stream)),
        Err(e) => {
            debug!("fetch: Kubo cat failed for {}: {}", item_hash, e);
            None
        }
    }
}

pub async fn run(
    mut incoming: libp2p_stream::IncomingStreams,
    settings: ProviderSettings,
    metrics: Metrics,
) {
    let http = reqwest::Client::builder()
        .connect_timeout(BACKEND_CONNECT_TIMEOUT)
        .read_timeout(BACKEND_READ_TIMEOUT)
        .build()
        .expect("reqwest client construction cannot fail with static settings");
    let global_slots = Arc::new(Semaphore::new(settings.max_inbound_streams));
    let per_peer: Arc<Mutex<HashMap<PeerId, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let limiter = Arc::new(Mutex::new(ByteRateLimiter::new(
        settings.serve_bytes_per_sec,
    )));

    while let Some((peer, stream)) = incoming.next().await {
        let Ok(permit) = global_slots.clone().try_acquire_owned() else {
            metrics.increment_event("fetch_serve_rejected_busy");
            drop(stream);
            continue;
        };
        {
            let mut counts = per_peer.lock().await;
            let count = counts.entry(peer).or_insert(0);
            if *count >= settings.max_inbound_streams_per_peer {
                metrics.increment_event("fetch_serve_rejected_busy");
                drop(stream);
                continue;
            }
            *count += 1;
        }
        let settings = settings.clone();
        let http = http.clone();
        let metrics = metrics.clone();
        let per_peer = per_peer.clone();
        let limiter = limiter.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let result =
                handle_inbound(peer, stream.compat(), &http, &settings, &limiter, &metrics).await;
            if let Err(e) = result {
                debug!("fetch: inbound stream from {} failed: {}", peer, e);
            }
            let mut counts = per_peer.lock().await;
            if let Some(count) = counts.get_mut(&peer) {
                *count -= 1;
                if *count == 0 {
                    counts.remove(&peer);
                }
            }
        });
    }
    warn!("fetch provider: incoming stream source closed");
}

pub async fn handle_inbound<S>(
    peer: PeerId,
    mut stream: S,
    http: &reqwest::Client,
    settings: &ProviderSettings,
    limiter: &Arc<Mutex<ByteRateLimiter>>,
    metrics: &Metrics,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request: FetchRequest = timeout(REQUEST_READ_TIMEOUT, read_frame(&mut stream))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "request read timeout"))??;

    let not_found = FetchResponseHeader {
        found: false,
        size: 0,
    };

    // Validate hash: guards both protocol correctness and path traversal.
    // is_valid_item_hash allows only ASCII alphanumeric chars, so a hash
    // accepted here can be safely appended to a filesystem path.
    if !is_valid_item_hash(&request.item_hash) || request.offset != 0 {
        metrics.increment_event("fetch_serve_not_found");
        return timed(write_frame(&mut stream, &not_found)).await;
    }

    // Attempt to resolve content from local sources.
    let resolved = resolve_content(http, settings, &request.item_hash, metrics).await;

    let (size, body) = match resolved {
        Some(pair) => pair,
        None => {
            return timed(write_frame(&mut stream, &not_found)).await;
        }
    };

    timed(write_frame(
        &mut stream,
        &FetchResponseHeader { found: true, size },
    ))
    .await?;

    let mut sent: u64 = 0;
    futures::pin_mut!(body);
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, e))?;
        if sent + chunk.len() as u64 > size {
            // Source lied about the size; cut the stream so the peer notices
            // the mismatch against the announced size.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "source served more bytes than announced",
            ));
        }
        limiter.lock().await.acquire(chunk.len()).await;
        timed(stream.write_all(&chunk)).await?;
        sent += chunk.len() as u64;
        metrics.fetch_bytes_served_total.inc_by(chunk.len() as u64);
    }
    if sent != size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "source served fewer bytes than announced",
        ));
    }
    timed(stream.flush()).await?;
    metrics.fetch_served_total.inc();
    debug!(
        "fetch: served {} ({} bytes) to {}",
        request.item_hash, sent, peer
    );
    Ok(())
}

/// Resolve content from local sources (filesystem then Kubo).
///
/// Returns `Some((size, stream))` on success, `None` on miss.
/// Increments the appropriate metric on misses.
async fn resolve_content(
    http: &reqwest::Client,
    settings: &ProviderSettings,
    item_hash: &str,
    metrics: &Metrics,
) -> Option<(u64, ContentStream)> {
    // 1. Filesystem
    if let Some(content_dir) = &settings.content_dir {
        match resolve_from_fs(content_dir, item_hash, settings.max_size_bytes).await {
            Ok(Some(pair)) => return Some(pair),
            Ok(None) => {} // not on FS; fall through
            Err(e) if e.kind() == std::io::ErrorKind::FileTooLarge => {
                // File present but oversized; treat as refused.
                debug!(
                    "fetch: {} exceeds max_size_bytes on filesystem, refusing",
                    item_hash
                );
                metrics.increment_event("fetch_serve_not_found");
                return None;
            }
            Err(e) => {
                debug!("fetch: filesystem read error for {}: {}", item_hash, e);
                // Fall through to IPFS.
            }
        }
    }

    // 2. Local Kubo
    if let Some(ipfs_api_url) = &settings.ipfs_api_url {
        if let Some(pair) =
            resolve_from_ipfs(http, ipfs_api_url, item_hash, settings.max_size_bytes).await
        {
            return Some(pair);
        }
    }

    // 3. Not found
    metrics.increment_event("fetch_serve_not_found");
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test(start_paused = true)]
    async fn rate_limiter_clamps_costs_larger_than_capacity() {
        // A cost above one second of budget must throttle, not spin forever.
        let mut limiter = ByteRateLimiter::new(1024);
        limiter.acquire(64 * 1024).await;
    }

    fn settings_with(
        content_dir: Option<PathBuf>,
        ipfs_api_url: Option<String>,
    ) -> ProviderSettings {
        ProviderSettings {
            content_dir,
            ipfs_api_url,
            max_size_bytes: 1024 * 1024,
            max_inbound_streams: 4,
            max_inbound_streams_per_peer: 2,
            serve_bytes_per_sec: 0,
        }
    }

    fn disabled_settings() -> ProviderSettings {
        settings_with(None, None)
    }

    async fn run_handler(
        settings: ProviderSettings,
        request: FetchRequest,
    ) -> (FetchResponseHeader, Vec<u8>) {
        let (provider_io, mut requester_io) = tokio::io::duplex(1024 * 1024);
        let http = reqwest::Client::new();
        let limiter = Arc::new(Mutex::new(ByteRateLimiter::new(
            settings.serve_bytes_per_sec,
        )));
        let metrics = Metrics::new();
        let handle = tokio::spawn(async move {
            let _ = handle_inbound(
                PeerId::random(),
                provider_io,
                &http,
                &settings,
                &limiter,
                &metrics,
            )
            .await;
        });
        write_frame(&mut requester_io, &request).await.unwrap();
        let header: FetchResponseHeader = read_frame(&mut requester_io).await.unwrap();
        let mut body = Vec::new();
        if header.found {
            let mut chunk = vec![0u8; header.size as usize];
            tokio::io::AsyncReadExt::read_exact(&mut requester_io, &mut chunk)
                .await
                .unwrap();
            body = chunk;
        }
        handle.await.unwrap();
        (header, body)
    }

    // --- Filesystem tests ---

    #[tokio::test]
    async fn fs_serves_known_content() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "a".repeat(64);
        let content = b"hello fetch protocol".to_vec();
        tokio::fs::write(dir.path().join(&hash), &content)
            .await
            .unwrap();
        let settings = settings_with(Some(dir.path().to_path_buf()), None);
        let (header, body) = run_handler(
            settings,
            FetchRequest {
                item_hash: hash,
                offset: 0,
            },
        )
        .await;
        assert!(header.found);
        assert_eq!(header.size, content.len() as u64);
        assert_eq!(body, content);
    }

    #[tokio::test]
    async fn fs_unknown_hash_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings_with(Some(dir.path().to_path_buf()), None);
        let (header, _) = run_handler(
            settings,
            FetchRequest {
                item_hash: "b".repeat(64),
                offset: 0,
            },
        )
        .await;
        assert!(!header.found);
    }

    #[tokio::test]
    async fn fs_oversized_refused() {
        let dir = tempfile::tempdir().unwrap();
        let hash = "a".repeat(64);
        let content = vec![0u8; 4096];
        tokio::fs::write(dir.path().join(&hash), &content)
            .await
            .unwrap();
        let mut settings = settings_with(Some(dir.path().to_path_buf()), None);
        settings.max_size_bytes = 1024;
        let (header, _) = run_handler(
            settings,
            FetchRequest {
                item_hash: hash,
                offset: 0,
            },
        )
        .await;
        assert!(!header.found);
    }

    #[tokio::test]
    async fn provider_disabled_not_found() {
        let (header, _) = run_handler(
            disabled_settings(),
            FetchRequest {
                item_hash: "a".repeat(64),
                offset: 0,
            },
        )
        .await;
        assert!(!header.found);
    }

    #[tokio::test]
    async fn invalid_hash_refused() {
        // Path traversal attempt: is_valid_item_hash must reject this.
        let dir = tempfile::tempdir().unwrap();
        let settings = settings_with(Some(dir.path().to_path_buf()), None);
        let (header, _) = run_handler(
            settings,
            FetchRequest {
                item_hash: "../../etc/passwd".to_string(),
                offset: 0,
            },
        )
        .await;
        assert!(!header.found);
    }

    // --- Mock Kubo HTTP server ---

    /// A minimal HTTP server that handles Kubo RPC requests.
    ///
    /// `files_stat_response`: if `Some(size)`, files/stat returns 200 + JSON
    /// `{"Size": size}`; if `None`, returns 500.
    /// `cat_body`: the body to return for cat requests (200 OK).
    /// Returns (addr, hit_count_arc).
    async fn mock_kubo(
        known_cid: String,
        files_stat_response: Option<u64>,
        cat_body: Vec<u8>,
        hit_counter: Arc<std::sync::Mutex<usize>>,
    ) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let known_cid = known_cid.clone();
                let files_stat_response = files_stat_response;
                let cat_body = cat_body.clone();
                let hit_counter = hit_counter.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    *hit_counter.lock().unwrap() += 1;

                    let response = if request.contains("files/stat") && request.contains(&known_cid)
                    {
                        match files_stat_response {
                            Some(size) => {
                                let body = format!(r#"{{"Size":{}}}"#, size);
                                format!(
                                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                    body.len(), body
                                ).into_bytes()
                            }
                            None => b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
                        }
                    } else if request.contains("/api/v0/cat") && request.contains(&known_cid) {
                        let mut r = format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            cat_body.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(&cat_body);
                        r
                    } else {
                        b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_vec()
                    };
                    let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, &response).await;
                });
            }
        });
        addr
    }

    // A known-good CIDv0 (empty-directory CID used by IPFS docs as a stable example).
    const KNOWN_CIDV0: &str = "QmUNLLsPACCz1vLxQVkXqqLX5R1X345qqfHbsf67hvA3Nn";

    #[tokio::test]
    async fn ipfs_serves_cid_when_local() {
        let content = b"ipfs content bytes".to_vec();
        let hit_counter = Arc::new(std::sync::Mutex::new(0usize));
        let addr = mock_kubo(
            KNOWN_CIDV0.to_string(),
            Some(content.len() as u64),
            content.clone(),
            hit_counter.clone(),
        )
        .await;
        let settings = settings_with(None, Some(format!("http://{}", addr)));
        let (header, body) = run_handler(
            settings,
            FetchRequest {
                item_hash: KNOWN_CIDV0.to_string(),
                offset: 0,
            },
        )
        .await;
        assert!(header.found, "expected found=true");
        assert_eq!(header.size, content.len() as u64);
        assert_eq!(body, content);
    }

    #[tokio::test]
    async fn ipfs_not_local_not_found() {
        let hit_counter = Arc::new(std::sync::Mutex::new(0usize));
        let addr = mock_kubo(
            KNOWN_CIDV0.to_string(),
            None, // files/stat returns 500 (not local)
            vec![],
            hit_counter.clone(),
        )
        .await;
        let settings = settings_with(None, Some(format!("http://{}", addr)));
        let (header, _) = run_handler(
            settings,
            FetchRequest {
                item_hash: KNOWN_CIDV0.to_string(),
                offset: 0,
            },
        )
        .await;
        assert!(!header.found);
    }

    #[tokio::test]
    async fn non_cid_hash_never_calls_kubo() {
        // A 64-hex hash does not parse as a CID; Kubo must never be contacted.
        let hit_counter = Arc::new(std::sync::Mutex::new(0usize));
        let addr = mock_kubo(
            "dummy".to_string(),
            Some(100),
            vec![0u8; 100],
            hit_counter.clone(),
        )
        .await;
        let settings = settings_with(None, Some(format!("http://{}", addr)));
        let (header, _) = run_handler(
            settings,
            FetchRequest {
                item_hash: "a".repeat(64),
                offset: 0,
            },
        )
        .await;
        assert!(!header.found);
        // Give any errant background tasks a moment to reach the server.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            *hit_counter.lock().unwrap(),
            0,
            "Kubo must not be contacted for a non-CID hash"
        );
    }

    #[tokio::test]
    async fn fs_precedence_over_ipfs() {
        // A file present on FS whose name parses as a CID is served from FS;
        // Kubo must not be contacted.
        let dir = tempfile::tempdir().unwrap();
        let content = b"from filesystem".to_vec();
        tokio::fs::write(dir.path().join(KNOWN_CIDV0), &content)
            .await
            .unwrap();
        let hit_counter = Arc::new(std::sync::Mutex::new(0usize));
        let addr = mock_kubo(
            KNOWN_CIDV0.to_string(),
            Some(999),
            b"from kubo".to_vec(),
            hit_counter.clone(),
        )
        .await;
        let settings = settings_with(
            Some(dir.path().to_path_buf()),
            Some(format!("http://{}", addr)),
        );
        let (header, body) = run_handler(
            settings,
            FetchRequest {
                item_hash: KNOWN_CIDV0.to_string(),
                offset: 0,
            },
        )
        .await;
        assert!(header.found);
        assert_eq!(body, content, "content must come from FS, not Kubo");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            *hit_counter.lock().unwrap(),
            0,
            "Kubo must not be contacted when FS has the content"
        );
    }
}
