//! 端到端路由测试：起一个假 DTK，走真实 HTTP，不 mock reqwest。

use std::path::PathBuf;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use clipkeep_gateway::config::{Config, DeliveryMode};
use clipkeep_gateway::{build_router, AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// 假 DTK：解析返回带 media 的内容信封，任务查询返回同名内容。
async fn spawn_stub_dtk() -> String {
    let app = Router::new()
        .route(
            "/api/v1/parse",
            post(|Json(body): Json<Value>| async move {
                Json(json!({
                    "success": true,
                    "data": {
                        "content_id": "7316684387616099599",
                        "kind": "video",
                        "title": format!("stub {}", body["url"].as_str().unwrap_or("")),
                        "media": { "video": { "url": "https://v.douyin.com/x.mp4" } }
                    }
                }))
            }),
        )
        .route(
            "/api/v1/tasks/{id}",
            get(|| async {
                Json(json!({
                    "success": true,
                    "data": { "content_id": "1", "media": {} }
                }))
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn test_config(dtk_base: String) -> Config {
    Config {
        bind: "127.0.0.1:0".parse().expect("bind"),
        dtk_base,
        dtk_key: "test-key".into(),
        site_token: "test-token".into(),
        sign_secret: "test-secret".into(),
        allowed_hosts: vec!["douyin.com".into()],
        static_dir: PathBuf::from(tempfile::tempdir().expect("tempdir").path()),
        delivery: DeliveryMode::Redirect,
        parse_rate_per_min: 1000,
        session_ttl: Duration::from_secs(3600),
        trust_proxy: false,
        cookie_secure: false,
        dtk_timeout: Duration::from_secs(10),
    }
}

async fn build_app() -> Router {
    let base = spawn_stub_dtk().await;
    let state = AppState::new(test_config(base)).expect("state");
    build_router(state)
}

async fn call(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.clone().oneshot(request).await.expect("request");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, headers, String::from_utf8_lossy(&bytes).to_string())
}

async fn login(app: &Router) -> String {
    let request = Request::builder()
        .method("POST")
        .uri("/api/session")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "token": "test-token" }).to_string()))
        .expect("login request");

    let (status, headers, _) = call(app, request).await;
    assert_eq!(status, StatusCode::OK, "口令正确时应登录成功");
    let cookie = headers
        .get(header::SET_COOKIE)
        .expect("Set-Cookie")
        .to_str()
        .expect("cookie str");
    cookie.split(';').next().expect("cookie pair").to_string()
}

fn parse_request(token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/parse")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = token {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder
        .body(Body::from(
            json!({ "url": "https://v.douyin.com/abc123/" }).to_string(),
        ))
        .expect("parse request")
}

#[tokio::test]
async fn healthz_is_public() {
    let app = build_app().await;
    let request = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .expect("request");
    let (status, _, body) = call(&app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"ok\":true"));
}

#[tokio::test]
async fn parse_requires_session() {
    let app = build_app().await;
    let (status, _, body) = call(&app, parse_request(None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("口令"));
}

#[tokio::test]
async fn wrong_token_is_rejected() {
    let app = build_app().await;
    let request = Request::builder()
        .method("POST")
        .uri("/api/session")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "token": "nope" }).to_string()))
        .expect("request");
    let (status, _, _) = call(&app, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn parse_passes_envelope_through() {
    let app = build_app().await;
    let cookie = login(&app).await;

    let (status, _, body) = call(&app, parse_request(Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);

    let value: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["success"], json!(true));
    // 原样透传：content_id 必须还在 data 里，前端 mapper 才认得。
    assert_eq!(value["data"]["content_id"], json!("7316684387616099599"));
    assert!(value["data"]["media"].is_object());
}

#[tokio::test]
async fn parse_rejects_non_douyin() {
    let app = build_app().await;
    let cookie = login(&app).await;

    let request = Request::builder()
        .method("POST")
        .uri("/api/parse")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, &cookie)
        .body(Body::from(
            json!({ "url": "https://example.com/video/1" }).to_string(),
        ))
        .expect("request");

    let (status, _, body) = call(&app, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("抖音"));
}

#[tokio::test]
async fn download_redirects_to_cdn() {
    let app = build_app().await;
    let cookie = login(&app).await;

    let request = Request::builder()
        .uri("/dl?u=https%3A%2F%2Fv.douyin.com%2Fx.mp4&n=a.mp4")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .expect("request");

    let (status, headers, _) = call(&app, request).await;
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        headers
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok()),
        Some("https://v.douyin.com/x.mp4")
    );
}

#[tokio::test]
async fn download_rejects_foreign_host() {
    let app = build_app().await;
    let cookie = login(&app).await;

    let request = Request::builder()
        .uri("/dl?u=https%3A%2F%2Fevil.example%2Fx.mp4")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .expect("request");

    let (status, _, body) = call(&app, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("域名"));
}

#[tokio::test]
async fn download_requires_session() {
    let app = build_app().await;
    let request = Request::builder()
        .uri("/dl?u=https%3A%2F%2Fv.douyin.com%2Fx.mp4")
        .body(Body::empty())
        .expect("request");

    let (status, _, _) = call(&app, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn session_status_reflects_cookie() {
    let app = build_app().await;

    let anonymous = Request::builder()
        .uri("/api/session")
        .body(Body::empty())
        .expect("request");
    let (status, _, body) = call(&app, anonymous).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"authenticated\":false"));

    let cookie = login(&app).await;
    let authed = Request::builder()
        .uri("/api/session")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .expect("request");
    let (_, _, body) = call(&app, authed).await;
    assert!(body.contains("\"authenticated\":true"));
}
