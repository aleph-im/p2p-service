use std::time::Duration;

use futures::StreamExt;
use libp2p::gossipsub::IdentTopic;
use libp2p::identity;
use prometheus_client::metrics::gauge::Gauge;

use aleph_p2p_service::p2p::network;

#[tokio::test]
async fn two_new_nodes_exchange_gossipsub_messages() {
    let topic = IdentTopic::new("interop-test");

    let (mut client_a, _events_a, loop_a) =
        network::new(identity::Keypair::generate_ed25519(), Gauge::default())
            .await
            .unwrap();
    let (mut client_b, mut events_b, loop_b) =
        network::new(identity::Keypair::generate_ed25519(), Gauge::default())
            .await
            .unwrap();

    tokio::spawn(loop_a.run());
    tokio::spawn(loop_b.run());

    client_a
        .start_listening("/ip4/127.0.0.1/tcp/0".parse().unwrap())
        .await
        .unwrap();
    // Wait for the listener to be registered, then read the actual address.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let info_a = client_a.identify().await.unwrap();
    let addr_a = info_a
        .listen_multiaddrs
        .first()
        .expect("node A should be listening")
        .clone();

    client_a.subscribe(&topic).await.unwrap();
    client_b.subscribe(&topic).await.unwrap();

    client_b
        .dial_and_wait(info_a.peer_id, addr_a)
        .await
        .unwrap();

    // Give gossipsub a few heartbeats to graft the mesh.
    tokio::time::sleep(Duration::from_secs(3)).await;

    client_a.publish(&topic, b"hello from A").await.unwrap();

    let received = tokio::time::timeout(Duration::from_secs(10), events_b.next())
        .await
        .expect("timed out waiting for message on node B")
        .expect("event stream ended");
    let network::Event::PubsubMessage { message } = received;
    assert_eq!(message.data, b"hello from A");
    assert_eq!(message.source, Some(info_a.peer_id));
}
