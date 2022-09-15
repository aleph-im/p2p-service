use actix_web::web;

pub mod endpoints;

pub fn config(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api").service(
            web::scope("/p2p")
                .route("/identify", web::get().to(endpoints::identity::identify))
                .route("/dial", web::post().to(endpoints::dial::dial)),
        ),
    );
}
