//! ClipKeep 网页版网关。
//!
//! 三件事：把 `/api/*` 透传给 DTK（注入 Key）、托管 Flutter web 静态产物、
//! 提供 `/dl` 让浏览器自己把视频拿下去。不含用户体系、不落盘、不转码。

pub mod auth;
pub mod bundle;
pub mod config;
pub mod dtk;
pub mod error;
pub mod media;
pub mod ratelimit;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, Extension, Request, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::dtk::DtkClient;
use crate::error::AppError;
use crate::ratelimit::RateLimiter;

pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub dtk: Arc<DtkClient>,
    pub media: reqwest::Client,
    pub limiter: Arc<RateLimiter>,
}

impl AppState {
    pub fn new(config: Config) -> Result<Self, reqwest::Error> {
        // 重启后上一轮的打包目录已经没人会下载，直接清掉，避免磁盘越积越多。
        bundle::purge_all(&config.bundle_tmp);

        let dtk = DtkClient::new(&config)?;
        let media = media::media_client();
        Ok(Self {
            config: Arc::new(config),
            dtk: Arc::new(dtk),
            media,
            limiter: Arc::new(RateLimiter::new()),
        })
    }
}

/// 由中间件填进 extensions，供 handler 取用（测试里没有真实连接信息也能工作）。
#[derive(Clone, Copy, Debug)]
pub struct ClientIp(pub IpAddr);

