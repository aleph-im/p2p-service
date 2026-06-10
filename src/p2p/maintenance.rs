use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use log::{debug, info, warn};
use rand::seq::SliceRandom;

use crate::p2p::backoff::DialBackoff;
use crate::p2p::network::P2PClient;
use crate::p2p::peerstore::{now_unix, PeerStore};

const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(300);
const PEERSTORE_MAX_AGE_SECS: u64 = 7 * 24 * 3600;
/// Upper bound on preferred-peer dial attempts per maintenance tick.
/// Preferred dial hints come from the registry and are attacker-influenced:
/// a poisoned registry full of blackholed addresses would otherwise turn a
/// tick into dozens of sequential DIAL_TIMEOUT waits.
const MAX_PREFERRED_DIALS_PER_TICK: usize = 16;

#[derive(Debug, Clone)]
pub struct MaintenanceSettings {
    pub interval: Duration,
    pub low_water: usize,
}

/// Splits a `/.../p2p/<peer-id>` multiaddr into (peer_id, dial_addr).
pub fn split_peer_multiaddr(addr: &Multiaddr) -> Option<(PeerId, Multiaddr)> {
    let mut dial_addr = addr.clone();
    match dial_addr.pop() {
        Some(Protocol::P2p(peer_id)) => Some((peer_id, dial_addr)),
        _ => None,
    }
}

/// True for addresses that must never be dialed from the peerstore
/// (loopback or unspecified; advertised by nodes listening on 0.0.0.0).
fn is_undialable(addr: &Multiaddr) -> bool {
    addr.iter().any(|p| match p {
        Protocol::Ip4(ip) => ip.is_loopback() || ip.is_unspecified(),
        Protocol::Ip6(ip) => ip.is_loopback() || ip.is_unspecified(),
        _ => false,
    })
}

