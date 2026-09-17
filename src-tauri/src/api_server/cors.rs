//! 可选的浏览器 CORS 适配。
//!
//! 默认不发送 CORS 允许头，桌面/CLI 客户端不受影响。仅当网关设置里配置了
//! 逗号分隔的来源时，才允许匹配的 Origin，并对预检请求返回空 204。

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::ApiSharedState;

fn allowed(origin: &str, configured: &str) -> bool {
    configured
        .split(',')
        .map(str::trim)
        .any(|item| item == "*" || item.eq_ignore_ascii_case(origin))
}

pub async fn headers(
    axum::extract::State(state): axum::extract::State<Arc<ApiSharedState>>,
    request: Request,
    next: Next,
) -> Response {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let is_allowed = origin
        .as_deref()
        .map(|value| allowed(value, &state.cors_origins))
        .unwrap_or(false);
    if request.method() == Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        if is_allowed {
            apply(&mut response, origin.as_deref().unwrap_or(""));
        }
        return response;
    }
    let mut response = next.run(request).await;
    if is_allowed {
        apply(&mut response, origin.as_deref().unwrap_or(""));
    }
    response
}

fn apply(response: &mut Response, origin: &str) {
    if let Ok(value) = HeaderValue::from_str(origin) {
        response.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
    response.headers_mut().insert(header::VARY, HeaderValue::from_static("Origin"));
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET,POST,OPTIONS"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Authorization,Content-Type,X-API-Key,Idempotency-Key,X-Conversation-ID"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("Content-Type,Request-Id"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_configured_origins() {
        assert!(allowed("http://localhost:3000", "http://localhost:3000, https://app.example"));
        assert!(!allowed("https://evil.example", "http://localhost:3000"));
        assert!(allowed("https://anything.example", "*"));
    }
}
