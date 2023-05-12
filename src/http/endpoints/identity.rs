use crate::http::endpoints::error::EndpointError;
use crate::p2p::network::NodeInfo;
use crate::AppState;
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
        Self {
            peer_id: node_info.peer_id,
            multiaddrs: node_info.multiaddrs,
        }
    }
}

pub async fn identify(
    app_state: web::Data<AppState>,
) -> Result<web::Json<IdentifyResponse>, actix_web::Error> {
    let mut p2p_client = app_state.p2p_client.lock().await;
    let node_info = p2p_client
        .identify()
        .await
        .map_err(|_| EndpointError::InternalError)?;
    Ok(web::Json(node_info.into()))
}
