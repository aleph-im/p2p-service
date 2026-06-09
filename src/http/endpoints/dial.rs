use actix_web::{web, HttpResponse, Responder};
use libp2p::swarm::DialError;
use libp2p::{Multiaddr, PeerId};
use log::{error, warn};
use serde::Deserialize;

use crate::http::endpoints::error::EndpointError;
use crate::http::AppState;
use crate::p2p::network::DialFailed;

#[derive(Deserialize, Debug)]
pub struct DialRequest {
    multiaddr: Multiaddr,
    peer_id: PeerId,
}

fn map_dial_error(dial_error: &DialError, peer_id: PeerId, multiaddr: &Multiaddr) -> EndpointError {
    match dial_error {
        DialError::WrongPeerId { obtained, .. } => {
            warn!(
                "Wrong peer ID: obtained {:?} - user specified {:?}",
                obtained, peer_id
            );
            EndpointError::Forbidden
        }
        _ => {
            warn!(
                "Failed to dial {:?} with multiaddr {:?}: {:?}",
                peer_id, multiaddr, dial_error
            );
            EndpointError::NotFound
        }
    }
}

fn handle_dial_error(
    error: Box<dyn std::error::Error + Send>,
    peer_id: PeerId,
    multiaddr: &Multiaddr,
) -> EndpointError {
    // Dial failures can surface either as an immediate `DialError` (the swarm
    // rejected the dial) or as a `DialFailed` reported by the event loop once
    // the connection attempt fails.
    if let Some(dial_failed) = error.downcast_ref::<DialFailed>() {
        map_dial_error(dial_failed.dial_error(), peer_id, multiaddr)
    } else if let Some(dial_error) = error.downcast_ref::<DialError>() {
        map_dial_error(dial_error, peer_id, multiaddr)
    } else {
        error!(
            "Failed to dial {:?} with multiaddr {:?}: {:?}",
            peer_id, multiaddr, error
        );
        EndpointError::InternalError
    }
}

pub async fn dial(
    app_state: web::Data<AppState>,
    dial_request: web::Json<DialRequest>,
) -> Result<impl Responder, actix_web::Error> {
    let DialRequest { peer_id, multiaddr } = dial_request.0;

    // The client is a cheap channel handle; clone it for this request.
    let mut client = app_state.p2p_client.clone();
    client
        .dial_and_wait(peer_id, multiaddr.clone())
        .await
        .map_err(|dial_error| handle_dial_error(dial_error, peer_id, &multiaddr))?;

    Ok(HttpResponse::Ok())
}
