use actix_web::web;
use libp2p::PeerId;

use crate::metrics::Metrics;

pub mod endpoints;

pub struct AppState {
    pub peer_id: PeerId,
    pub metrics: Metrics,
}

pub fn config(cfg: &mut web::ServiceConfig) {
    cfg.route("/metrics", web::get().to(endpoints::metrics))
        .route("/health", web::get().to(endpoints::health));
}
