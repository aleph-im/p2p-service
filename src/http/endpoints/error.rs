use std::fmt::{Display, Formatter};
use actix_web::http::StatusCode;

#[derive(Debug)]
pub enum EndpointError {
    InternalError,
}

impl Display for EndpointError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            EndpointError::InternalError => "internal error",
        };
        write!(f, "{}", s)
    }
}

impl std::error::Error for EndpointError {}

impl actix_web::ResponseError for EndpointError {
    fn status_code(&self) -> StatusCode {
        match self {
            Self::InternalError => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}