/// Keeps the node connected: bootstrap peers when isolated, preferred peers
/// always, peerstore peers when below the low watermark.
pub async fn run(
    mut client: P2PClient,
    bootstrap_multiaddrs: Vec<Multiaddr>,
    settings: MaintenanceSettings,
    peerstore: Arc<Mutex<PeerStore>>,
) {
    let bootstrap: Vec<(PeerId, Multiaddr)> = bootstrap_multiaddrs
        .iter()
        .filter_map(split_peer_multiaddr)
        .collect();
    if bootstrap.len() != bootstrap_multiaddrs.len() {
        warn!("Some bootstrap peers are missing a /p2p/<peer-id> suffix and were ignored");
    }

    let mut backoff = DialBackoff::new(BACKOFF_BASE, BACKOFF_CAP);

    loop {
        // Drop long-expired backoff entries so the map does not grow without
        // bound across churning peers.
        backoff.prune(Instant::now());

        let snapshot = match client.network_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                warn!("Maintenance: could not get network snapshot: {}", e);
                tokio::time::sleep(settings.interval).await;
                continue;
            }
        };

        // High-priority candidates: bootstrap re-anchoring first, then
        // preferred peers. Each candidate carries all of its known addresses
        // so a dead first address does not mask a live one.
        //
        // Ordering and capping matter here: preferred dial hints come from
        // the registry and are attacker-controlled. A poisoned registry full
        // of fail-slow (blackholed) hints must not consume the tick with
        // sequential DIAL_TIMEOUT waits and starve bootstrap re-anchoring,
        // so bootstrap entries go first (never capped; there are at most a
        // handful) and preferred dials are capped per tick.
        let mut priority_candidates: Vec<(PeerId, Vec<Multiaddr>)> = Vec::new();

        // Stay anchored to at least one bootstrap peer.
        let bootstrap_connected = bootstrap
            .iter()
            .any(|(peer_id, _)| snapshot.connected.contains(peer_id));
        if !bootstrap_connected {
            priority_candidates.extend(
                bootstrap
                    .iter()
                    .map(|(peer_id, addr)| (*peer_id, vec![addr.clone()])),
            );
        }

        // Preferred peers should always be connected. Drop undialable hint
        // addresses (loopback/unspecified) so a poisoned registry cannot make
        // us dial 127.0.0.1/0.0.0.0; shuffle so all hints rotate across ticks
        // even when the cap applies.
        let mut preferred_candidates: Vec<(PeerId, Vec<Multiaddr>)> = Vec::new();
        for (peer_id, addrs) in &snapshot.preferred {
            if snapshot.connected.contains(peer_id) {
                continue;
            }
            let dialable: Vec<Multiaddr> = addrs
                .iter()
                .filter(|addr| !is_undialable(addr))
                .cloned()
                .collect();
            if !dialable.is_empty() {
                preferred_candidates.push((*peer_id, dialable));
            }
        }
        preferred_candidates.shuffle(&mut rand::thread_rng());
        preferred_candidates.truncate(MAX_PREFERRED_DIALS_PER_TICK);
        priority_candidates.extend(preferred_candidates);

        // Below the low watermark: dial peers we have seen before.
        // Bound the number of peerstore refill dials per tick to avoid:
        //   - overshooting high_water when many candidates are queued, and
        //   - worst-case tick durations proportional to the full peerstore size
        //     (each dial attempt carries a 10-second timeout).
        // The budget adds a small slack (+2) so one slow connection does not
        // stall the refill completely.
        let refill_budget = settings.low_water.saturating_sub(snapshot.connected.len()) + 2;
        let mut refill_candidates: Vec<(PeerId, Multiaddr)> =
            if snapshot.connected.len() < settings.low_water {
                let stored = peerstore
                    .lock()
                    .expect("peerstore lock poisoned")
                    .dial_candidates(PEERSTORE_MAX_AGE_SECS, now_unix());
                let mut candidates: Vec<(PeerId, Multiaddr)> = stored
                    .into_iter()
                    .filter(|(peer_id, addr)| {
                        !snapshot.connected.contains(peer_id) && !is_undialable(addr)
                    })
                    .collect();
                // Shuffle so that no single peer dominates successive ticks.
                candidates.shuffle(&mut rand::thread_rng());
                candidates
            } else {
                Vec::new()
            };
        // Honour the per-tick budget for refill dials.
        refill_candidates.truncate(refill_budget);

        let mut attempted: HashSet<PeerId> = HashSet::new();
        for (peer_id, addrs) in priority_candidates.into_iter().chain(
            refill_candidates
                .into_iter()
                .map(|(peer_id, addr)| (peer_id, vec![addr])),
        ) {
            if snapshot.connected.contains(&peer_id) || attempted.contains(&peer_id) {
                continue;
            }
            if !backoff.ready(&peer_id, Instant::now()) {
                continue;
            }
            attempted.insert(peer_id);
            debug!("Maintenance: dialing {} at {:?}", peer_id, addrs);
            match tokio::time::timeout(DIAL_TIMEOUT, client.dial_and_wait(peer_id, addrs)).await {
                Ok(Ok(())) => {
                    info!("Maintenance: connected to {}", peer_id);
                    backoff.record_success(&peer_id);
                }
                _ => backoff.record_failure(peer_id, Instant::now()),
            }
        }

        tokio::time::sleep(settings.interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// split_peer_multiaddr extracts the peer ID and dial address from a
    /// /dns/.../p2p/<id> multiaddr.
    #[test]
    fn split_peer_multiaddr_extracts_id() {
        let addr: Multiaddr =
            "/dns/api2.aleph.im/tcp/4025/p2p/QmZkurbY2G2hWay59yiTgQNaQxHSNzKZFt2jbnwJhQcKgV"
                .parse()
                .unwrap();
        let (peer_id, dial_addr) = split_peer_multiaddr(&addr).unwrap();
        assert_eq!(
            peer_id.to_string(),
            "QmZkurbY2G2hWay59yiTgQNaQxHSNzKZFt2jbnwJhQcKgV"
        );
        assert_eq!(dial_addr, "/dns/api2.aleph.im/tcp/4025".parse().unwrap());
    }

    /// split_peer_multiaddr rejects an address without a /p2p suffix.
    #[test]
    fn split_peer_multiaddr_rejects_missing_p2p() {
        let addr: Multiaddr = "/dns/api2.aleph.im/tcp/4025".parse().unwrap();
        assert!(split_peer_multiaddr(&addr).is_none());
    }

    /// is_undialable flags loopback and unspecified addresses, but not
    /// routable IPv4 or DNS addresses.
    #[test]
    fn is_undialable_classification() {
        let loopback: Multiaddr = "/ip4/127.0.0.1/tcp/4025".parse().unwrap();
        let unspecified: Multiaddr = "/ip4/0.0.0.0/tcp/4025".parse().unwrap();
        let routable: Multiaddr = "/ip4/10.0.0.1/tcp/4025".parse().unwrap();
        let dns: Multiaddr = "/dns/example.org/tcp/4025".parse().unwrap();
        assert!(is_undialable(&loopback));
        assert!(is_undialable(&unspecified));
        assert!(!is_undialable(&routable));
        assert!(!is_undialable(&dns));
    }
}
