use crate::http::endpoints::error::EndpointError;
use crate::http::AppState;
use crate::p2p::network::NodeInfo;
use actix_web::web;
use libp2p::{Multiaddr, PeerId};
use serde::Serialize;

#[derive(Serialize, Debug)]
pub struct IdentifyResponse {
    pub peer_id: PeerId,
    pub multiaddrs: Vec<Multiaddr>,
}

impl From<NodeInfo> for IdentifyResponse {
    fn from(node_info: NodeInfo) -> Self {
        // Keep the historical JSON shape: a single `multiaddrs` field,
        // external addresses first, then listen addresses.
        let mut multiaddrs = node_info.external_multiaddrs;
        multiaddrs.extend(node_info.listen_multiaddrs);
        Self {
            peer_id: node_info.peer_id,
            multiaddrs,
        }
    }
}

pub async fn identify(
    app_state: web::Data<AppState>,
) -> Result<web::Json<IdentifyResponse>, actix_web::Error> {
    let node_info = {
        let mut p2p_client = app_state.p2p_client.lock().await;
        p2p_client.identify().await
    }
    .map_err(|_identify_error| EndpointError::ServiceUnavailable)?;

    Ok(web::Json(node_info.into()))
}
