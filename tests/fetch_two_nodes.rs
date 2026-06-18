//! End-to-end coverage of the /aleph/fetch/1.0.0 protocol between two
//! in-process nodes: node A serves content from a local filesystem store,
//! node B fetches it over a libp2p stream (directly via the requester and
//! through the gRPC Fetch handler).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libp2p::{identity, Multiaddr, PeerId};
use tokio::sync::mpsc;
use tonic::Request;

use aleph_p2p_service::fetch::requester::{
    fetch_from_peers, FetchCooldowns, FetchError, FetchProgress, FetchSettings,
};
use aleph_p2p_service::fetch::{provider, FETCH_PROTOCOL};
use aleph_p2p_service::grpc::proto::aleph_p2p_server::AlephP2p;
use aleph_p2p_service::grpc::{proto, GrpcService};
use aleph_p2p_service::metrics::Metrics;
use aleph_p2p_service::p2p::network::{self, NetworkSettings, P2PClient};
use aleph_p2p_service::p2p::peerstore::PeerStore;
use aleph_p2p_service::subscriptions::Subscriptions;

fn default_settings() -> NetworkSettings {
    NetworkSettings {
        low_water: 80,
        high_water: 160,
        per_subnet_cap: 4,
        max_protected_share: 0.5,
    }
}

/// Deterministic payload large enough to span several 64 KiB chunks.
fn payload_300_kib() -> Vec<u8> {
    (0..300 * 1024).map(|i| (i % 251) as u8).collect()
}

/// Writes `content` to `dir/known_hash` so the provider can serve it from
/// the filesystem store.
async fn write_content(dir: &std::path::Path, known_hash: &str, content: &[u8]) {
    tokio::fs::write(dir.join(known_hash), content)
        .await
        .unwrap();
}

struct Node {
    client: P2PClient,
    control: libp2p_stream::Control,
    subscriptions: Arc<Subscriptions>,
}

async fn start_node() -> Node {
    let subscriptions = Arc::new(Subscriptions::new(1024));
    let (client, event_loop, control) = network::new(
        identity::Keypair::generate_ed25519(),
        default_settings(),
        HashSet::new(),
        Metrics::new(),
        subscriptions.clone(),
        Arc::new(Mutex::new(PeerStore::default())),
    )
    .await
    .unwrap();
    tokio::spawn(event_loop.run());
    Node {
        client,
        control,
        subscriptions,
    }
}

