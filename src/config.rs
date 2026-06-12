use libp2p::Multiaddr;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(transparent)]
pub struct Port(pub u16);

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(default)]
pub struct P2PConfig {
    /// Port to use for P2P communication.
    pub port: Port,
    /// Host/interface the gRPC control/pubsub API binds to. Defaults to
    /// 0.0.0.0 (all interfaces) for cross-container clients; set to 127.0.0.1
    /// when the client is co-located, as the API is unauthenticated.
    pub grpc_host: String,
    /// Port of the gRPC control/pubsub API.
    /// `control_port` is the legacy key for this setting.
    #[serde(alias = "control_port")]
    pub grpc_port: Port,
    /// Port of the HTTP metrics/health server.
    pub metrics_port: Port,
    /// Bootstrap peers (multiaddr format).
    pub peers: Vec<Multiaddr>,
    /// Topics to subscribe to.
    pub topics: Vec<String>,
    /// Number of HTTP metrics server workers. The server only serves
    /// /metrics and /health, so 1 worker is typically sufficient.
    pub nb_api_workers: usize,
    /// Path of the persisted peerstore file.
    pub peerstore_path: std::path::PathBuf,
    /// Maintain at least this many connections (maintenance dials below it).
    pub low_water: usize,
    /// Disconnect non-protected peers above this many connected peers.
    /// Enforcement is peer-level: a peer with multiple connections counts once.
    pub high_water: usize,
    /// Maximum connections per IPv4 /24 subnet for non-protected peers.
    pub per_subnet_cap: usize,
    /// Maximum share of high_water that protected (preferred) peers may occupy.
    pub max_protected_share: f32,
    /// Seconds between mesh maintenance passes (bootstrap anchoring,
    /// preferred-peer dialing, low-water refill from the peerstore).
    pub maintenance_interval_secs: u64,
    /// Base URL of the local pyaleph API used to serve inbound fetch
    /// requests. Empty string disables the provider side.
    #[serde(default = "default_content_provider_url")]
    pub content_provider_url: String,
    /// Maximum content size served or accepted by the fetch protocol.
    #[serde(default = "default_fetch_max_size_bytes")]
    pub fetch_max_size_bytes: u64,
    /// Maximum concurrent inbound fetch streams (global).
    #[serde(default = "default_fetch_max_inbound_streams")]
    pub fetch_max_inbound_streams: usize,
    /// Maximum concurrent inbound fetch streams per remote peer.
    #[serde(default = "default_fetch_max_inbound_streams_per_peer")]
    pub fetch_max_inbound_streams_per_peer: usize,
    /// Token-bucket rate limit on bytes served, per second. 0 disables it.
    #[serde(default = "default_fetch_serve_bytes_per_sec")]
    pub fetch_serve_bytes_per_sec: u64,
    /// Per-peer timeout for one fetch attempt step (open/header/chunk read).
    #[serde(default = "default_fetch_peer_timeout_secs")]
    pub fetch_peer_timeout_secs: u64,
    /// Total wall-clock deadline for a Fetch RPC.
    #[serde(default = "default_fetch_total_deadline_secs")]
    pub fetch_total_deadline_secs: u64,
    /// Maximum number of peers tried per Fetch RPC.
    #[serde(default = "default_fetch_max_peer_attempts")]
    pub fetch_max_peer_attempts: usize,
}

fn default_content_provider_url() -> String {
    String::new()
}
fn default_fetch_max_size_bytes() -> u64 {
    256 * 1024 * 1024
}
fn default_fetch_max_inbound_streams() -> usize {
    32
}
fn default_fetch_max_inbound_streams_per_peer() -> usize {
    4
}
fn default_fetch_serve_bytes_per_sec() -> u64 {
    64 * 1024 * 1024
}
fn default_fetch_peer_timeout_secs() -> u64 {
    10
}
fn default_fetch_total_deadline_secs() -> u64 {
    60
}
fn default_fetch_max_peer_attempts() -> usize {
    5
}

const PEER_MULTIADDR_ERROR_MESSAGE: &str = "bootstrap peer multiaddr should be valid";

