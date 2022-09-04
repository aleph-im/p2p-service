use actix_web::{get, Responder, web};

#[get("/hello/{name}")]
pub async fn say_hello(name: web::Path<String>) -> impl Responder {
    format!("Hello {name}!")
}
