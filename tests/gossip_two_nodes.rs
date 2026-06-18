use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libp2p::gossipsub::IdentTopic;
use libp2p::identity;

use aleph_p2p_service::metrics::Metrics;
use aleph_p2p_service::p2p::network::{self, NetworkSettings};
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

#[tokio::test]
async fn two_new_nodes_exchange_gossipsub_messages() {
    let topic = IdentTopic::new("interop-test");

    let subscriptions_a = Arc::new(Subscriptions::new(1024));
    let subscriptions_b = Arc::new(Subscriptions::new(1024));

    let (mut client_a, loop_a) = network::new(
        identity::Keypair::generate_ed25519(),
        default_settings(),
        HashSet::new(),
        Metrics::new(),
        subscriptions_a,
        Arc::new(Mutex::new(PeerStore::default())),
    )
    .await
    .unwrap();
    let (mut client_b, loop_b) = network::new(
        identity::Keypair::generate_ed25519(),
        default_settings(),
        HashSet::new(),
        Metrics::new(),
        subscriptions_b.clone(),
        Arc::new(Mutex::new(PeerStore::default())),
    )
    .await
    .unwrap();

    tokio::spawn(loop_a.run());
    tokio::spawn(loop_b.run());

    client_a
        .start_listening("/ip4/127.0.0.1/tcp/0".parse().unwrap())
        .await
        .unwrap();
    // Poll until the listener is registered, then read the actual address.
    let info_a = {
        let mut info = None;
        for _ in 0..50 {
            let candidate = client_a.identify().await.unwrap();
            if !candidate.listen_multiaddrs.is_empty() {
                info = Some(candidate);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        info.expect("node A never reported a listen address within 5 seconds")
    };
    let addr_a = info_a
        .listen_multiaddrs
        .first()
        .expect("node A should be listening")
        .clone();

    client_a.subscribe(&topic).await.unwrap();
    client_b.subscribe(&topic).await.unwrap();

    // Subscribe BEFORE dialing so no messages are missed.
    let mut rx = subscriptions_b.subscribe("interop-test");

    client_b
        .dial_and_wait(info_a.peer_id, vec![addr_a])
        .await
        .unwrap();

    // Give gossipsub a few heartbeats to graft the mesh.
    tokio::time::sleep(Duration::from_secs(3)).await;

    client_a.publish(&topic, b"hello from A").await.unwrap();

    let envelope = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for message on node B")
        .expect("subscription channel closed");
    assert_eq!(envelope.data, b"hello from A");
    assert_eq!(envelope.source_peer_id, info_a.peer_id.to_string());
}
