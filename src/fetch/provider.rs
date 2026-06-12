//! Provider side of the `/aleph/fetch/1.0.0` protocol.
//!
//! The provider holds no storage: every inbound request is proxied to the
//! local pyaleph API (`GET {content_provider_url}/api/v0/storage/raw/{hash}`)
//! and the response body is relayed to the remote peer. Serving is disabled
//! while `provider_url` is `None`; every request is then answered with
//! `found: false` so old deployments keep working without config changes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use libp2p::PeerId;
use log::{debug, warn};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, Semaphore};
use tokio::time::{timeout, Instant};
use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::fetch::is_valid_item_hash;
use crate::fetch::wire::{read_frame, write_frame, FetchRequest, FetchResponseHeader};
use crate::metrics::Metrics;

const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);
const BACKEND_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct ProviderSettings {
    /// None disables serving (every request answered found=false).
    pub provider_url: Option<String>,
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

pub async fn run(
    mut incoming: libp2p_stream::IncomingStreams,
    settings: ProviderSettings,
    metrics: Metrics,
) {
    let http = reqwest::Client::builder()
        .timeout(BACKEND_TIMEOUT)
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

    if !is_valid_item_hash(&request.item_hash) || request.offset != 0 {
        metrics.increment_event("fetch_serve_not_found");
        return write_frame(&mut stream, &not_found).await;
    }
    let Some(base_url) = &settings.provider_url else {
        metrics.increment_event("fetch_serve_not_found");
        return write_frame(&mut stream, &not_found).await;
    };

    let url = format!(
        "{}/api/v0/storage/raw/{}",
        base_url.trim_end_matches('/'),
        request.item_hash
    );
    let response = match http.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            debug!(
                "fetch: backend request failed for {}: {}",
                request.item_hash, e
            );
            metrics.increment_event("fetch_serve_backend_error");
            metrics.fetch_serve_errors_total.inc();
            return write_frame(&mut stream, &not_found).await;
        }
    };

    let size = match response.content_length() {
        Some(size) if response.status().is_success() && size <= settings.max_size_bytes => size,
        _ => {
            metrics.increment_event(if response.status().is_success() {
                "fetch_serve_not_found"
            } else {
                "fetch_serve_backend_error"
            });
            metrics.fetch_serve_errors_total.inc();
            return write_frame(&mut stream, &not_found).await;
        }
    };

    write_frame(&mut stream, &FetchResponseHeader { found: true, size }).await?;

    let mut sent: u64 = 0;
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, e))?;
        if sent + chunk.len() as u64 > size {
            // Backend lied about Content-Length; cut the stream, the peer
            // notices the mismatch against the announced size.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "backend served more bytes than announced",
            ));
        }
        limiter.lock().await.acquire(chunk.len()).await;
        stream.write_all(&chunk).await?;
        sent += chunk.len() as u64;
        metrics.fetch_bytes_served_total.inc_by(chunk.len() as u64);
    }
    if sent != size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "backend served fewer bytes than announced",
        ));
    }
    stream.flush().await?;
    metrics.fetch_served_total.inc();
    debug!(
        "fetch: served {} ({} bytes) to {}",
        request.item_hash, sent, peer
    );
    Ok(())
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

    /// Serves HTTP requests: 200 with `content` for /api/v0/storage/raw/<known_hash>,
    /// 404 otherwise. Closes the connection after responding.
    async fn mock_backend(known_hash: String, content: Vec<u8>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let known_hash = known_hash.clone();
                let content = content.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let response = if request.contains(&known_hash) {
                        let mut r = format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            content.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(&content);
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

    fn settings(provider_url: Option<String>) -> ProviderSettings {
        ProviderSettings {
            provider_url,
            max_size_bytes: 1024 * 1024,
            max_inbound_streams: 4,
            max_inbound_streams_per_peer: 2,
            serve_bytes_per_sec: 0,
        }
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

    #[tokio::test]
    async fn serves_known_content() {
        let hash = "a".repeat(64);
        let content = b"hello fetch protocol".to_vec();
        let addr = mock_backend(hash.clone(), content.clone()).await;
        let settings = settings(Some(format!("http://{}", addr)));
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
    async fn unknown_hash_is_not_found() {
        let addr = mock_backend("a".repeat(64), b"x".to_vec()).await;
        let settings = settings(Some(format!("http://{}", addr)));
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
    async fn disabled_provider_answers_not_found() {
        let (header, _) = run_handler(
            settings(None),
            FetchRequest {
                item_hash: "a".repeat(64),
                offset: 0,
            },
        )
        .await;
        assert!(!header.found);
    }

    #[tokio::test]
    async fn oversized_content_is_refused() {
        let hash = "a".repeat(64);
        let content = vec![0u8; 4096];
        let addr = mock_backend(hash.clone(), content).await;
        let mut s = settings(Some(format!("http://{}", addr)));
        s.max_size_bytes = 1024;
        let (header, _) = run_handler(
            s,
            FetchRequest {
                item_hash: hash,
                offset: 0,
            },
        )
        .await;
        assert!(!header.found);
    }

    #[tokio::test]
    async fn invalid_hash_is_refused_without_backend_call() {
        let settings = settings(Some("http://127.0.0.1:1".to_string()));
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
}
