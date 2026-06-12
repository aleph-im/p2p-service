//! Requester side of the `/aleph/fetch/1.0.0` protocol.
//!
//! Tries candidate peers in order (gRPC caller hints first, then connected
//! peers sorted preferred/score by the caller) with a per-peer timeout on
//! every step and an attempt cap. Failed peers go into a cooldown map so the
//! next fetch skips them. The content hash is verified by pyaleph, never
//! here; this side only enforces the size limit.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use libp2p::PeerId;
use log::debug;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::time::{timeout, Instant};
use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::fetch::wire::{read_frame, write_frame, FetchRequest, FetchResponseHeader, CHUNK_SIZE};
use crate::fetch::FETCH_PROTOCOL;
use crate::metrics::Metrics;

const COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct FetchSettings {
    pub peer_timeout: Duration,
    pub max_size_bytes: u64,
    pub max_peer_attempts: usize,
}

#[derive(Debug)]
pub enum FetchError {
    /// No peer could provide the content (all candidates tried or skipped).
    NotFound,
    /// Failed after content bytes were already forwarded; not retryable.
    Aborted(String),
}

#[derive(Debug)]
pub enum FetchProgress {
    Header { size: u64 },
    Chunk(Vec<u8>),
}

/// Peers that recently failed a fetch are skipped for COOLDOWN.
/// This stands in for a gossipsub behaviour penalty, which the gossipsub
/// API does not let us inject from the outside.
#[derive(Default)]
pub struct FetchCooldowns {
    inner: Mutex<HashMap<PeerId, Instant>>,
}

impl FetchCooldowns {
    pub fn is_cooling(&self, peer: &PeerId) -> bool {
        let mut map = self.inner.lock().expect("cooldown lock poisoned");
        match map.get(peer) {
            Some(since) if since.elapsed() < COOLDOWN => true,
            Some(_) => {
                map.remove(peer);
                false
            }
            None => false,
        }
    }

    pub fn penalize(&self, peer: PeerId) {
        self.inner
            .lock()
            .expect("cooldown lock poisoned")
            .insert(peer, Instant::now());
    }
}

/// Tries candidates in order until one serves the content. Chunks are
/// forwarded through `tx` as they arrive. The caller enforces the total
/// deadline by wrapping this future in a timeout.
pub async fn fetch_from_peers(
    control: libp2p_stream::Control,
    candidates: Vec<PeerId>,
    item_hash: String,
    settings: FetchSettings,
    cooldowns: &FetchCooldowns,
    metrics: Metrics,
    tx: mpsc::Sender<FetchProgress>,
) -> Result<(), FetchError> {
    let mut attempts = 0usize;
    for peer in candidates {
        if attempts >= settings.max_peer_attempts {
            break;
        }
        if cooldowns.is_cooling(&peer) {
            continue;
        }
        attempts += 1;
        match try_peer(control.clone(), peer, &item_hash, &settings, &metrics, &tx).await {
            Ok(PeerOutcome::Served) => return Ok(()),
            Ok(PeerOutcome::NotFound) => {
                // An honest miss; no penalty.
                metrics.increment_event("fetch_peer_not_found");
            }
            Err(TryPeerError::Retryable(reason)) => {
                debug!("fetch: peer {} failed for {}: {}", peer, item_hash, reason);
                metrics.increment_event("fetch_peer_error");
                cooldowns.penalize(peer);
            }
            Err(TryPeerError::SkipNoPenalty(reason)) => {
                debug!(
                    "fetch: skipping peer {} for {}: {}",
                    peer, item_hash, reason
                );
                metrics.increment_event("fetch_peer_skipped");
            }
            Err(TryPeerError::Fatal(reason)) => {
                cooldowns.penalize(peer);
                return Err(FetchError::Aborted(reason));
            }
            Err(TryPeerError::ConsumerGone) => {
                // Our gRPC consumer went away; the serving peer did nothing
                // wrong, so it is not penalized.
                return Err(FetchError::Aborted("consumer went away".into()));
            }
        }
    }
    Err(FetchError::NotFound)
}

enum PeerOutcome {
    Served,
    NotFound,
}

