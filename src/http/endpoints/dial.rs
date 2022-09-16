use actix_web::{web, HttpResponse, Responder};
use libp2p::swarm::DialError;
use libp2p::{Multiaddr, PeerId};
use log::{error, warn};
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
    match app_state.p2p_client.lock() {
        Ok(mut p2p_client) => {
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

        Err(e) => {
            error!("Could not load P2P client: {:?}", e);
            HttpResponse::InternalServerError().into()
        }
    }
}
