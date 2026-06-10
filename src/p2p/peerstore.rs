use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use libp2p::{Multiaddr, PeerId};
use log::warn;
use serde::{Deserialize, Serialize};

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRecord {
    pub addrs: Vec<String>,
    pub last_seen_unix: u64,
}

/// Known peers and their addresses, persisted across restarts so the node
/// does not depend solely on bootstrap peers after the first run.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PeerStore {
    peers: HashMap<String, PeerRecord>,
}

const MAX_ADDRS_PER_PEER: usize = 8;

impl PeerStore {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(store) => store,
                Err(e) => {
                    warn!("Corrupt peerstore at {:?} ({}), starting empty", path, e);
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Atomic save: write to a temp file in the same directory, then rename.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp_path = path.with_extension("json.tmp");
        let contents = serde_json::to_string(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp_path, contents)?;
        std::fs::rename(&tmp_path, path)
    }

    pub fn record(
        &mut self,
        peer_id: &PeerId,
        addrs: impl IntoIterator<Item = Multiaddr>,
        now_unix: u64,
    ) {
        let record = self
            .peers
            .entry(peer_id.to_string())
            .or_insert_with(|| PeerRecord {
                addrs: Vec::new(),
                last_seen_unix: now_unix,
            });
        record.last_seen_unix = now_unix;
        for addr in addrs {
            let addr = addr.to_string();
            if !record.addrs.contains(&addr) {
                record.addrs.push(addr);
                if record.addrs.len() > MAX_ADDRS_PER_PEER {
                    record.addrs.remove(0);
                }
            }
        }
    }

    pub fn dial_candidates(&self, max_age_secs: u64, now_unix: u64) -> Vec<(PeerId, Multiaddr)> {
        let mut candidates = Vec::new();
        for (peer_id, record) in &self.peers {
            if now_unix.saturating_sub(record.last_seen_unix) > max_age_secs {
                continue;
            }
            let Ok(peer_id) = PeerId::from_str(peer_id) else {
                continue;
            };
            for addr in &record.addrs {
                if let Ok(addr) = Multiaddr::from_str(addr) {
                    candidates.push((peer_id, addr));
                }
            }
        }
        candidates
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p::identity;
    use tempfile::TempDir;

    fn make_peer_id() -> PeerId {
        let keypair = identity::Keypair::generate_ed25519();
        PeerId::from(keypair.public())
    }

    fn make_addr(port: u16) -> Multiaddr {
        format!("/ip4/127.0.0.1/tcp/{}", port).parse().unwrap()
    }

    #[test]
    fn record_and_get_candidates() {
        let mut store = PeerStore::default();
        let peer_id = make_peer_id();
        let addr = make_addr(4025);

        store.record(&peer_id, [addr.clone()], 1000);

        let candidates = store.dial_candidates(3600, 2000);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, peer_id);
        assert_eq!(candidates[0].1, addr);
    }

    #[test]
    fn stale_peers_are_not_candidates() {
        let mut store = PeerStore::default();
        let peer_id = make_peer_id();
        let addr = make_addr(4025);

        store.record(&peer_id, [addr], 1000);

        // 1000 + 3601 = 4601; age = 3601 > max_age 3600
        let candidates = store.dial_candidates(3600, 1000 + 3601);
        assert!(candidates.is_empty());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let tmp_dir = TempDir::new().unwrap();
        let path = tmp_dir.path().join("peerstore.json");

        let mut store = PeerStore::default();
        let peer_id = make_peer_id();
        store.record(&peer_id, [make_addr(4025), make_addr(4026)], 1000);

        store.save(&path).unwrap();

        let loaded = PeerStore::load(&path);
        assert_eq!(loaded.len(), 1);
        let candidates = loaded.dial_candidates(u64::MAX, 1000);
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn corrupt_file_loads_empty() {
        let tmp_dir = TempDir::new().unwrap();
        let path = tmp_dir.path().join("peerstore.json");

        std::fs::write(&path, "not json").unwrap();

        let store = PeerStore::load(&path);
        assert!(store.is_empty());
    }

    #[test]
    fn addrs_per_peer_are_capped() {
        let mut store = PeerStore::default();
        let peer_id = make_peer_id();

        let addrs: Vec<Multiaddr> = (0u16..20).map(make_addr).collect();
        store.record(&peer_id, addrs, 1000);

        let candidates = store.dial_candidates(u64::MAX, 1000);
        assert_eq!(candidates.len(), MAX_ADDRS_PER_PEER);
    }
}
