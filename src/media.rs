//! 媒体交付：默认 302 让浏览器直连 CDN，失败时用 `?p=1` 走流式转发。
//!
//! 两条路都透传 Range，浏览器才能边下边续。

use std::time::Duration;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use url::Url;

use crate::config::DeliveryMode;
use crate::error::AppError;
use crate::AppState;

const REFERER: &str = "https://www.douyin.com/";

#[derive(Debug, Deserialize)]
pub struct DownloadQuery {
    /// 目标直链，必须是白名单域名。
    pub u: String,
    /// 下载文件名，留空用 clipkeep.mp4。
    pub n: Option<String>,
    /// 1 = 强制走服务端中转。
    #[serde(default)]
    pub p: Option<u8>,
}

pub async fn download(
    State(state): State<AppState>,
    Query(query): Query<DownloadQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let target = Url::parse(&query.u).map_err(|_| AppError::BadRequest("下载地址不合法".into()))?;
    if !matches!(target.scheme(), "http" | "https") {
        return Err(AppError::BadRequest("只支持 http/https".into()));
    }
    let host = target.host_str().unwrap_or_default();
    if !state.config.host_allowed(host) {
        return Err(AppError::BadRequest("不支持的下载域名".into()));
    }

    let filename = sanitize_filename(query.n.as_deref());
    let force_proxy = query.p == Some(1);
    let use_proxy = force_proxy || state.config.delivery == DeliveryMode::Proxy;

    if !use_proxy {
        // 零带宽路径：让浏览器自己去拿，进度、续传都交给浏览器。
        let mut response = Redirect::temporary(target.as_str()).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return Ok(response);
    }

    proxy(&state, &target, &filename, &headers).await
}

async fn proxy(
    state: &AppState,
    target: &Url,
    filename: &str,
    client_headers: &HeaderMap,
) -> Result<Response, AppError> {
    let mut request = state
        .media
        .get(target.as_str())
        .header(header::USER_AGENT, crate::USER_AGENT)
        .header(header::REFERER, REFERER)
        .header(header::ACCEPT, "*/*");

    // Range 必须原样透传，否则浏览器拿不到 206，断点续传失效。
    if let Some(range) = client_headers.get(header::RANGE) {
        request = request.header(header::RANGE, range);
    }
    if let Some(if_range) = client_headers.get(header::IF_RANGE) {
        request = request.header(header::IF_RANGE, if_range);
    }

    let upstream = request.send().await?;
    let status = upstream.status();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(AppError::Upstream("直链已失效，请重新解析".into()));
    }
    if status.is_client_error() || status.is_server_error() {
        return Err(AppError::Upstream(format!(
            "上游返回 HTTP {}",
            status.as_u16()
        )));
    }

    let mut builder = Response::builder().status(status);
    {
        let out = builder
            .headers_mut()
            .ok_or_else(|| AppError::Internal("构造响应头失败".to_string()))?;
        for name in [
            header::CONTENT_TYPE,
            header::CONTENT_LENGTH,
            header::CONTENT_RANGE,
            header::ACCEPT_RANGES,
            header::ETAG,
            header::LAST_MODIFIED,
        ] {
            if let Some(value) = upstream.headers().get(&name) {
                out.insert(name, value.clone());
            }
        }
        if let Ok(value) = HeaderValue::from_str(&content_disposition(filename)) {
            out.insert(header::CONTENT_DISPOSITION, value);
        }
        out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }

    builder
        .body(Body::from_stream(upstream.bytes_stream()))
        .map_err(|error| AppError::Internal(error.to_string()))
}

/// 文件名的非 ASCII 部分按 RFC 5987 编码，中文才不会变成乱码。
fn content_disposition(filename: &str) -> String {
    let ascii_fallback: String = filename
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let fallback = if ascii_fallback.trim_matches(['.', '_', '-']).is_empty() {
        "clipkeep.mp4".to_string()
    } else {
        ascii_fallback
    };
    format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
        percent_encode(filename)
    )
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn sanitize_filename(raw: Option<&str>) -> String {
    let name = raw.unwrap_or("").trim();
    let name = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut cleaned: String = name
        .chars()
        .filter(|ch| !ch.is_control() && *ch != '"')
        .take(120)
        .collect();
    if cleaned.trim().is_empty() {
        return "clipkeep.mp4".to_string();
    }
    if !cleaned.contains('.') {
        cleaned.push_str(".mp4");
    }
    cleaned
}

/// 媒体流不设总超时，只限制连接建立。
pub fn media_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(3600))
        .pool_max_idle_per_host(8)
        .build()
        .expect("reqwest client 构建失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_is_sanitized() {
        assert_eq!(sanitize_filename(None), "clipkeep.mp4");
        assert_eq!(sanitize_filename(Some("  ")), "clipkeep.mp4");
        // 没有扩展名时按视频补 .mp4，浏览器下载才有正确的文件类型。
        assert_eq!(sanitize_filename(Some("../../etc/passwd")), "passwd.mp4");
        assert_eq!(sanitize_filename(Some("视频")), "视频.mp4");
        assert_eq!(sanitize_filename(Some("a.mp4")), "a.mp4");
    }

    #[test]
    fn disposition_encodes_chinese() {
        let value = content_disposition("视频 1.mp4");
        assert!(value.starts_with("attachment;"));
        assert!(value.contains("filename*=UTF-8''"));
        assert!(value.contains("%E8%A7%86%E9%A2%91"));
        // ASCII 回退名里不能出现引号或非 ASCII。
        assert!(value.contains("filename=\""));
    }
}
