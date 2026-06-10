use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aleph_p2p_service::metrics::Metrics;
use aleph_p2p_service::p2p::network::{self, NetworkSettings};
use aleph_p2p_service::p2p::peerstore::PeerStore;
use aleph_p2p_service::subscriptions::Subscriptions;

/// Old-stack (libp2p 0.51) swarm with the exact production parameters of v0.1.x.
fn make_old_swarm() -> libp2p_old::Swarm<libp2p_old::gossipsub::Behaviour> {
    use libp2p_old::core::upgrade;
    use libp2p_old::dns::TokioDnsConfig;
    use libp2p_old::gossipsub::{
        Behaviour as Gossipsub, ConfigBuilder, Message, MessageAuthenticity, MessageId,
        ValidationMode,
    };
    use libp2p_old::swarm::SwarmBuilder;
    use libp2p_old::tcp::tokio::Transport as TcpTransport;
    use libp2p_old::tcp::Config as TcpConfig;
    use libp2p_old::{identity, noise, yamux, PeerId, Transport};

    let id_keys = identity::Keypair::generate_ed25519();
    let peer_id = PeerId::from(id_keys.public());

    let transport = TokioDnsConfig::system(TcpTransport::new(TcpConfig::default().nodelay(true)))
        .unwrap()
        .upgrade(upgrade::Version::V1)
        .authenticate(noise::Config::new(&id_keys).unwrap())
        .multiplex(yamux::Config::default())
        .boxed();

    let message_id_fn = |message: &Message| {
        let mut s = DefaultHasher::new();
        message.data.hash(&mut s);
        MessageId::from(s.finish().to_string())
    };
    let gossipsub_config = ConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(1))
        .validation_mode(ValidationMode::Strict)
        .message_id_fn(message_id_fn)
        .max_transmit_size(262144)
        .max_messages_per_rpc(Some(100))
        .duplicate_cache_time(Duration::from_secs(1800))
        .build()
        .unwrap();
    let gossipsub = Gossipsub::new(MessageAuthenticity::Signed(id_keys), gossipsub_config).unwrap();

    SwarmBuilder::with_tokio_executor(transport, gossipsub, peer_id).build()
}

#[tokio::test]
async fn new_node_interops_with_libp2p_0_51_node() {
    use libp2p_old::futures::StreamExt as OldStreamExt;
    use libp2p_old::gossipsub::Event as OldGossipsubEvent;
    use libp2p_old::swarm::SwarmEvent as OldSwarmEvent;

    let topic_name = "interop-test";

    // New node.
    let subscriptions = Arc::new(Subscriptions::new(1024));
    let (mut new_client, new_loop) = network::new(
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
    tokio::spawn(new_loop.run());
    new_client
        .start_listening("/ip4/127.0.0.1/tcp/0".parse().unwrap())
        .await
        .unwrap();
    // Poll for the listen address (same pattern as tests/gossip_two_nodes.rs).
    let mut new_info = None;
    for _ in 0..50 {
        let info = new_client.identify().await.unwrap();
        if !info.listen_multiaddrs.is_empty() {
            new_info = Some(info);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let new_info = new_info.expect("new node never reported a listen address");
    let new_addr = new_info.listen_multiaddrs.first().unwrap().clone();
    new_client
        .subscribe(&libp2p::gossipsub::IdentTopic::new(topic_name))
        .await
        .unwrap();

    // Subscribe BEFORE dialing so no messages are missed.
    let mut rx = subscriptions.subscribe(topic_name);

    // Old node.
    let mut old_swarm = make_old_swarm();
    let old_topic = libp2p_old::gossipsub::IdentTopic::new(topic_name);
    old_swarm.behaviour_mut().subscribe(&old_topic).unwrap();
    let old_dial_addr: libp2p_old::Multiaddr = new_addr.to_string().parse().unwrap();
    old_swarm.dial(old_dial_addr).unwrap();

    // Drive the old swarm; after the mesh forms publish old -> new, then new -> old.
    let mut old_received: Option<Vec<u8>> = None;
    let mut new_received: Option<Vec<u8>> = None;
    let mut published_old_to_new = false;
    let mut new_publish_sent = false;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut graft_wait = Box::pin(tokio::time::sleep(Duration::from_secs(3)));

    while (old_received.is_none() || new_received.is_none())
        && tokio::time::Instant::now() < deadline
    {
        tokio::select! {
            event = OldStreamExt::select_next_some(&mut old_swarm) => {
                if let OldSwarmEvent::Behaviour(OldGossipsubEvent::Message { message, .. }) = event {
                    old_received = Some(message.data);
                }
            }
            result = rx.recv(), if published_old_to_new => {
                match result {
                    Ok(envelope) => {
                        new_received = Some(envelope.data.to_vec());
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // Lagged: some messages dropped, keep waiting.
                        let _ = n;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = &mut graft_wait, if !published_old_to_new => {
                old_swarm
                    .behaviour_mut()
                    .publish(old_topic.clone(), b"hello from old".to_vec())
                    .unwrap();
                published_old_to_new = true;
            }
            _ = tokio::time::sleep(Duration::from_millis(500)), if published_old_to_new && !new_publish_sent => {
                new_client
                    .publish(&libp2p::gossipsub::IdentTopic::new(topic_name), b"hello from new")
                    .await
                    .unwrap();
                new_publish_sent = true;
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }

    assert_eq!(new_received.as_deref(), Some(b"hello from old".as_slice()));
    assert_eq!(old_received.as_deref(), Some(b"hello from new".as_slice()));
}
