use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tonic::transport::Channel;

use aleph_p2p_service::grpc::proto::aleph_p2p_client::AlephP2pClient;
use aleph_p2p_service::grpc::proto::{
    DialRequest, GetPeersRequest, IdentifyRequest, PreferredPeer, PublishRequest,
    SetPreferredPeersRequest, SubscribeRequest,
};
use aleph_p2p_service::grpc::GrpcService;
use aleph_p2p_service::metrics::Metrics;
use aleph_p2p_service::p2p::network::{self, NetworkSettings};
use aleph_p2p_service::p2p::peerstore::PeerStore;
use aleph_p2p_service::subscriptions::Subscriptions;

async fn start_service() -> (AlephP2pClient<Channel>, String) {
    let subscriptions = Arc::new(Subscriptions::new(1024));
    let (mut client, event_loop) = network::new(
        libp2p::identity::Keypair::generate_ed25519(),
        NetworkSettings {
            low_water: 80,
            high_water: 160,
            per_subnet_cap: 4,
            max_protected_share: 0.5,
        },
        HashSet::new(),
        Metrics::new(),
        subscriptions.clone(),
        Arc::new(Mutex::new(PeerStore::default())),
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

#[tokio::test]
async fn dial_with_invalid_peer_id_returns_invalid_argument() {
    let (mut grpc, _peer_id) = start_service().await;
    let err = grpc
        .dial(DialRequest {
            peer_id: "not-a-peer-id".to_string(),
            multiaddr: "/ip4/127.0.0.1/tcp/1".to_string(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn publish_empty_topic_returns_invalid_argument() {
    let (mut grpc, _peer_id) = start_service().await;
    let err = grpc
        .publish(PublishRequest {
            topic: "".to_string(),
            payload: b"hello".to_vec(),
            echo: false,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn set_preferred_peers_accepts_and_reports_counts() {
    let (mut grpc, _peer_id) = start_service().await;
    let peer = libp2p::PeerId::from(libp2p::identity::Keypair::generate_ed25519().public());
    let result = grpc
        .set_preferred_peers(SetPreferredPeersRequest {
            peers: vec![PreferredPeer {
                peer_id: peer.to_string(),
                multiaddrs: vec!["/ip4/127.0.0.1/tcp/4025".to_string()],
            }],
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(result.accepted, 1);
    assert_eq!(result.truncated, 0);
}

#[tokio::test]
async fn set_preferred_peers_skips_invalid_entries() {
    let (mut grpc, _peer_id) = start_service().await;
    let good = libp2p::PeerId::from(libp2p::identity::Keypair::generate_ed25519().public());
    let result = grpc
        .set_preferred_peers(SetPreferredPeersRequest {
            peers: vec![
                PreferredPeer {
                    peer_id: "not-a-peer-id".to_string(),
                    multiaddrs: vec![],
                },
                PreferredPeer {
                    peer_id: good.to_string(),
                    multiaddrs: vec!["garbage".to_string(), "/ip4/10.0.0.1/tcp/4025".to_string()],
                },
            ],
        })
        .await
        .unwrap()
        .into_inner();
    // The invalid peer id entry is skipped; the good peer survives with its valid addr.
    assert_eq!(result.accepted, 1);
    assert_eq!(result.truncated, 0);
}

#[tokio::test]
async fn set_preferred_peers_truncates_over_the_protected_share_cap() {
    // start_service uses high_water 160 and max_protected_share 0.5 -> cap 80.
    let (mut grpc, _peer_id) = start_service().await;
    let peers: Vec<PreferredPeer> = (0..85)
        .map(|_| {
            let p = libp2p::PeerId::from(libp2p::identity::Keypair::generate_ed25519().public());
            PreferredPeer {
                peer_id: p.to_string(),
                multiaddrs: vec![],
            }
        })
        .collect();
    let result = grpc
        .set_preferred_peers(SetPreferredPeersRequest { peers })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(result.accepted, 80);
    assert_eq!(result.truncated, 5);
}

#[tokio::test]
async fn get_peers_returns_connected_peers_with_preferred_flag() {
    let (mut grpc_a, peer_id_a) = start_service().await;
    let (mut grpc_b, _peer_id_b) = start_service().await;

    let info_a = grpc_a
        .identify(IdentifyRequest {})
        .await
        .unwrap()
        .into_inner();
    let addr_a = info_a.listen_multiaddrs.first().unwrap().clone();

    // Mark A preferred on B before dialing.
    grpc_b
        .set_preferred_peers(SetPreferredPeersRequest {
            peers: vec![PreferredPeer {
                peer_id: peer_id_a.clone(),
                multiaddrs: vec![],
            }],
        })
        .await
        .unwrap();

    grpc_b
        .dial(DialRequest {
            peer_id: peer_id_a.clone(),
            multiaddr: addr_a,
        })
        .await
        .unwrap();

    // Allow a gossipsub heartbeat (1s interval) so the peer score, including
    // the app-specific bonus for preferred peers, is refreshed.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let peers = grpc_b
        .get_peers(GetPeersRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(peers.peers.len(), 1);
    assert_eq!(peers.peers[0].peer_id, peer_id_a);
    assert!(peers.peers[0].preferred);
    // Peer scoring is enabled with app_specific_weight = 1.0, so the
    // preferred-peer bonus (100.0) must surface as a positive score. Other
    // score components can shift the exact value, so only require > 0.
    assert!(
        peers.peers[0].score > 0.0,
        "preferred peer should have a positive application score, got {}",
        peers.peers[0].score
    );
}

#[tokio::test]
async fn set_preferred_peers_replacement_drops_old_set() {
    let (mut grpc_a, peer_id_a) = start_service().await;
    let (mut grpc_b, _peer_id_b) = start_service().await;

    let info_a = grpc_a
        .identify(IdentifyRequest {})
        .await
        .unwrap()
        .into_inner();
    let addr_a = info_a.listen_multiaddrs.first().unwrap().clone();

    grpc_b
        .set_preferred_peers(SetPreferredPeersRequest {
            peers: vec![PreferredPeer {
                peer_id: peer_id_a.clone(),
                multiaddrs: vec![],
            }],
        })
        .await
        .unwrap();

    grpc_b
        .dial(DialRequest {
            peer_id: peer_id_a.clone(),
            multiaddr: addr_a,
        })
        .await
        .unwrap();

    let peers = grpc_b
        .get_peers(GetPeersRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(peers.peers.len(), 1);
    assert_eq!(peers.peers[0].peer_id, peer_id_a);
    assert!(peers.peers[0].preferred);

    // The preferred set is a full replacement: setting a different peer must
    // drop A from the preferred set.
    let other = libp2p::PeerId::from(libp2p::identity::Keypair::generate_ed25519().public());
    grpc_b
        .set_preferred_peers(SetPreferredPeersRequest {
            peers: vec![PreferredPeer {
                peer_id: other.to_string(),
                multiaddrs: vec![],
            }],
        })
        .await
        .unwrap();

    let peers = grpc_b
        .get_peers(GetPeersRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(peers.peers.len(), 1);
    assert_eq!(peers.peers[0].peer_id, peer_id_a);
    assert!(
        !peers.peers[0].preferred,
        "A must lose its preferred flag after a full replacement"
    );
}
