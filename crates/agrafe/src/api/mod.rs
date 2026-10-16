//! HTTP interfaces. `private` is unsigned and trusted; `public` verifies a
//! signature on every route before touching the database.

pub mod json;
pub mod private;
pub mod public;
pub mod signed;
pub mod urls;

use axum::Router;
use axum::routing::get;
use tower_http::trace::TraceLayer;

use crate::app::AppState;

pub fn private_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .merge(private::routes())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub fn public_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .merge(public::routes())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}
