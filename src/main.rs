use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use actix_web::web::Data;
use actix_web::{middleware, App, HttpServer};
use clap::Parser;
use futures::StreamExt;
use lapin::message::Delivery;
use lapin::options::BasicAckOptions;
use libp2p::multiaddr::Protocol;
use libp2p::{gossipsub, identity, Multiaddr, PeerId};
use log::{debug, error, info, warn};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use aleph_p2p_service::config::AppConfig;
use aleph_p2p_service::http::AppState;
use aleph_p2p_service::message_queue::{self, RabbitMqClient};
use aleph_p2p_service::metrics::Metrics;
use aleph_p2p_service::p2p::network::P2PClient;
use aleph_p2p_service::subscriptions::{Envelope, Subscriptions};
use aleph_p2p_service::{http, p2p};
use tokio_stream::wrappers::TcpListenerStream;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct CliArgs {
    /// Path to the config file
    #[clap(short, long, value_parser, default_value = "config.yml")]
    config: PathBuf,

    /// Path to the private key file
    #[clap(short, long, value_parser, default_value = "node-secret.pkcs8.der")]
    private_key_file: PathBuf,
}

fn read_config(config_file: &PathBuf) -> AppConfig {
    let f = std::fs::File::open(config_file)
        .unwrap_or_else(|e| panic!("Could not open config file {:?}: {}", config_file, e));
    serde_yaml::from_reader(&f)
        .unwrap_or_else(|e| panic!("Invalid YAML content in {:?}: {}", config_file, e))
}

fn load_p2p_private_key(private_key_path: &PathBuf) -> identity::Keypair {
    // Note for later: translate RSA PEM key with:
    // openssl pkcs8 -topk8 -inform PEM -outform DER -in node-secret.key -out node-secret.pkcs8.der -nocrypt

    // let private_key_path = std::path::Path::new(private_key_file);
    let mut private_key_bytes = std::fs::read(private_key_path).unwrap_or_else(|e| {
        panic!(
            "Could not load private key file {:?}: {}",
            private_key_path, e
        )
    });
    let rsa_keypair = identity::rsa::Keypair::try_decode_pkcs8(private_key_bytes.as_mut())
        .unwrap_or_else(|e| {
            panic!(
                "Could not decode private key from {:?}: {}",
                private_key_path, e
            )
        });

    identity::Keypair::from(rsa_keypair)
}

async fn dial_bootstrap_peers(network_client: &mut P2PClient, peers: &[Multiaddr]) {
    for peer_addr in peers.iter() {
        let mut addr = peer_addr.clone();
        let last_protocol = addr.pop();
        let peer_id = match last_protocol {
            Some(Protocol::P2p(peer_id)) => peer_id,
            _ => {
                error!("Bootstrap peer multiaddr must end with its peer ID (/p2p/<peer-id>).");
                continue;
            }
        };

        match tokio::time::timeout(
            Duration::from_secs(10),
            network_client.dial_and_wait(peer_id, addr),
        )
        .await
        {
            Err(_) => error!("Timed out while dialing bootstrap peer {}", peer_addr),
            Ok(result) => match result {
                Ok(_) => info!("Successfully dialed bootstrap peer: {}", &peer_addr),
                Err(e) => error!("Failed to dial bootstrap peer {}: {}", peer_addr, e),
            },
        }
    }
}

async fn subscribe_to_topics(
    network_client: &mut P2PClient,
    topics: &Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    for topic in topics {
        info!("Subscribing to topic: {}", topic);
        let topic = gossipsub::IdentTopic::new(topic);
        if let Err(e) = network_client.subscribe(&topic).await {
            // A node that cannot subscribe runs deaf; abort startup.
            return Err(format!("could not subscribe to topic {}: {}", topic, e).into());
        }
    }
    Ok(())
}

async fn publish_message(network_client: &mut P2PClient, delivery: &Delivery, metrics: &Metrics) {
    let topic = gossipsub::IdentTopic::new(delivery.routing_key.as_str());
    info!("Publishing message on topic: {}", topic);
    match network_client.publish(&topic, &delivery.data).await {
        Ok(_) => {
            metrics.total_messages_sent.inc();
            metrics.increment_event("message_published");
        }
        Err(e) => {
            error!("Could not publish to P2P topic {}: {}", topic, e);
            metrics.increment_event("publish_error");
        }
    }
}

async fn mq_to_p2p_loop(
    mut mq_client: RabbitMqClient,
    mut network_client: P2PClient,
    metrics: Metrics,
) {
    while let Some(delivery) = mq_client.next().await {
        if let Ok(delivery) = delivery {
            publish_message(&mut network_client, &delivery, &metrics).await;
            if let Err(e) = delivery.ack(BasicAckOptions::default()).await {
                error!("Failed to acknowledge RabbitMQ message: {}", e);
            }
        }
    }
}

async fn p2p_to_mq_loop(
    mut mq_client: RabbitMqClient,
    topic_receivers: Vec<(String, broadcast::Receiver<Arc<Envelope>>)>,
    metrics: Metrics,
) {
    let streams = topic_receivers.into_iter().map(|(topic, rx)| {
        let stream = BroadcastStream::new(rx);
        Box::pin(stream.filter_map(move |item| {
            let topic = topic.clone();
            async move {
                match item {
                    Ok(envelope) => Some(envelope),
                    Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                        warn!(
                            "MQ bridge lagged on topic {}, dropped {} messages",
                            topic, n
                        );
                        None
                    }
                }
            }
        }))
    });
    let mut tasks = futures::stream::select_all(streams);

    while let Some(envelope) = tasks.next().await {
        let routing_key = format!("p2p.{}.{}", envelope.topic, envelope.source_peer_id);
        info!(
            "Forwarding p2p message from peer_id: {} on topic: {} routing_key: {}",
            envelope.source_peer_id, envelope.topic, routing_key,
        );
        match mq_client.publish(&routing_key, &envelope.data).await {
            Ok(_) => {
                metrics.total_messages_received.inc();
                metrics.increment_event("message_forwarded");
            }
            Err(e) => {
                error!("Failed to forward message to RabbitMQ: {}", e);
                metrics.increment_event("forward_error");
            }
        }
    }
    info!("p2p_to_mq_loop stopped");
}

