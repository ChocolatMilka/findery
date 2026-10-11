//! # findery-server — layer L3
//!
//! The HTTP transport. It parses, authorizes, calls exactly one [`Indexer`]
//! method, and serializes the answer.
//!
//! INTENT: no orchestration lives here. If a handler ever needs to decide
//! something, that decision belongs in L2 — otherwise the MCP transport and the
//! CLI will each grow their own subtly different copy of it.

pub mod dto;
pub mod routes;

use axum::Json;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use findery_core::Error;
use findery_core::config::Config;
use findery_index::Indexer;
use findery_index::watch::WatchHandle;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

/// Everything a handler can reach.
#[derive(Clone)]
pub struct AppState {
    pub indexer: Arc<Indexer>,
    /// Fires when a client asks the daemon to stop.
    ///
    /// INTENT: a daemon started in the background has no terminal to Ctrl-C and
    /// no pid anyone recorded. Asking it to leave over its own API is the only
    /// way that works identically on every platform.
    pub shutdown: Arc<tokio::sync::Notify>,
    /// Live watchers, one per project. Dropping a handle stops its task.
    pub watches: Arc<Mutex<HashMap<String, WatchHandle>>>,
    pub token: Option<Arc<str>>,
}

impl AppState {
    pub fn new(indexer: Arc<Indexer>) -> Self {
        let token = indexer.cfg.server.api_token.clone().map(Arc::from);
        Self {
            indexer,
            watches: Arc::new(Mutex::new(HashMap::new())),
            token,
            shutdown: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

/// Builds the router with logging, body limits and optional bearer auth.
pub fn app(state: AppState) -> axum::Router {
    let limit = state.indexer.cfg.server.max_body_bytes;
    routes::router()
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authorize,
        ))
        .layer(RequestBodyLimitLayer::new(limit))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Rejects requests without the configured bearer token.
///
/// `/healthz` stays open: a container health check must not need a secret.
async fn authorize(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = state.token.as_deref() else {
        return next.run(request).await;
    };
    if request.uri().path() == "/healthz" {
        return next.run(request).await;
    }
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(token) if constant_time_eq(token, expected) => next.run(request).await,
        _ => ApiError(Error::Invalid("missing or invalid bearer token".into()))
            .with_status(StatusCode::UNAUTHORIZED)
            .into_response(),
    }
}

/// Compares without leaking the position of the first difference through timing.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Wrapper turning a domain error into an HTTP response.
///
/// The status comes from `Error::http_status`, which lives in L0: one mapping,
/// shared by every route, impossible to drift.
pub struct ApiError(pub Error);

impl ApiError {
    fn with_status(self, status: StatusCode) -> (StatusCode, Json<dto::ErrorBody>) {
        let body = dto::ErrorBody {
            code: self.0.code().to_string(),
            // The payload, not the rendered message: the client rebuilds the
            // variant from `code` and renders it once (see `Error::detail`).
            message: self.0.detail(),
        };
        (status, Json(body))
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        Self(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        if status.is_server_error() {
            tracing::error!(error = %self.0, "request failed");
        } else {
            tracing::debug!(error = %self.0, "request rejected");
        }
        self.with_status(status).into_response()
    }
}

/// Result alias used by every handler.
pub type ApiResult<T> = std::result::Result<T, ApiError>;

/// Boots an indexer from a configuration and wraps it in application state.
pub async fn state_from_config(cfg: Config) -> findery_core::Result<AppState> {
    let indexer = Indexer::new(cfg).await?;
    Ok(AppState::new(indexer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison_is_length_safe() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn errors_carry_their_status_and_code() {
        let resp = ApiError(Error::NotFound("project `x`".into())).into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = ApiError(Error::upstream("embedder", "down")).into_response();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        let resp = ApiError(Error::Internal("boom".into())).into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
