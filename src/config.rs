use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(transparent)]
pub struct Port(pub u16);

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct PendingMessageJobConfig {
    pub max_concurrency: u32,
    pub aggregate: Option<u32>,
    pub forget: Option<u32>,
    pub post: Option<u32>,
    pub program: Option<u32>,
    pub store: Option<u32>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct PendingTxsJobConfig {
    pub max_concurrency: u32,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct JobsConfig {
    pub pending_messages: PendingMessageJobConfig,
    pub pending_txs: PendingTxsJobConfig,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(default)]
pub struct AlephConfig {
    pub queue_topic: String,
    pub host: String,
    pub port: Port,
    pub reference_node_url: Option<String>,
    pub jobs: JobsConfig,
}

impl Default for AlephConfig {
    fn default() -> Self {
        AlephConfig {
            queue_topic: "ALEPH-QUEUE".to_string(),
            host: "0.0.0.0".to_string(),
            port: Port(8000),
            reference_node_url: None,
            jobs: JobsConfig {
                pending_messages: PendingMessageJobConfig {
                    max_concurrency: 2000,
                    aggregate: None,
                    forget: None,
                    post: None,
                    program: None,
                    store: Some(30),
                },
                pending_txs: PendingTxsJobConfig { max_concurrency: 0 },
            },
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(default)]
pub struct LoggingConfig {
    pub level: u32,
    pub max_log_file_size: usize,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        LoggingConfig {
            level: 30, // Corresponds to logging.WARNING in Python
            max_log_file_size: 1_000_000_000,
        }
    }
}

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
    pub peers: Vec<String>,
}

impl Default for P2PConfig {
    fn default() -> Self {
        P2PConfig {
            http_port: Port(4024),
            port: Port(4025),
            control_port: Port(4030),
            listen_port: Port(4031),
            daemon_host: "p2pd".to_string(),
            reconnect_delay: 60,
            alive_topic: "ALIVE".to_string(),
            clients: vec!["http".to_string()],
            peers: vec![
                "/ip4/51.159.57.71/tcp/4025/p2p/QmZkurbY2G2hWay59yiTgQNaQxHSNzKZFt2jbnwJhQcKgV"
                    .to_string(),
                "/ip4/95.216.100.234/tcp/4025/p2p/Qmaxufiqdyt5uVWcy1Xh2nh3Rs3382ArnSP2umjCiNG2Vs"
                    .to_string(),
                "/ip4/62.210.93.220/tcp/4025/p2p/QmXdci5feFmA2pxTg8p3FCyWmSKnWYAAmr7Uys1YCTFD8U"
                    .to_string(),
            ],
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(default)]
pub struct StorageConfig {
    pub folder: String,
    pub store_files: bool,
    pub engine: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig {
            folder: "./data".to_string(),
            store_files: false,
            engine: "mongodb".to_string(),
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(default)]
pub struct EthereumConfig {
    pub enabled: bool,
    pub api_url: String,
    pub packing_node: bool,
    pub chain_id: u32,
    pub private_key: Option<String>,
    pub sync_contract: Option<String>,
    pub start_height: u64,
    pub commit_delay: u32,
    pub token_contract: Option<String>,
    pub token_start_height: u64,
    pub max_gas_price: u64,
    pub authorized_emitters: Vec<String>,
}

impl Default for EthereumConfig {
    fn default() -> Self {
        EthereumConfig {
            enabled: false,
            api_url: "http://127.0.0.1:8545".to_string(),
            packing_node: false,
            chain_id: 1,
            private_key: None,
            sync_contract: None,
            start_height: 11400000,
            commit_delay: 35,
            token_contract: None,
            token_start_height: 10_900_000,
            max_gas_price: 150_000_000_000,
            authorized_emitters: vec!["0x23eC28598DCeB2f7082Cc3a9D670592DfEd6e0dC".to_string()],
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(default)]
pub struct MongodbConfig {
    pub uri: String,
    pub database: String,
}

impl Default for MongodbConfig {
    fn default() -> Self {
        MongodbConfig {
            uri: "mongodb://127.0.0.1:27017".to_string(),
            database: "aleph".to_string(),
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(default)]
pub struct IpfsConfig {
    pub enabled: bool,
    pub host: String,
    pub port: Port,
    pub gateway_port: Port,
    pub id: Option<String>,
    pub alive_topic: String,
    pub reconnect_delay: u32,
    pub peers: Vec<String>,
}

impl Default for IpfsConfig {
    fn default() -> Self {
        IpfsConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: Port(5001),
            gateway_port: Port(8080),
            id: None,
            alive_topic: "ALEPH_ALIVE".to_string(),
            reconnect_delay: 60,
            peers: vec![
                "/dnsaddr/api1.aleph.im/ipfs/12D3KooWNgogVS6o8fVsPdzh2FJpCdJJLVSgJT38XGE1BJoCerHx".to_string(),
                "/ip4/51.159.57.71/tcp/4001/p2p/12D3KooWBH3JVSBwHLNzxv7EzniBP3tDmjJaoa3EJBF9wyhZtHt2".to_string(),
                "/ip4/62.210.93.220/tcp/4001/p2p/12D3KooWLcmvqojHzUnR7rr8YhFKGDD8z7fmsPyBfAm2rT3sFGAF".to_string()],
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(default)]
pub struct SentryConfig {
    pub dsn: Option<String>,
    pub traces_sample_rate: Option<f32>,
}

impl Default for SentryConfig {
    fn default() -> Self {
        SentryConfig {
            dsn: None,
            traces_sample_rate: None,
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(default)]
pub struct RabbitMqConfig {
    pub host: String,
    pub port: Port,
    pub username: String,
    pub password: String,
    pub pub_exchange: String,
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
    pub logging: LoggingConfig,
    #[serde(default)]
    pub aleph: AlephConfig,
    #[serde(default)]
    pub ipfs: IpfsConfig,
    #[serde(default)]
    pub p2p: P2PConfig,
    #[serde(default)]
    pub ethereum: EthereumConfig,
    #[serde(default)]
    pub mongodb: MongodbConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub sentry: SentryConfig,
    #[serde(default)]
    pub rabbitmq: RabbitMqConfig,
}
