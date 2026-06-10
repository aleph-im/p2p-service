use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use actix_web::web::Data;
use actix_web::{middleware, App, HttpServer};
use clap::Parser;
use libp2p::multiaddr::Protocol;
use libp2p::{gossipsub, identity, Multiaddr, PeerId};
use log::{debug, error, info, warn};

use aleph_p2p_service::config::AppConfig;
use aleph_p2p_service::http::AppState;
use aleph_p2p_service::metrics::Metrics;
use aleph_p2p_service::p2p::network::{NetworkSettings, P2PClient};
use aleph_p2p_service::p2p::peerstore::PeerStore;
use aleph_p2p_service::subscriptions::Subscriptions;
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

/// Extract the PeerId from each bootstrap peer multiaddr (trailing /p2p/<id> component).
fn bootstrap_peer_ids(peers: &[Multiaddr]) -> std::collections::HashSet<PeerId> {
    peers
        .iter()
        .filter_map(p2p::maintenance::split_peer_multiaddr)
        .map(|(id, _)| id)
        .collect()
}

/// Reject configurations that the connection-limit machinery cannot honour.
fn validate_config(app_config: &AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let p2p = &app_config.p2p;
    if p2p.low_water > p2p.high_water {
        return Err(format!(
            "Invalid p2p config: low_water ({}) must not exceed high_water ({})",
            p2p.low_water, p2p.high_water
        )
        .into());
    }
    if p2p.per_subnet_cap == 0 {
        return Err("Invalid p2p config: per_subnet_cap must be at least 1".into());
    }
    if !(0.0..=1.0).contains(&p2p.max_protected_share) {
        return Err(format!(
            "Invalid p2p config: max_protected_share ({}) must be between 0.0 and 1.0",
            p2p.max_protected_share
        )
        .into());
    }
    if p2p.maintenance_interval_secs == 0 {
        return Err(
            "Invalid p2p config: maintenance_interval_secs must be at least 1 (0 hot-spins the command channel)".into(),
        );
    }
    Ok(())
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
    validate_config(&app_config)?;

    let _sentry_guard = configure_sentry(&app_config);

    let id_keys = load_p2p_private_key(&args.private_key_file);
    let peer_id = PeerId::from(id_keys.public());
    info!("Peer ID: {:?}", peer_id);

    let metrics = Metrics::new();

    let subscriptions = Arc::new(Subscriptions::new(1024));

    let peerstore_path = app_config.p2p.peerstore_path.clone();
    let peerstore = Arc::new(std::sync::Mutex::new(PeerStore::load(&peerstore_path)));
    info!(
        "Loaded peerstore from {:?}: {} known peers",
        peerstore_path,
        peerstore.lock().expect("peerstore lock poisoned").len()
    );

    let network_settings = NetworkSettings {
        low_water: app_config.p2p.low_water,
        high_water: app_config.p2p.high_water,
        per_subnet_cap: app_config.p2p.per_subnet_cap,
        max_protected_share: app_config.p2p.max_protected_share,
    };
    let bootstrap_peers = bootstrap_peer_ids(&app_config.p2p.peers);

    let (mut network_client, network_event_loop) = p2p::network::new(
        id_keys,
        network_settings,
        bootstrap_peers,
        metrics.clone(),
        subscriptions.clone(),
        peerstore.clone(),
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

    // Subscribe to topics
    subscribe_to_topics(&mut network_client, &topics).await?;

    // Mesh maintenance: dials bootstrap peers right away on its first pass
    // (startup no longer blocks on bootstrap dialing) and keeps the node
    // connected from then on.
    let maintenance_handle = tokio::spawn(p2p::maintenance::run(
        network_client.clone(),
        app_config.p2p.peers.clone(),
        p2p::maintenance::MaintenanceSettings {
            interval: Duration::from_secs(app_config.p2p.maintenance_interval_secs),
            low_water: app_config.p2p.low_water,
        },
        peerstore.clone(),
    ));

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

    // Periodically flush the peerstore to disk.
    // save() does file I/O while the lock is held; this is acceptable because
    // the file is small and the only contenders are synchronous record() calls.
    let peerstore_flush = peerstore.clone();
    let peerstore_flush_path = peerstore_path.clone();
    let peerstore_flush_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(300));
        // The first tick fires immediately; skip it so we do not write on startup.
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(e) = peerstore_flush
                .lock()
                .expect("peerstore lock poisoned")
                .save(&peerstore_flush_path)
            {
                warn!("Failed to flush peerstore: {}", e);
            }
        }
    });

    let app_data = Data::new(AppState {
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
    // Signals are handled by the supervision select below; letting actix
    // install its own handlers would race ours.
    .disable_signals()
    .bind(http_server_bind_address)
    .expect("bind should succeed");

    info!("HTTP server listening on: {:?}", http_server.addrs());

    // `run()` is called outside the spawned task: the `HttpServer` factory is
    // not `Send`, but the `Server` future it returns is.
    let http_server_future = http_server.run();
    let http_server_handle = tokio::spawn(async move {
        if let Err(e) = http_server_future.await {
            error!("Metrics HTTP server stopped: {:?}", e);
        }
    });

    // Supervision: every spawned task is critical. If any of them ends, log
    // which one died, flush the peerstore and exit non-zero so the container
    // runtime restarts the service. SIGINT/SIGTERM trigger a clean shutdown.
    let abort_handles = [
        p2p_event_loop_handle.abort_handle(),
        grpc_handle.abort_handle(),
        maintenance_handle.abort_handle(),
        peerstore_flush_handle.abort_handle(),
        http_server_handle.abort_handle(),
    ];

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing the SIGTERM handler should succeed");

    let exit_code = tokio::select! {
        _ = p2p_event_loop_handle => { error!("Critical task 'p2p-event-loop' ended"); 1 }
        _ = grpc_handle => { error!("Critical task 'grpc-server' ended"); 1 }
        _ = maintenance_handle => { error!("Critical task 'maintenance' ended"); 1 }
        _ = peerstore_flush_handle => { error!("Critical task 'peerstore-flusher' ended"); 1 }
        _ = http_server_handle => { error!("Critical task 'metrics-http' ended"); 1 }
        _ = tokio::signal::ctrl_c() => { info!("Shutdown signal received (SIGINT)"); 0 }
        _ = sigterm.recv() => { info!("Shutdown signal received (SIGTERM)"); 0 }
    };

    for handle in abort_handles {
        handle.abort();
    }

    // Final peerstore flush on shutdown.
    if let Err(e) = peerstore
        .lock()
        .expect("peerstore lock poisoned")
        .save(&peerstore_path)
    {
        warn!("Failed to save peerstore on shutdown: {}", e);
    }

    std::process::exit(exit_code);
}
