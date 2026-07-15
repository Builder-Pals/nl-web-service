use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("invalid asset id")]
    InvalidId,
    #[error("authentication required")]
    Unauthorized,
    #[error("asset is not an eligible Roblox-authored model")]
    Forbidden,
    #[error("game must be an uncopylocked root place created before 2018")]
    IneligibleGame,
    #[error("asset was not found")]
    NotFound,
    #[error("model exceeds the 20 MB limit")]
    TooLarge,
    #[error("invalid binary Roblox model: {0}")]
    InvalidModel(String),
    #[error("Roblox rate limit exceeded")]
    RateLimited,
    #[error("Roblox request timed out")]
    Timeout,
    #[error("Roblox service failure: {0}")]
    Upstream(String),
    #[error("internal service failure")]
    Internal(#[from] anyhow::Error),
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorDetail<'a>,
}
#[derive(Serialize)]
struct ErrorDetail<'a> {
    code: &'a str,
    message: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            Self::InvalidId => (StatusCode::BAD_REQUEST, "invalid_asset_id"),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "ineligible_asset"),
            Self::IneligibleGame => (StatusCode::FORBIDDEN, "ineligible_game"),
            Self::NotFound => (StatusCode::NOT_FOUND, "asset_not_found"),
            Self::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "asset_too_large"),
            Self::InvalidModel(_) => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_rbxm"),
            Self::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "roblox_rate_limited"),
            Self::Timeout => (StatusCode::GATEWAY_TIMEOUT, "roblox_timeout"),
            Self::Upstream(_) => (StatusCode::BAD_GATEWAY, "roblox_failure"),
            Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        };
        let message = if matches!(self, Self::Internal(_)) {
            "internal service failure".into()
        } else {
            self.to_string()
        };
        (
            status,
            Json(ErrorBody {
                error: ErrorDetail { code, message },
            }),
        )
            .into_response()
    }
}
