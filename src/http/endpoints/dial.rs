use actix_web::{HttpResponse, Responder, web};
use libp2p::{Multiaddr, PeerId};
use libp2p::swarm::DialError;
use log::{error, warn};
use serde::Deserialize;

use crate::AppState;
use crate::http::endpoints::error::EndpointError;

#[derive(Deserialize, Debug)]
pub struct DialRequest {
    multiaddr: Multiaddr,
    peer_id: PeerId,
}

fn handle_dial_error(error: Box<dyn std::error::Error + Send>, peer_id: PeerId, multiaddr: Multiaddr) -> EndpointError {
    if let Some(dial_error) = error.downcast_ref::<DialError>() {
        match dial_error {
            DialError::WrongPeerId { obtained, endpoint: _ } => {
                warn!(
                    "Wrong peer ID: obtained {:?} - user specified {:?}",
                    obtained, peer_id
                );
                EndpointError::Forbidden
            }
            _ => {
                warn!(
                    "Failed to dial {:?} with multiaddr {:?}: {:?}",
                    peer_id, multiaddr, error
                );
                EndpointError::NotFound
            }
        }
    } else {
        error!("Failed to dial {:?} with multiaddr {:?}: {:?}", peer_id, multiaddr, error);
        EndpointError::InternalError
    }
}

pub async fn dial(
    app_state: web::Data<AppState>,
    dial_request: web::Json<DialRequest>,
) -> Result<impl Responder, actix_web::Error> {
    let DialRequest { peer_id, multiaddr } = dial_request.0;
    let mut p2p_client = app_state.p2p_client.lock().await;
    p2p_client.dial(peer_id, multiaddr.clone()).await.map_err(|e| handle_dial_error(e, peer_id, multiaddr))?;

    Ok(HttpResponse::Ok())
}
