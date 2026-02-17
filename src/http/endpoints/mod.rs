pub mod dial;
mod error;
pub mod identity;

use actix_web::{web, HttpResponse, Result};
use prometheus_client::encoding::text::encode;
use crate::AppState;

pub async fn metrics(data: web::Data<AppState>) -> Result<HttpResponse> {
    data.metrics.http_requests_total.inc();
    data.metrics.update_memory_usage();

    let mut buffer = String::new();
    if let Err(e) = encode(&mut buffer, &data.metrics.registry) {
        return Ok(HttpResponse::InternalServerError().body(format!("Failed to encode metrics: {}", e)));
    }

    Ok(HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4; charset=utf-8")
        .body(buffer))
}

pub async fn health(data: web::Data<AppState>) -> Result<HttpResponse> {
    data.metrics.http_requests_total.inc();

    let health_status = serde_json::json!({
        "status": "healthy",
        "peer_id": data.peer_id.to_string(),
        "timestamp": chrono::Utc::now().to_rfc3339()
    });

    Ok(HttpResponse::Ok().json(health_status))
}
