use std::sync::Arc;

use axum::{extract::DefaultBodyLimit, http::StatusCode, middleware::from_fn_with_state, response::IntoResponse, routing::{get, post}, Router, Json};
use serde_json::json;

use crate::{admin_auth, admin_routes, auth, state::StarlinkRouterState, user_routes};

pub fn build_router(state: Arc<StarlinkRouterState>) -> Router {
    let user = Router::new()
        .route("/v1/models", get(user_routes::models))
        .route("/v1/chat/completions", post(user_routes::chat_completions))
        .route("/v1/assets", post(user_routes::assets_upload).layer(DefaultBodyLimit::max(46 * 1024 * 1024)))
        .route("/v1/videos/generations", post(user_routes::video_generations))
        .route("/v1/videos/:task_id", get(user_routes::video_task))
        .route("/v1/videos/:task_id/content", get(user_routes::video_content))
        .layer(from_fn_with_state(state.clone(), auth::user_auth));
    let admin = admin_routes::router()
        .layer(from_fn_with_state(state.clone(), admin_auth::require_admin));
    let public_admin = admin_routes::public_router()
        .route("/v1/assets/:asset_id/content", get(user_routes::assets_content));
    Router::new()
        .route("/health", get(health))
        .route("/healthz", get(health))
        .route("/admin", get(admin_routes::page))
        .route("/admin/", get(admin_routes::page))
        .merge(public_admin)
        .merge(user)
        .merge(admin)
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({"status": "ok", "service": "星链维度分流系统"})))
}

#[cfg(test)]
mod tests {
    use super::health;
    use axum::response::IntoResponse;

    #[tokio::test]
    async fn health_is_local_and_public() {
        assert_eq!(health().await.into_response().status(), axum::http::StatusCode::OK);
    }
}
