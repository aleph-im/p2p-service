use actix_web::{web, HttpResponse, Responder};
use libp2p::{Multiaddr, PeerId};
use log::{error, warn};
use serde::Deserialize;

use crate::http::endpoints::error::EndpointError;
use crate::http::AppState;
use crate::p2p::network::{classify_dial_error, DialErrorKind};

#[derive(Deserialize, Debug)]
pub struct DialRequest {
    multiaddr: Multiaddr,
    peer_id: PeerId,
}

fn handle_dial_error(
    error: Box<dyn std::error::Error + Send>,
    peer_id: PeerId,
    multiaddr: &Multiaddr,
) -> EndpointError {
    match classify_dial_error(error.as_ref()) {
        DialErrorKind::WrongPeerId => {
            warn!(
                "Wrong peer ID dialing {:?} with multiaddr {:?}: {}",
                peer_id, multiaddr, error
            );
            EndpointError::Forbidden
        }
        DialErrorKind::Unreachable => {
            warn!(
                "Failed to dial {:?} with multiaddr {:?}: {}",
                peer_id, multiaddr, error
            );
            EndpointError::NotFound
        }
        DialErrorKind::Internal => {
            error!(
                "Failed to dial {:?} with multiaddr {:?}: {}",
                peer_id, multiaddr, error
            );
            EndpointError::InternalError
        }
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
