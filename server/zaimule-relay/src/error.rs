use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("service non configuré")]
    ServiceUnavailable,
    #[error("clé d'accès invalide ou absente")]
    Unauthorized,
    #[error("trop de connexions actives")]
    TooManyConnections,
}

impl IntoResponse for RelayError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            Self::ServiceUnavailable => (StatusCode::SERVICE_UNAVAILABLE, "relay_unconfigured"),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::TooManyConnections => (StatusCode::TOO_MANY_REQUESTS, "too_many_connections"),
        };
        (
            status,
            Json(json!({
                "error": code,
                "message": self.to_string(),
            })),
        )
            .into_response()
    }
}
