use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

/// An error that maps directly onto an HTTP response.
#[derive(Debug, Clone)]
pub struct AppError {
    pub status: StatusCode,
    pub msg: String,
}

impl AppError {
    pub fn new(status: StatusCode, msg: impl Into<String>) -> Self {
        Self { status, msg: msg.into() }
    }
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg)
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, msg)
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, msg)
    }
    pub fn unprocessable(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, msg)
    }
    pub fn bad_gateway(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, msg)
    }
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, msg)
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status.as_u16(), self.msg)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let mut res = (self.status, self.msg).into_response();
        let h = res.headers_mut();
        h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        if self.status == StatusCode::SERVICE_UNAVAILABLE {
            h.insert(header::RETRY_AFTER, "1".parse().unwrap());
        }
        res
    }
}