pub fn build_router(state: AppState) -> Router {
    let protected = Router::new()
        // 同时挂 /api/v1/*：Flutter 端的 DtkClient 直连 DTK 时就是打这个前缀，
        // 客户端代码不用为 web 单独改路径。
        .route("/api/parse", post(parse))
        .route("/api/v1/parse", post(parse))
        .route("/api/tasks/{id}", get(task))
        .route("/api/v1/tasks/{id}", get(task))
        .route("/api/bundle", post(bundle::bundle))
        .route("/api/bundle/{token}", get(bundle::bundle_download))
        .route("/dl", get(media::download))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_session,
        ));

    let public = Router::new().route("/healthz", get(healthz)).route(
        "/api/session",
        get(session_status).post(login).delete(logout),
    );

    Router::new()
        .merge(public)
        .merge(protected)
        .fallback_service({
            // Flutter web 只有单页；找不到的资源回 index.html，避免刷新 404。
            let dir = &state.config.static_dir;
            ServeDir::new(dir).not_found_service(ServeFile::new(dir.join("index.html")))
        })
        .layer(middleware::from_fn_with_state(
            state.clone(),
            inject_client_ip,
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        // 让浏览器在跳转 CDN 时不带本站 Referer，减少被防盗链拦的概率。
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

// ---------- 中间件 ----------

async fn inject_client_ip(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let ip = resolve_ip(&state, &req);
    req.extensions_mut().insert(ClientIp(ip));
    next.run(req).await
}

fn resolve_ip(state: &AppState, req: &Request) -> IpAddr {
    if state.config.trust_proxy {
        if let Some(raw) = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
        {
            if let Some(first) = raw.split(',').next() {
                if let Ok(ip) = first.trim().parse() {
                    return ip;
                }
            }
        }
    }
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip())
        .unwrap_or(IpAddr::from([127, 0, 0, 1]))
}

async fn require_session(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let header_value = req
        .headers()
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();

    let session = auth::read_cookie(header_value, auth::COOKIE_NAME)
        .ok_or(AppError::Unauthorized("请先输入访问口令"))?;
    let remaining = auth::verify_session(&state.config.sign_secret, session)
        .ok_or(AppError::Unauthorized("登录已过期，请重新输入口令"))?;

    let mut response = next.run(req).await;

    // 滑动续期：过了一半有效期就重发一张，长期使用不用反复输口令。
    if remaining < state.config.session_ttl.as_secs() / 2 {
        if let Some((value, max_age)) =
            auth::issue_session(&state.config.sign_secret, state.config.session_ttl)
        {
            if let Ok(cookie) = HeaderValue::from_str(&auth::session_cookie(
                &value,
                max_age,
                state.config.cookie_secure,
            )) {
                response.headers_mut().append(header::SET_COOKIE, cookie);
            }
        }
    }
    Ok(response)
}

// ---------- handlers ----------

async fn healthz() -> Json<Value> {
    // 带版本号，方便 curl 一下就知道线上跑的是哪个构建。
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    token: String,
}

async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> Result<Response, AppError> {
    if !auth::verify_secret(body.token.trim(), &state.config.site_token) {
        // 口令错误也走限流，避免被离线枚举。
        return Err(AppError::Unauthorized("口令不正确"));
    }
    let (value, max_age) = auth::issue_session(&state.config.sign_secret, state.config.session_ttl)
        .ok_or_else(|| AppError::Internal("签发会话失败".to_string()))?;
    let cookie = HeaderValue::from_str(&auth::session_cookie(
        &value,
        max_age,
        state.config.cookie_secure,
    ))
    .map_err(|error| AppError::Internal(error.to_string()))?;

    let mut response = Json(json!({ "ok": true })).into_response();
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(response)
}

async fn logout(State(state): State<AppState>) -> Response {
    let mut response = Json(json!({ "ok": true })).into_response();
    if let Ok(cookie) = HeaderValue::from_str(&auth::clear_cookie(state.config.cookie_secure)) {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

async fn session_status(State(state): State<AppState>, headers: HeaderMap) -> Json<Value> {
    let authenticated = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|header_value| auth::read_cookie(header_value, auth::COOKIE_NAME))
        .and_then(|session| auth::verify_session(&state.config.sign_secret, session))
        .is_some();
    Json(json!({ "authenticated": authenticated }))
}

#[derive(Debug, Deserialize)]
struct ParseRequest {
    url: String,
}

async fn parse(
    State(state): State<AppState>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Json(body): Json<ParseRequest>,
) -> Result<Json<Value>, AppError> {
    let normalized = normalize_input(&body.url);
    if normalized.is_empty() {
        return Err(AppError::BadRequest("请提供抖音链接".to_string()));
    }
    if !is_douyin(&normalized) {
        return Err(AppError::BadRequest("目前仅支持抖音链接".to_string()));
    }
    if !state
        .limiter
        .allow(ip, state.config.parse_rate_per_min, Duration::from_secs(60))
    {
        return Err(AppError::TooManyRequests(
            "请求过于频繁，请稍后再试".to_string(),
        ));
    }

    let envelope = state.dtk.parse(&normalized).await?;
    Ok(Json(envelope))
}

async fn task(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, AppError> {
    let envelope = state.dtk.task(&id).await?;
    Ok(Json(envelope))
}

// ---------- 输入归一化 ----------

/// 与 Android 端一致：从分享文案里抠出第一个 http(s) 链接，纯数字当作品 ID。
pub fn normalize_input(raw: &str) -> String {
    let text = raw.trim();
    if text.is_empty() {
        return String::new();
    }
    if let Some(start) = text.find("http://").or_else(|| text.find("https://")) {
        let tail = &text[start..];
        // 中文标点不是 whitespace，分享文案里「链接，后面的字」很常见，
        // 只按空白截断会把整段话当成 URL。
        let end = tail
            .char_indices()
            .find(|(_, ch)| ends_url(*ch))
            .map(|(index, _)| index)
            .unwrap_or(tail.len());
        return tail[..end]
            .trim_end_matches([
                ',', ')', '.', ';', '!', '?', '，', '。', '；', '！', '？', '、',
            ])
            .to_string();
    }
    if text.len() >= 15 && text.chars().all(|ch| ch.is_ascii_digit()) {
        return format!("https://www.douyin.com/video/{text}");
    }
    text.to_string()
}

fn is_douyin(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("douyin.com") || lower.contains("iesdouyin.com")
}

/// URL 在分享文案里的自然终点：空白或中文标点。
fn ends_url(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '，' | '。' | '；' | '！' | '？' | '、' | '）' | '】' | '》' | '」' | '』'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_share_text() {
        let raw = "7.68 复制打开抖音，看看【某人的作品】 https://v.douyin.com/abc123/ 复制此链接";
        assert_eq!(normalize_input(raw), "https://v.douyin.com/abc123/");
    }

    #[test]
    fn normalizes_bare_id() {
        assert_eq!(
            normalize_input("7316684387616099599"),
            "https://www.douyin.com/video/7316684387616099599"
        );
    }

    #[test]
    fn strips_trailing_punctuation() {
        assert_eq!(
            normalize_input("看这个 https://v.douyin.com/x1/，不错"),
            "https://v.douyin.com/x1/"
        );
    }

    #[test]
    fn rejects_non_douyin() {
        assert!(!is_douyin("https://example.com/video/1"));
        assert!(is_douyin("https://www.iesdouyin.com/share/video/1"));
    }
}
