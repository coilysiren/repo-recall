use axum::routing::{get, post};
use axum::Router;
use tower_http::services::ServeDir;

use crate::AppState;

pub mod dashboard;
pub mod fallback;
pub mod refresh;
pub mod repos;
pub mod search;
pub mod sessions;
pub mod templates;
pub mod ws;

pub fn router(state: AppState) -> Router {
    let static_dir = std::env::var("REPO_RECALL_STATIC")
        .ok()
        .unwrap_or_else(|| format!("{}/static", env!("CARGO_MANIFEST_DIR")));
    Router::new()
        .route("/", get(dashboard::index))
        .route("/repos/{id}", get(repos::detail))
        .route("/sessions/{id}", get(sessions::detail))
        .route("/search", get(search::search))
        .route("/refresh", post(refresh::trigger))
        .route("/ws", get(ws::ws_handler))
        .route("/livereload", get(ws::livereload_handler))
        .nest_service("/static", ServeDir::new(static_dir))
        .fallback(fallback::not_found)
        .with_state(state)
}
