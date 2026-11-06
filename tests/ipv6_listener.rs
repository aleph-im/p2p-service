//! The P2P listener binds both `/ip4/0.0.0.0` and `/ip6/::` on the same port
//! when `listen_ipv6` is enabled, and an IPv6-only peer can connect to it.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libp2p::multiaddr::Protocol;
use libp2p::{identity, Multiaddr};

use aleph_p2p_service::metrics::Metrics;
use aleph_p2p_service::p2p::network::{self, NetworkSettings, NodeInfo, P2PClient};
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

async fn start_node() -> P2PClient {
    let (client, event_loop) = network::new(
        identity::Keypair::generate_ed25519(),
        default_settings(),
        HashSet::new(),
        Metrics::new(),
        Arc::new(Subscriptions::new(1024)),
        Arc::new(Mutex::new(PeerStore::default())),
    )
    .await
    .unwrap();
    tokio::spawn(event_loop.run());
    client
}

fn has_ipv6_loopback() -> bool {
    std::net::TcpListener::bind("[::1]:0").is_ok()
}

/// A port that is free on both IPv4 and IPv6. Binding `[::]:0` without
/// IPV6_V6ONLY reserves the port for both families on Linux (bindv6only=0).
fn free_dual_stack_port() -> u16 {
    std::net::TcpListener::bind("[::]:0")
        .or_else(|_| std::net::TcpListener::bind("0.0.0.0:0"))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn is_ip4(addr: &Multiaddr) -> bool {
    matches!(addr.iter().next(), Some(Protocol::Ip4(_)))
}

fn is_ip6(addr: &Multiaddr) -> bool {
    matches!(addr.iter().next(), Some(Protocol::Ip6(_)))
}

fn tcp_port(addr: &Multiaddr) -> Option<u16> {
    addr.iter().find_map(|p| match p {
        Protocol::Tcp(port) => Some(port),
        _ => None,
    })
}

/// Polls `identify` until `done` holds for the listen addresses (5 s max).
async fn wait_for_listen_addrs(
    client: &mut P2PClient,
    done: impl Fn(&[Multiaddr]) -> bool,
) -> NodeInfo {
    let mut last = None;
    for _ in 0..50 {
        let info = client.identify().await.unwrap();
        if done(&info.listen_multiaddrs) {
            return info;
        }
        last = Some(info);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "listen addresses never reached the expected state: {:?}",
        last.map(|i| i.listen_multiaddrs)
    );
}

#[tokio::test]
async fn listens_on_ipv4_and_ipv6_and_accepts_ipv6_dial() {
    if !has_ipv6_loopback() {
        eprintln!("SKIPPED: no IPv6 loopback (::1) on this machine");
        return;
    }
    let port = free_dual_stack_port();

    let mut client_a = start_node().await;
    let ipv6_active = client_a.listen_on_port(port, true).await.unwrap();
    assert!(ipv6_active, "the /ip6/:: listener should start");

    let info_a = wait_for_listen_addrs(&mut client_a, |addrs| {
        addrs.iter().any(is_ip4) && addrs.iter().any(is_ip6)
    })
    .await;
    for addr in &info_a.listen_multiaddrs {
        assert_eq!(
            tcp_port(addr),
            Some(port),
            "every listen address should use the configured port: {addr}"
        );
    }

    let mut client_b = start_node().await;
    let ip6_loopback: Multiaddr = format!("/ip6/::1/tcp/{port}").parse().unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        client_b.dial_and_wait(info_a.peer_id, vec![ip6_loopback]),
    )
    .await
    .expect("dial over IPv6 timed out")
    .expect("dial over IPv6 should succeed");
}

#[tokio::test]
async fn listen_ipv6_false_reports_no_ipv6_address() {
    let port = free_dual_stack_port();

    let mut client = start_node().await;
    let ipv6_active = client.listen_on_port(port, false).await.unwrap();
    assert!(!ipv6_active);

    let info = wait_for_listen_addrs(&mut client, |addrs| addrs.iter().any(is_ip4)).await;
    assert!(
        !info.listen_multiaddrs.iter().any(is_ip6),
        "no /ip6/ address expected with listen_ipv6=false: {:?}",
        info.listen_multiaddrs
    );
}
