use actix_web::{Responder, web};
use serde::Serialize;
use crate::AppState;

#[derive(Serialize, Debug)]
struct IdentifyResponse {
    pub peer_id: String,
}

pub async fn identify(app_state: web::Data<AppState>) -> impl Responder {
    let response = IdentifyResponse {
        peer_id: app_state.peer_id.to_string(),
    };

    web::Json(response)
}
