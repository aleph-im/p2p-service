use actix_web::web;
use libp2p::PeerId;

use crate::config::AppConfig;
use crate::metrics::Metrics;
use crate::p2p::network::P2PClient;

pub mod endpoints;

pub struct AppState {
    pub app_config: AppConfig,
    pub p2p_client: P2PClient,
    pub peer_id: PeerId,
    pub metrics: Metrics,
}

pub fn config(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api").service(
            web::scope("/p2p")
                .route("/identify", web::get().to(endpoints::identity::identify))
                .route("/dial", web::post().to(endpoints::dial::dial)),
        ),
    )
    .route("/metrics", web::get().to(endpoints::metrics))
    .route("/health", web::get().to(endpoints::health));
}
