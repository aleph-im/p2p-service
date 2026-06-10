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
    /// Port of the gRPC control/pubsub API.
    pub grpc_port: Port,
    /// Port of the HTTP metrics/health server.
    pub metrics_port: Port,
    /// Bootstrap peers (multiaddr format).
    pub peers: Vec<Multiaddr>,
    /// Topics to subscribe to.
    pub topics: Vec<String>,
    /// Number of HTTP metrics server workers.
    pub nb_api_workers: usize,
    /// Path of the persisted peerstore file.
    pub peerstore_path: std::path::PathBuf,
    /// Maintain at least this many connections (maintenance dials below it).
    pub low_water: usize,
    /// Disconnect non-protected peers above this many connections.
    pub high_water: usize,
    /// Maximum connections per IPv4 /24 subnet for non-protected peers.
    pub per_subnet_cap: usize,
    /// Maximum share of high_water that protected (preferred) peers may occupy.
    pub max_protected_share: f32,
    /// Seconds between mesh maintenance passes (bootstrap anchoring,
    /// preferred-peer dialing, low-water refill from the peerstore).
    pub maintenance_interval_secs: u64,
}

const PEER_MULTIADDR_ERROR_MESSAGE: &str = "bootstrap peer multiaddr should be valid";

impl Default for P2PConfig {
    fn default() -> Self {
        P2PConfig {
            port: Port(4025),
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

    #[test]
    fn old_config_files_with_legacy_keys_still_parse() {
        let yaml = r#"
p2p:
  http_port: 4024
  port: 4025
  control_port: 4030
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
        assert_eq!(config.p2p.grpc_port.0, 4030);
        assert_eq!(
            config.p2p.topics,
            vec!["ALIVE".to_string(), "ALEPH-TEST".to_string()]
        );
    }
}
