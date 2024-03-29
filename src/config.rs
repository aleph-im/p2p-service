use libp2p::Multiaddr;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(transparent)]
pub struct Port(pub u16);

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(default)]
pub struct P2PConfig {
    /// Port of the REST API dedicated to calls between peers.
    pub http_port: Port,
    /// Port to use for P2P communication.
    pub port: Port,
    /// Port of the P2P daemon control API.
    pub control_port: Port,
    /// Response port for the P2P daemon.
    pub listen_port: Port,
    /// URL of the P2P daemon.
    pub daemon_host: String,
    /// P2P reconnection delay, in case of error.
    pub reconnect_delay: u32,
    /// Name of the "alive" topic.
    pub alive_topic: String,
    /// Protocols to use for P2P communication.
    pub clients: Vec<String>,
    /// Bootstrap peers (multiaddr format).
    pub peers: Vec<Multiaddr>,
    /// Topics to subscribe to.
    pub topics: Vec<String>,
    /// Number of API workers.
    pub nb_api_workers: usize,
}

const PEER_MULTIADDR_ERROR_MESSAGE: &str = "bootstrap peer multiaddr should be valid";

impl Default for P2PConfig {
    fn default() -> Self {
        P2PConfig {
            http_port: Port(4024),
            port: Port(4025),
            control_port: Port(4030),
            listen_port: Port(4031),
            daemon_host: "p2pd".to_owned(),
            reconnect_delay: 60,
            alive_topic: "ALIVE".to_owned(),
            clients: vec!["http".to_owned()],
            peers: vec![
                "/dns/api1.aleph.im/tcp/4025/p2p/Qmaxufiqdyt5uVWcy1Xh2nh3Rs3382ArnSP2umjCiNG2Vs"
                    .parse()
                    .expect(PEER_MULTIADDR_ERROR_MESSAGE),
                "/dns/api2.aleph.im/tcp/4025/p2p/QmZkurbY2G2hWay59yiTgQNaQxHSNzKZFt2jbnwJhQcKgV"
                    .parse()
                    .expect(PEER_MULTIADDR_ERROR_MESSAGE),
            ],
            topics: vec!["ALIVE".to_owned(), "ALEPH-TEST".to_owned()],
            nb_api_workers: 4,
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
#[serde(default)]
pub struct RabbitMqConfig {
    /// The hostname of the RabbitMQ instance.
    pub host: String,
    /// The AMQP port of the RabbitMQ instance.
    pub port: Port,
    /// Username.
    pub username: String,
    /// Password.
    pub password: String,
    /// Name of the exchange used to publish messages on the P2P network.
    pub pub_exchange: String,
    /// Name of the exchange used by the service to relay messages received from the P2P network.
    pub sub_exchange: String,
}

impl Default for RabbitMqConfig {
    fn default() -> Self {
        RabbitMqConfig {
            host: "127.0.0.1".to_owned(),
            port: Port(5672),
            username: "guest".to_owned(),
            password: "guest".to_owned(),
            pub_exchange: "p2p-publish".to_owned(),
            sub_exchange: "p2p-subscribe".to_owned(),
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct AppConfig {
    #[serde(default)]
    pub p2p: P2PConfig,
    #[serde(default)]
    pub sentry: SentryConfig,
    #[serde(default)]
    pub rabbitmq: RabbitMqConfig,
}
