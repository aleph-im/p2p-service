use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use log::{debug, info, warn};

use crate::p2p::backoff::DialBackoff;
use crate::p2p::network::P2PClient;
use crate::p2p::peerstore::{now_unix, PeerStore};

const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(300);
const PEERSTORE_MAX_AGE_SECS: u64 = 7 * 24 * 3600;

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
        let snapshot = match client.network_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                warn!("Maintenance: could not get network snapshot: {}", e);
                tokio::time::sleep(settings.interval).await;
                continue;
            }
        };

        let mut candidates: Vec<(PeerId, Multiaddr)> = Vec::new();

        // Preferred peers should always be connected.
        for (peer_id, addrs) in &snapshot.preferred {
            if !snapshot.connected.contains(peer_id) {
                for addr in addrs {
                    candidates.push((*peer_id, addr.clone()));
                }
            }
        }

        // Stay anchored to at least one bootstrap peer.
        let bootstrap_connected = bootstrap
            .iter()
            .any(|(peer_id, _)| snapshot.connected.contains(peer_id));
        if !bootstrap_connected {
            candidates.extend(bootstrap.iter().cloned());
        }

        // Below the low watermark: dial peers we have seen before.
        if snapshot.connected.len() < settings.low_water {
            let stored = peerstore
                .lock()
                .expect("peerstore lock poisoned")
                .dial_candidates(PEERSTORE_MAX_AGE_SECS, now_unix());
            candidates.extend(stored.into_iter().filter(|(peer_id, addr)| {
                !snapshot.connected.contains(peer_id) && !is_undialable(addr)
            }));
        }

        let mut attempted: HashSet<PeerId> = HashSet::new();
        for (peer_id, addr) in candidates {
            if snapshot.connected.contains(&peer_id) || attempted.contains(&peer_id) {
                continue;
            }
            if !backoff.ready(&peer_id, Instant::now()) {
                continue;
            }
            attempted.insert(peer_id);
            debug!("Maintenance: dialing {} at {}", peer_id, addr);
            match tokio::time::timeout(DIAL_TIMEOUT, client.dial_and_wait(peer_id, addr)).await {
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
