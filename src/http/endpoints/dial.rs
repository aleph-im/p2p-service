use actix_web::{HttpResponse, Responder, web};
use libp2p::{Multiaddr, PeerId};
use libp2p::swarm::DialError;
use log::warn;
use serde::Deserialize;

use crate::AppState;

#[derive(Deserialize, Debug)]
pub struct DialRequest {
    multiaddr: Multiaddr,
    peer_id: PeerId,
}

pub async fn dial(
    app_state: web::Data<AppState>,
    dial_request: web::Json<DialRequest>,
) -> impl Responder {
    let DialRequest { peer_id, multiaddr } = dial_request.0;
    let mut p2p_client = app_state.p2p_client.lock().await;
    if let Err(err) = p2p_client.dial(peer_id, multiaddr.clone()).await {
        if let Some(dial_err) = err.downcast_ref::<DialError>() {
            return if let DialError::WrongPeerId {
                obtained,
                endpoint: _endpoint,
            } = dial_err
            {
                warn!(
                    "Wrong peer ID: obtained {:?} - user specified {:?}",
                    obtained, peer_id
                );
                HttpResponse::Forbidden().body("Wrong peer ID")
            } else {
                warn!(
                    "Failed to dial {:?} with multiaddr {:?}: {:?}",
                    peer_id, multiaddr, err
                );
                HttpResponse::NotFound().body(format!(
                    "Could not find peer {} with multiaddr {}",
                    peer_id, multiaddr
                ))
            };
        }
    }
    HttpResponse::Ok().into()
}