impl Default for P2PConfig {
    fn default() -> Self {
        P2PConfig {
            port: Port(4025),
            grpc_host: "0.0.0.0".to_owned(),
            grpc_port: Port(4030),
            metrics_port: Port(4040),
            peers: vec![
                "/dns/api2.aleph.im/tcp/4025/p2p/QmZkurbY2G2hWay59yiTgQNaQxHSNzKZFt2jbnwJhQcKgV"
                    .parse()
                    .expect(PEER_MULTIADDR_ERROR_MESSAGE),
                "/dns/api3.aleph.im/tcp/4025/p2p/Qmb5b2ZwJm9pVWrppf3D3iMF1bXbjZhbJTwGvKEBMZNxa2"
                    .parse()
                    .expect(PEER_MULTIADDR_ERROR_MESSAGE),
            ],
            topics: vec!["ALIVE".to_owned(), "ALEPH-TEST".to_owned()],
            nb_api_workers: 4,
            peerstore_path: std::path::PathBuf::from("peerstore.json"),
            low_water: 80,
            high_water: 160,
            per_subnet_cap: 4,
            max_protected_share: 0.5,
            maintenance_interval_secs: 30,
            content_provider_url: default_content_provider_url(),
            fetch_max_size_bytes: default_fetch_max_size_bytes(),
            fetch_max_inbound_streams: default_fetch_max_inbound_streams(),
            fetch_max_inbound_streams_per_peer: default_fetch_max_inbound_streams_per_peer(),
            fetch_serve_bytes_per_sec: default_fetch_serve_bytes_per_sec(),
            fetch_peer_timeout_secs: default_fetch_peer_timeout_secs(),
            fetch_total_deadline_secs: default_fetch_total_deadline_secs(),
            fetch_max_peer_attempts: default_fetch_max_peer_attempts(),
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(default)]
pub struct SentryConfig {
    pub dsn: Option<String>,
    pub traces_sample_rate: Option<f32>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct AppConfig {
    #[serde(default)]
    pub p2p: P2PConfig,
    #[serde(default)]
    pub sentry: SentryConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Legacy configs from the previous (python) p2p service must keep working.
    /// Two distinct guarantees are exercised here:
    ///   1. `control_port` is mapped to `grpc_port` via serde alias (proven with
    ///      a NON-default value so a coincidental default cannot mask a failure).
    ///   2. Removed keys (http_port, listen_port, daemon_host, reconnect_delay,
    ///      alive_topic, clients) are silently ignored rather than causing a
    ///      parse error. They have no equivalent in this service and are
    ///      intentionally dropped, not remapped.
    #[test]
    fn legacy_control_port_maps_and_removed_keys_are_ignored() {
        let yaml = r#"
p2p:
  http_port: 4024
  port: 4025
  control_port: 4031
  listen_port: 4031
  daemon_host: p2p-service
  reconnect_delay: 60
  alive_topic: ALIVE
  clients: [http]
  topics: [ALIVE, ALEPH-TEST]
rabbitmq:
  host: rabbitmq
  username: aleph-p2p
  password: secret
"#;
        let config: AppConfig = serde_yaml::from_str(yaml).expect("legacy config should parse");
        assert_eq!(config.p2p.port.0, 4025);
        // 4031 (not the 4030 default): proves control_port -> grpc_port mapping.
        assert_eq!(config.p2p.grpc_port.0, 4031);
        assert_eq!(
            config.p2p.topics,
            vec!["ALIVE".to_string(), "ALEPH-TEST".to_string()]
        );
    }

    #[test]
    fn fetch_defaults_are_sane() {
        let config = P2PConfig::default();
        assert!(config.content_provider_url.is_empty());
        assert_eq!(config.fetch_max_size_bytes, 256 * 1024 * 1024);
        assert_eq!(config.fetch_max_inbound_streams, 32);
        assert_eq!(config.fetch_max_inbound_streams_per_peer, 4);
        assert_eq!(config.fetch_serve_bytes_per_sec, 64 * 1024 * 1024);
        assert_eq!(config.fetch_peer_timeout_secs, 10);
        assert_eq!(config.fetch_total_deadline_secs, 60);
        assert_eq!(config.fetch_max_peer_attempts, 5);
    }
}
