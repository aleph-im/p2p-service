use std::path::PathBuf;

use actix_web::web::Data;
use actix_web::{middleware, App, HttpServer};
use clap::Parser;
use futures::StreamExt;
use lapin::message::Delivery;
use lapin::options::BasicAckOptions;
use libp2p::multiaddr::Protocol;
use libp2p::{gossipsub, identity, Multiaddr, PeerId};
use log::{debug, error, info, warn};
use crate::gossipsub::GossipsubMessage;

use crate::message_queue::RabbitMqClient;
use crate::p2p::network::P2PClient;

mod config;
mod http;
mod message_queue;
mod p2p;

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

fn read_config(config_file: &PathBuf) -> config::AppConfig {
    let f = std::fs::File::open(config_file).expect("could not open config.yml");
    serde_yaml::from_reader(&f).expect("invalid YAML content")
}

fn load_p2p_private_key(private_key_path: &PathBuf) -> identity::Keypair {
    // Note for later: translate RSA PEM key with:
    // openssl pkcs8 -topk8 -inform PEM -outform DER -in node-secret.key -out node-secret.pkcs8.der -nocrypt

    // let private_key_path = std::path::Path::new(private_key_file);
    let mut private_key_bytes =
        std::fs::read(private_key_path).expect("could not load private key file");
    let rsa_keypair = identity::rsa::Keypair::from_pkcs8(private_key_bytes.as_mut())
        .expect("could not decode private key");

    identity::Keypair::Rsa(rsa_keypair)
}

async fn dial_bootstrap_peers(network_client: &mut P2PClient, peers: &Vec<Multiaddr>) {
    for peer_addr in peers.iter() {
        let mut addr = peer_addr.clone();
        let last_protocol = addr.pop();
        let peer_id = match last_protocol {
            Some(Protocol::P2p(hash)) => PeerId::from_multihash(hash).expect("valid hash"),
            _ => {
                error!("Bootstrap peer multiaddr must end with its peer ID (/p2p/<peer-id>.");
                continue;
            }
        };
        match network_client.dial(peer_id, addr).await {
            Ok(_) => info!("Successfully dialed bootstrap peer: {}", &peer_addr),
            Err(e) => error!("Failed to dial bootstrap peer {}: {}", peer_addr, e),
        }
    }
}

async fn subscribe_to_topics(network_client: &mut P2PClient, topics: &Vec<String>) {
    for topic in topics {
        let topic = gossipsub::IdentTopic::new(topic);
        network_client
            .subscribe(&topic)
            .await
            .expect(&format!("subscription to {} should succeed", topic));
    }
}

async fn publish_message(network_client: &mut P2PClient, delivery: &Delivery) {
    let topic = gossipsub::IdentTopic::new(delivery.routing_key.as_str());
    let publish_result = network_client
        .publish(&topic, &delivery.data)
        .await;

    if let Err(e) = publish_result {
        error!("Could not publish to P2P topic {}: {}", topic, e);
    }
}

async fn forward_p2p_message(mq_client: &mut RabbitMqClient, message: GossipsubMessage) {
    match message.source {
        None => {
            warn!("Received pubsub message from an unspecified sender. Discarding.");
            return;
        }
        Some(peer_id) => {
            let routing_key = format!("{}.{}.{}", "p2p", message.topic, peer_id);
            mq_client.publish(&routing_key, &message.data).await.unwrap();
        }
    }
}

async fn p2p_loop(
    mut network_events: impl StreamExt<Item=p2p::network::Event> + std::marker::Unpin,
    mut network_client: P2PClient,
    mut mq_client: RabbitMqClient,
) {
    loop {
        tokio::select! {
            network_event = network_events.next() => {
                match network_event {
                    Some(p2p::network::Event::PubsubMessage {
                        propagation_source: _,
                        message_id: _,
                        message,
                    }) => {
                        forward_p2p_message(&mut mq_client, message).await;
                    }
                    None => {
                        error!("Event loop stopped");
                        break;
                    }
                }
            }
            message_to_publish = mq_client.next() => {
                if let Some(Ok(delivery)) = message_to_publish {
                    publish_message(&mut network_client, &delivery).await;
                    delivery.ack(BasicAckOptions::default()).await.expect("ack");
                }
            }
        }
    }
}

pub struct AppState {
    pub app_config: config::AppConfig,
    pub p2p_client: std::sync::Mutex<P2PClient>,
    pub peer_id: PeerId,
}

#[actix_web::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let args = CliArgs::parse();
    debug!("CLI args: {:?}", args);

    let id_keys = load_p2p_private_key(&args.private_key_file);
    let peer_id = PeerId::from(id_keys.public());
    info!("Peer ID: {:?}", peer_id);

    let app_config = read_config(&args.config);
    debug!("Config: {:?}", app_config);

    let (mut network_client, network_events, network_event_loop) =
        p2p::network::new(id_keys).await?;

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

    // Subscribe to topics
    subscribe_to_topics(&mut network_client, &app_config.p2p.topics).await;

    // Create RabbitMQ exchanges/queues
    let mq_client = message_queue::new(&app_config).await?;

    let p2p_handle = tokio::spawn(p2p_loop(network_events, network_client.clone(), mq_client));

    let app_data = Data::new(AppState {
        app_config: app_config.clone(),
        p2p_client: std::sync::Mutex::new(network_client.clone()),
        peer_id,
    });

    let http_server_bind_address = format!("0.0.0.0:{}", &app_config.p2p.control_port.0);
    let http_server = HttpServer::new(move || {
        App::new()
            .app_data(Data::clone(&app_data))
            // enable logger - always register actix-web Logger middleware last
            .wrap(middleware::Logger::default())
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
    p2p_event_loop_handle.abort();
    let _ = p2p_event_loop_handle.await;

    p2p_handle.abort();
    let _ = p2p_handle.await;

    Ok(())
}