/// Starts listening on `node` and polls until a listen address is reported.
async fn listen_addr(node: &mut Node) -> (PeerId, Multiaddr) {
    node.client
        .start_listening("/ip4/127.0.0.1/tcp/0".parse().unwrap())
        .await
        .unwrap();
    for _ in 0..50 {
        let info = node.client.identify().await.unwrap();
        if let Some(addr) = info.listen_multiaddrs.first() {
            return (info.peer_id, addr.clone());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("node never reported a listen address within 5 seconds");
}

fn provider_settings(content_dir: Option<std::path::PathBuf>) -> provider::ProviderSettings {
    provider::ProviderSettings {
        content_dir,
        ipfs_api_url: None,
        max_size_bytes: 1024 * 1024,
        max_inbound_streams: 4,
        max_inbound_streams_per_peer: 2,
        serve_bytes_per_sec: 0,
    }
}

fn fetch_settings() -> FetchSettings {
    FetchSettings {
        peer_timeout: Duration::from_secs(10),
        max_size_bytes: 1024 * 1024,
        max_peer_attempts: 5,
    }
}

/// Two connected nodes: A serves `content` for `known_hash` from the local
/// filesystem store, B is dialed in and ready to fetch. Node A is returned
/// too so its command channel (and with it the event loop) stays alive for
/// the test. The returned `tempfile::TempDir` must be kept alive for the
/// duration of the test.
async fn connected_pair(
    known_hash: String,
    content: Vec<u8>,
) -> (Node, PeerId, Node, tempfile::TempDir) {
    let mut node_a = start_node().await;
    let mut node_b = start_node().await;

    let dir = tempfile::tempdir().unwrap();
    write_content(dir.path(), &known_hash, &content).await;

    let incoming = node_a
        .control
        .clone()
        .accept(FETCH_PROTOCOL)
        .expect("fetch protocol registered twice");
    tokio::spawn(provider::run(
        incoming,
        provider_settings(Some(dir.path().to_path_buf())),
        Metrics::new(),
    ));

    let (peer_a, addr_a) = listen_addr(&mut node_a).await;
    node_b
        .client
        .dial_and_wait(peer_a, vec![addr_a])
        .await
        .unwrap();
    // Settle delay: the connection is established, give the stream protocol
    // negotiation a moment before opening fetch streams.
    tokio::time::sleep(Duration::from_millis(500)).await;

    (node_a, peer_a, node_b, dir)
}

#[tokio::test]
async fn fetch_roundtrip() {
    let hash = "a".repeat(64);
    let content = payload_300_kib();
    let (_node_a, peer_a, node_b, _dir) = connected_pair(hash.clone(), content.clone()).await;

    let (tx, mut rx) = mpsc::channel(16);
    let collector = tokio::spawn(async move {
        let mut header_size = None;
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        while let Some(progress) = rx.recv().await {
            match progress {
                FetchProgress::Header { size } => header_size = Some(size),
                FetchProgress::Chunk(data) => chunks.push(data),
            }
        }
        (header_size, chunks)
    });

    let cooldowns = FetchCooldowns::default();
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        fetch_from_peers(
            node_b.control.clone(),
            vec![peer_a],
            hash,
            fetch_settings(),
            &cooldowns,
            Metrics::new(),
            tx,
        ),
    )
    .await
    .expect("fetch timed out");
    assert!(result.is_ok(), "fetch failed: {:?}", result.err());

    let (header_size, chunks) = collector.await.unwrap();
    assert_eq!(header_size, Some(content.len() as u64));
    assert!(
        chunks.len() > 1,
        "expected multiple chunks for a 300 KiB payload, got {}",
        chunks.len()
    );
    let reassembled: Vec<u8> = chunks.concat();
    assert_eq!(reassembled, content);
}

#[tokio::test]
async fn fetch_not_found_falls_through() {
    let known_hash = "a".repeat(64);
    let (_node_a, peer_a, node_b, _dir) =
        connected_pair(known_hash, b"some content".to_vec()).await;

    let (tx, mut rx) = mpsc::channel(16);
    let cooldowns = FetchCooldowns::default();
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        fetch_from_peers(
            node_b.control.clone(),
            vec![peer_a],
            "b".repeat(64),
            fetch_settings(),
            &cooldowns,
            Metrics::new(),
            tx,
        ),
    )
    .await
    .expect("fetch timed out");
    assert!(matches!(result, Err(FetchError::NotFound)));
    // An honest miss forwards nothing to the consumer.
    assert!(rx.recv().await.is_none());
    // And does not penalize the peer.
    assert!(!cooldowns.is_cooling(&peer_a));
}

#[tokio::test]
async fn fetch_grpc_end_to_end() {
    let hash = "a".repeat(64);
    let content = payload_300_kib();
    let (_node_a, peer_a, node_b, _dir) = connected_pair(hash.clone(), content.clone()).await;

    let local_peer_id = node_b.client.clone().identify().await.unwrap().peer_id;
    let service = GrpcService {
        client: node_b.client.clone(),
        subscriptions: node_b.subscriptions.clone(),
        local_peer_id,
        metrics: Metrics::new(),
        fetch_control: node_b.control.clone(),
        fetch_settings: fetch_settings(),
        fetch_total_deadline: Duration::from_secs(30),
        fetch_cooldowns: Arc::new(FetchCooldowns::default()),
    };

    let response = service
        .fetch(Request::new(proto::FetchRequest {
            item_hash: hash,
            preferred_peer_ids: vec![peer_a.to_string()],
        }))
        .await
        .expect("fetch call failed");
    let mut stream = response.into_inner();

    let mut reassembled = Vec::new();
    let mut total_size = None;
    while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
        let chunk = chunk.expect("fetch stream failed");
        total_size = Some(chunk.total_size);
        reassembled.extend_from_slice(&chunk.data);
    }
    assert_eq!(total_size, Some(content.len() as u64));
    assert_eq!(reassembled, content);
}
