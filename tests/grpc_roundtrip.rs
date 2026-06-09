use std::sync::Arc;
use std::time::Duration;

use prometheus_client::metrics::gauge::Gauge;
use tonic::transport::Channel;

use aleph_p2p_service::grpc::proto::aleph_p2p_client::AlephP2pClient;
use aleph_p2p_service::grpc::proto::{IdentifyRequest, PublishRequest, SubscribeRequest};
use aleph_p2p_service::grpc::GrpcService;
use aleph_p2p_service::metrics::Metrics;
use aleph_p2p_service::p2p::network;
use aleph_p2p_service::subscriptions::Subscriptions;

async fn start_service() -> (AlephP2pClient<Channel>, String) {
    let subscriptions = Arc::new(Subscriptions::new(1024));
    let (mut client, event_loop) = network::new(
        libp2p::identity::Keypair::generate_ed25519(),
        Gauge::default(),
        subscriptions.clone(),
    )
    .await
    .unwrap();
    tokio::spawn(event_loop.run());
    client
        .start_listening("/ip4/127.0.0.1/tcp/0".parse().unwrap())
        .await
        .unwrap();
    // Poll for the listen address (pattern from the other tests).
    let mut peer_id = None;
    for _ in 0..50 {
        let info = client.identify().await.unwrap();
        if !info.listen_multiaddrs.is_empty() {
            peer_id = Some(info.peer_id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let peer_id = peer_id.expect("node never reported a listen address");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_addr = listener.local_addr().unwrap();
    let service = GrpcService {
        client,
        subscriptions,
        local_peer_id: peer_id,
        metrics: Metrics::new(),
    };
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service.into_server())
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    let channel = tonic::transport::Endpoint::from_shared(format!("http://{grpc_addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    (AlephP2pClient::new(channel), peer_id.to_string())
}

#[tokio::test]
async fn identify_returns_local_peer_id() {
    let (mut grpc, peer_id) = start_service().await;
    let info = grpc
        .identify(IdentifyRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(info.peer_id, peer_id);
    assert!(!info.listen_multiaddrs.is_empty());
}

#[tokio::test]
async fn publish_with_echo_loops_back_to_subscriber() {
    let (mut grpc, peer_id) = start_service().await;

    let mut stream = grpc
        .subscribe(SubscribeRequest {
            topic: "test-topic".to_string(),
        })
        .await
        .unwrap()
        .into_inner();

    grpc.publish(PublishRequest {
        topic: "test-topic".to_string(),
        payload: b"echo me".to_vec(),
        echo: true,
    })
    .await
    .unwrap();

    let envelope = tokio::time::timeout(Duration::from_secs(5), stream.message())
        .await
        .expect("timed out waiting for echo")
        .unwrap()
        .expect("stream ended");
    assert_eq!(envelope.payload, b"echo me");
    assert_eq!(envelope.source_peer_id, peer_id);
    assert_eq!(envelope.topic, "test-topic");
}

#[tokio::test]
async fn publish_without_echo_does_not_loop_back() {
    let (mut grpc, _peer_id) = start_service().await;
    let mut stream = grpc
        .subscribe(SubscribeRequest {
            topic: "test-topic".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    grpc.publish(PublishRequest {
        topic: "test-topic".to_string(),
        payload: b"no echo".to_vec(),
        echo: false,
    })
    .await
    .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), stream.message()).await;
    assert!(result.is_err(), "expected no message, got {:?}", result);
}
