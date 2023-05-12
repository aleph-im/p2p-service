use actix_web::http::StatusCode;
use std::fmt::{Display, Formatter};

#[derive(Debug)]
pub enum EndpointError {
    Forbidden,
    NotFound,
    InternalError,
}

impl Display for EndpointError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Forbidden => "forbidden",
            Self::NotFound => "not found",
            Self::InternalError => "internal error",
        };
        write!(f, "{s}")
    }
}

impl std::error::Error for EndpointError {}

impl actix_web::ResponseError for EndpointError {
    fn status_code(&self) -> StatusCode {
        match self {
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::InternalError => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}