fn configure_logging() {
    let mut log_builder = env_logger::builder();
    let logger = sentry::integrations::log::SentryLogger::with_dest(log_builder.build());
    if let Err(e) = log::set_boxed_logger(Box::new(logger)) {
        eprintln!("Failed to set global logger: {}", e);
    }
    log::set_max_level(log::LevelFilter::Info);
}

fn configure_sentry(app_config: &AppConfig) -> Option<sentry::ClientInitGuard> {
    app_config.sentry.dsn.as_ref().map(|dsn| {
        sentry::init((
            dsn.clone(),
            sentry::ClientOptions {
                release: sentry::release_name!(),
                ..Default::default()
            },
        ))
    })
}

#[actix_web::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    configure_logging();

    let args = CliArgs::parse();
    debug!("CLI args: {:?}", args);

    let app_config = read_config(&args.config);
    debug!("Config: {:?}", app_config);

    let _sentry_guard = configure_sentry(&app_config);

    let id_keys = load_p2p_private_key(&args.private_key_file);
    let peer_id = PeerId::from(id_keys.public());
    info!("Peer ID: {:?}", peer_id);

    let metrics = Metrics::new();

    let subscriptions = Arc::new(Subscriptions::new(1024));

    let (mut network_client, network_event_loop) = p2p::network::new(
        id_keys,
        metrics.connected_peers.clone(),
        subscriptions.clone(),
    )
    .await?;

    // Spawn the network task and run it in the background.
    let p2p_event_loop_handle = tokio::spawn(network_event_loop.run());

    // Start listening
    {
        let mut p2p_bind_multiaddr: Multiaddr = "/ip4/0.0.0.0"
            .parse()
            .expect("The hardcoded string should be a valid IPv4 multiaddr");
        p2p_bind_multiaddr.push(Protocol::Tcp(app_config.p2p.port.0));

        network_client
            .start_listening(p2p_bind_multiaddr)
            .await
            .expect("Listening not to fail.");
    }

    // Dial bootstrap peers
    dial_bootstrap_peers(&mut network_client, &app_config.p2p.peers).await;

    // Dedup topics (preserve order, discard duplicates).
    let topics: Vec<String> = {
        let mut seen = Vec::new();
        for t in &app_config.p2p.topics {
            if !seen.contains(t) {
                seen.push(t.clone());
            }
        }
        seen
    };

    // Create per-topic broadcast receivers BEFORE subscribing to gossipsub so that
    // no messages arriving between the subscribe call and the loop start are dropped.
    let topic_receivers: Vec<(String, _)> = topics
        .iter()
        .map(|t| (t.clone(), subscriptions.subscribe(t)))
        .collect();

    // Subscribe to topics
    subscribe_to_topics(&mut network_client, &topics).await?;

    // Create RabbitMQ exchanges/queues
    let mq_client = message_queue::new(&app_config).await?;

    let mq_to_p2p_handle = tokio::spawn(mq_to_p2p_loop(
        mq_client.clone(),
        network_client.clone(),
        metrics.clone(),
    ));
    let p2p_to_mq_handle =
        tokio::spawn(p2p_to_mq_loop(mq_client, topic_receivers, metrics.clone()));

    let grpc_service = aleph_p2p_service::grpc::GrpcService {
        client: network_client.clone(),
        subscriptions: subscriptions.clone(),
        local_peer_id: peer_id,
        metrics: metrics.clone(),
    };
    let grpc_bind_addr: std::net::SocketAddr =
        format!("0.0.0.0:{}", app_config.p2p.grpc_port.0).parse()?;
    let grpc_listener = tokio::net::TcpListener::bind(grpc_bind_addr).await?;
    info!("gRPC server listening on: {}", grpc_listener.local_addr()?);
    let grpc_handle = tokio::spawn(async move {
        if let Err(e) = tonic::transport::Server::builder()
            .add_service(grpc_service.into_server())
            .serve_with_incoming(TcpListenerStream::new(grpc_listener))
            .await
        {
            error!("gRPC server stopped: {}", e);
        }
    });

    let app_data = Data::new(AppState {
        app_config: app_config.clone(),
        p2p_client: network_client.clone(),
        peer_id,
        metrics: metrics.clone(),
    });

    let http_server_bind_address = format!("0.0.0.0:{}", &app_config.p2p.metrics_port.0);
    let http_server = HttpServer::new(move || {
        App::new()
            .app_data(Data::clone(&app_data))
            // enable logger - always register actix-web Logger middleware last
            .wrap(middleware::Logger::default())
            .wrap(sentry_actix::Sentry::new())
            // register HTTP requests handlers
            .configure(http::config)
    })
    .workers(app_config.p2p.nb_api_workers)
    .bind(http_server_bind_address)
    .expect("bind should succeed");

    info!("HTTP server listening on: {:?}", http_server.addrs());

    if let Err(e) = http_server.run().await {
        error!("HTTP server stopped: {:?}", e);
    }

    // If the HTTP server goes down, cancel the P2P loops as well
    let handles = vec![
        p2p_event_loop_handle,
        mq_to_p2p_handle,
        p2p_to_mq_handle,
        grpc_handle,
    ];
    for handle in handles {
        handle.abort();
        let _ = handle.await;
    }

    Ok(())
}