enum TryPeerError {
    /// Nothing was forwarded to the caller yet; the next peer can be tried.
    Retryable(String),
    /// Nothing was forwarded and the failure was not the peer's fault
    /// (e.g. we could not even reach it); try the next peer, no cooldown.
    SkipNoPenalty(String),
    /// Content bytes were already forwarded and the peer is at fault;
    /// the fetch must abort.
    Fatal(String),
    /// Content bytes were already forwarded but our own consumer dropped
    /// the channel; the fetch must abort without penalizing the peer.
    ConsumerGone,
}

async fn try_peer(
    mut control: libp2p_stream::Control,
    peer: PeerId,
    item_hash: &str,
    settings: &FetchSettings,
    metrics: &Metrics,
    tx: &mpsc::Sender<FetchProgress>,
) -> Result<PeerOutcome, TryPeerError> {
    let retryable = TryPeerError::Retryable;

    let stream = match timeout(
        settings.peer_timeout,
        control.open_stream(peer, FETCH_PROTOCOL),
    )
    .await
    {
        Err(_) => return Err(retryable("open timeout".into())),
        Ok(Err(libp2p_stream::OpenStreamError::UnsupportedProtocol(_))) => {
            // A peer without fetch support; cool it down so transition-era fetches do
            // not burn their attempts on old nodes on every RPC.
            return Err(retryable("fetch protocol unsupported".into()));
        }
        Ok(Err(e)) => {
            // Typically a dial failure for a hinted peer we are not
            // connected to; not the peer's fault, skip without penalty.
            return Err(TryPeerError::SkipNoPenalty(format!("open failed: {}", e)));
        }
        Ok(Ok(stream)) => stream,
    };
    let mut stream = stream.compat();

    timeout(
        settings.peer_timeout,
        write_frame(
            &mut stream,
            &FetchRequest {
                item_hash: item_hash.to_string(),
                offset: 0,
            },
        ),
    )
    .await
    .map_err(|_| retryable("request write timeout".into()))?
    .map_err(|e| retryable(format!("request write failed: {}", e)))?;

    let header: FetchResponseHeader = timeout(settings.peer_timeout, read_frame(&mut stream))
        .await
        .map_err(|_| {
            metrics.increment_event("fetch_peer_timeout");
            retryable("header read timeout".into())
        })?
        .map_err(|e| retryable(format!("header read failed: {}", e)))?;

    if !header.found {
        return Ok(PeerOutcome::NotFound);
    }
    if header.size > settings.max_size_bytes {
        return Err(retryable(format!(
            "announced size {} over limit",
            header.size
        )));
    }

    // From the first forwarded byte on, failures are fatal for this RPC.
    tx.send(FetchProgress::Header { size: header.size })
        .await
        .map_err(|_| TryPeerError::ConsumerGone)?;

    let mut remaining = header.size;
    let mut buf = vec![0u8; CHUNK_SIZE];
    while remaining > 0 {
        let to_read = remaining.min(CHUNK_SIZE as u64) as usize;
        timeout(
            settings.peer_timeout,
            stream.read_exact(&mut buf[..to_read]),
        )
        .await
        .map_err(|_| {
            metrics.increment_event("fetch_peer_timeout");
            TryPeerError::Fatal("chunk read timeout".into())
        })?
        .map_err(|e| TryPeerError::Fatal(format!("chunk read failed: {}", e)))?;
        metrics.fetch_bytes_fetched_total.inc_by(to_read as u64);
        tx.send(FetchProgress::Chunk(buf[..to_read].to_vec()))
            .await
            .map_err(|_| TryPeerError::ConsumerGone)?;
        remaining -= to_read as u64;
    }
    Ok(PeerOutcome::Served)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;

    use libp2p::identity;

    use crate::p2p::network::{self, NetworkSettings};
    use crate::p2p::peerstore::PeerStore;
    use crate::subscriptions::Subscriptions;

    #[tokio::test]
    async fn penalized_peer_is_cooling() {
        let cooldowns = FetchCooldowns::default();
        let peer = PeerId::random();
        assert!(!cooldowns.is_cooling(&peer));
        cooldowns.penalize(peer);
        assert!(cooldowns.is_cooling(&peer));
        // Other peers are unaffected.
        assert!(!cooldowns.is_cooling(&PeerId::random()));
    }

    #[tokio::test(start_paused = true)]
    async fn cooldown_expires() {
        let cooldowns = FetchCooldowns::default();
        let peer = PeerId::random();
        cooldowns.penalize(peer);
        assert!(cooldowns.is_cooling(&peer));

        // Just before expiry the peer is still cooling.
        tokio::time::advance(COOLDOWN - Duration::from_secs(1)).await;
        assert!(cooldowns.is_cooling(&peer));

        // After expiry the peer is usable again and the entry is dropped.
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(!cooldowns.is_cooling(&peer));
        assert!(!cooldowns.is_cooling(&peer));
    }

    #[tokio::test]
    async fn attempt_cap_limits_tried_peers() {
        // A real Control needs a running swarm; opening a stream to a
        // disconnected random peer fails fast (no known addresses) or hits
        // the 100 ms per-peer timeout.
        let (_client, event_loop, control) = network::new(
            identity::Keypair::generate_ed25519(),
            NetworkSettings {
                low_water: 80,
                high_water: 160,
                per_subnet_cap: 4,
                max_protected_share: 0.5,
            },
            HashSet::new(),
            Metrics::new(),
            Arc::new(Subscriptions::new(16)),
            Arc::new(std::sync::Mutex::new(PeerStore::default())),
        )
        .await
        .unwrap();
        tokio::spawn(event_loop.run());

        let candidates: Vec<PeerId> = (0..5).map(|_| PeerId::random()).collect();
        let settings = FetchSettings {
            peer_timeout: Duration::from_millis(100),
            max_size_bytes: 1024,
            max_peer_attempts: 2,
        };
        let cooldowns = FetchCooldowns::default();
        let (tx, _rx) = mpsc::channel(16);
        let metrics = Metrics::new();

        let result = fetch_from_peers(
            control,
            candidates.clone(),
            "a".repeat(64),
            settings,
            &cooldowns,
            metrics.clone(),
            tx,
        )
        .await;
        assert!(matches!(result, Err(FetchError::NotFound)));

        // Only the first two candidates were attempted; the attempt cap
        // stopped the loop before the rest. Unreachable peers are skipped
        // without a cooldown penalty (it is not the peer's fault).
        let skipped = metrics
            .p2p_events
            .get_or_create(&crate::metrics::EventLabel {
                event_type: "fetch_peer_skipped".to_string(),
            })
            .get();
        assert_eq!(skipped, 2);
        for candidate in &candidates {
            assert!(!cooldowns.is_cooling(candidate));
        }
    }

    #[tokio::test]
    async fn cooling_peers_are_skipped_without_consuming_attempts() {
        let (_client, event_loop, control) = network::new(
            identity::Keypair::generate_ed25519(),
            NetworkSettings {
                low_water: 80,
                high_water: 160,
                per_subnet_cap: 4,
                max_protected_share: 0.5,
            },
            HashSet::new(),
            Metrics::new(),
            Arc::new(Subscriptions::new(16)),
            Arc::new(std::sync::Mutex::new(PeerStore::default())),
        )
        .await
        .unwrap();
        tokio::spawn(event_loop.run());

        let candidates: Vec<PeerId> = (0..3).map(|_| PeerId::random()).collect();
        let cooldowns = FetchCooldowns::default();
        cooldowns.penalize(candidates[0]);
        cooldowns.penalize(candidates[1]);

        let settings = FetchSettings {
            peer_timeout: Duration::from_millis(100),
            max_size_bytes: 1024,
            max_peer_attempts: 1,
        };
        let (tx, _rx) = mpsc::channel(16);
        let metrics = Metrics::new();

        let result = fetch_from_peers(
            control,
            candidates.clone(),
            "a".repeat(64),
            settings,
            &cooldowns,
            metrics.clone(),
            tx,
        )
        .await;
        assert!(matches!(result, Err(FetchError::NotFound)));

        // The two cooling peers were skipped without consuming the single
        // attempt, which went to the third candidate. Being unreachable,
        // that candidate is skipped without a penalty.
        let skipped = metrics
            .p2p_events
            .get_or_create(&crate::metrics::EventLabel {
                event_type: "fetch_peer_skipped".to_string(),
            })
            .get();
        assert_eq!(skipped, 1);
        assert!(!cooldowns.is_cooling(&candidates[2]));
    }
}
