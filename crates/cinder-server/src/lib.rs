//! The Cinder playground backend: a small HTTP API around the `cinder` compiler,
//! plus the static front end, with a layered sandbox for running user programs.
//!
//! * `GET  /api/health`
//! * `POST /api/compile` `{ "code", "optLevel": 0|1|2, "emit": "asm"|"ir"|"ast"|"hir"|"pp" }`
//! * `POST /api/run`     `{ "code", "optLevel", "stdin" }`
//!
//! See [`sandbox`] for how programs are confined and [`config`] for the settings.

pub mod api;
pub mod config;
pub mod output;
pub mod ratelimit;
pub mod sandbox;

use axum::extract::{DefaultBodyLimit, Request};
use axum::http::{header, HeaderValue};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;
use std::time::Instant;
use tower_http::services::{ServeDir, ServeFile};

const CSP: &str = "default-src 'self'; script-src 'self' https://cdn.jsdelivr.net; \
style-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; font-src 'self' https://cdn.jsdelivr.net data:; \
img-src 'self' data:; worker-src 'self' blob: data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Security headers on every response and one log line per API request.
async fn headers_and_log(req: Request, next: Next) -> Response {
    let started = Instant::now();
    let (method, path) = (req.method().clone(), req.uri().path().to_string());
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    if path.starts_with("/api/") {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        eprintln!("{} {} {} {}ms", method, path, resp.status().as_u16(), started.elapsed().as_millis());
    }
    resp
}

pub fn router(state: Arc<api::AppState>) -> Router {
    let web = state.cfg.web_dir.clone();
    let body_limit = state.cfg.max_code_bytes + state.cfg.max_stdin_bytes + 8192;
    let files = ServeDir::new(&web).not_found_service(ServeFile::new(web.join("index.html")));
    Router::new()
        .route("/api/health", get(api::health))
        .route("/api/compile", post(api::compile))
        .route("/api/run", post(api::run))
        .fallback_service(files)
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(middleware::from_fn(headers_and_log))
        .with_state(state)
}
