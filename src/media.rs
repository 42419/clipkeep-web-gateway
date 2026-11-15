//! 媒体交付：默认 302 让浏览器直连 CDN，失败时用 `?p=1` 走流式转发。
//!
//! 两条路都透传 Range，浏览器才能边下边续。
//!
//! 另有 `/img`：给网页端显示封面、头像用的同源图片中转，见 [`image`]。

use std::sync::OnceLock;
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

/// `/img` 单张图片的体积上限。封面、头像都是几十到几百 KB，超过这个值多半不是图片。
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// `/img` 的整体超时；图片很小，不需要像视频那样放宽到一小时。
const IMAGE_TIMEOUT: Duration = Duration::from_secs(20);

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

#[derive(Debug, Deserialize)]
pub struct ImageQuery {
    /// 图片直链，必须是白名单域名。
    pub u: String,
}

/// 同源图片中转：网页端显示封面、头像用。
///
/// 为什么需要它：Flutter web 的 `Image.network` 只要带了 `headers` 就会改用 XHR 拉字节，
/// 这条路要求 CDN 返回 CORS 头；而 `user-agent` / `referer` 是浏览器禁止脚本设置的请求头，
/// 带了也会被丢掉，抖音图床的防盗链就没人处理。Edge、Android 浏览器碰巧能过，
/// iOS Safari 上整批图片都加载失败。改走同源 `/img` 后，浏览器看到的是同源图片，
/// Referer 由服务端补，不依赖 CDN 的 CORS 行为。
///
/// 与 `/dl` 的区别：不加 `Content-Disposition: attachment`，要让浏览器内联显示；
/// 整张读进内存并限制体积，不做流式转发，也不转码。
pub async fn image(
    State(state): State<AppState>,
    Query(query): Query<ImageQuery>,
) -> Result<Response, AppError> {
    let target = Url::parse(&query.u).map_err(|_| AppError::BadRequest("图片地址不合法".into()))?;
    if !matches!(target.scheme(), "http" | "https") {
        return Err(AppError::BadRequest("只支持 http/https".into()));
    }
    let host = target.host_str().unwrap_or_default();
    if !state.config.host_allowed(host) {
        return Err(AppError::BadRequest("不支持的图片域名".into()));
    }

    let mut upstream = image_client()
        .get(target.as_str())
        .header(header::USER_AGENT, crate::USER_AGENT)
        .header(header::REFERER, REFERER)
        .header(header::ACCEPT, "image/*,*/*;q=0.8")
        .send()
        .await?;

    let status = upstream.status();
    if status.is_redirection() {
        // 客户端禁了自动跟随（见 image_client），这里显式拒绝：
        // 跟随的话 302 的目标不受 host_allowed 约束，等于开了个内网探针。
        return Err(AppError::Upstream(format!(
            "图片地址要求跳转（HTTP {}），已拒绝",
            status.as_u16()
        )));
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(AppError::Upstream("图片链接已失效，请重新解析".into()));
    }
    if !status.is_success() {
        return Err(AppError::Upstream(format!(
            "上游返回 HTTP {}",
            status.as_u16()
        )));
    }
    if upstream
        .content_length()
        .is_some_and(|len| len > MAX_IMAGE_BYTES as u64)
    {
        return Err(AppError::Upstream("图片过大".into()));
    }

    let declared = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    // 没有 Content-Length 时（分块传输）边读边数，超限立刻放弃。
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = upstream.chunk().await? {
        if body.len() + chunk.len() > MAX_IMAGE_BYTES {
            return Err(AppError::Upstream("图片过大".into()));
        }
        body.extend_from_slice(&chunk);
    }

    let content_type = resolve_image_type(declared.as_deref(), &body)
        .ok_or_else(|| AppError::Upstream("上游返回的不是图片".into()))?;

    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    // 带登录态的接口，缓存只允许浏览器私有缓存；签名直链本身会过期，缓存时间别太长。
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    Ok(response)
}

/// 决定回给浏览器的 Content-Type。
///
/// 全站开了 `nosniff`，类型不对浏览器就不渲染，所以不能照抄上游：
/// CDN 偶尔会回 `application/octet-stream`，这时按文件头判断。
/// 既不是图片声明、文件头也认不出来的，当作失败。
fn resolve_image_type(declared: Option<&str>, body: &[u8]) -> Option<&'static str> {
    const KNOWN: [&str; 6] = [
        "image/jpeg",
        "image/png",
        "image/webp",
        "image/gif",
        "image/avif",
        "image/heic",
    ];
    if let Some(declared) = declared {
        let essence = declared
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        // image/jpg 之类的非标准写法统一成 image/jpeg。
        let essence = if essence == "image/jpg" {
            "image/jpeg".to_string()
        } else {
            essence
        };
        if let Some(known) = KNOWN.iter().find(|item| **item == essence) {
            return Some(known);
        }
    }
    sniff_image_type(body)
}

fn sniff_image_type(body: &[u8]) -> Option<&'static str> {
    if body.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if body.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some("image/png");
    }
    if body.starts_with(b"GIF8") {
        return Some("image/gif");
    }
    if body.len() >= 12 && &body[0..4] == b"RIFF" && &body[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if body.len() >= 12 && &body[4..8] == b"ftyp" {
        return match &body[8..12] {
            b"avif" | b"avis" => Some("image/avif"),
            b"heic" | b"heix" | b"hevc" | b"heim" | b"heis" | b"mif1" | b"msf1" => {
                Some("image/heic")
            }
            _ => None,
        };
    }
    None
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

/// `/img` 专用客户端：**不跟随重定向**。
///
/// `/dl` 的转发依赖跟随 CDN 的 302，所以共享 client 保持默认策略；图片没有这个需求，
/// 而跟随重定向会绕过 `host_allowed` —— 上游只要 302 到内网地址，服务端就会照着去拉，
/// 攻击者可以通过响应差异探测内网端口。这里建成独立 client，等待首个请求时才初始化。
fn image_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(IMAGE_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .pool_max_idle_per_host(8)
            .build()
            .expect("image client 构建失败")
    })
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
    fn image_type_trusts_declared_image_types() {
        assert_eq!(
            resolve_image_type(Some("image/webp"), b"junk"),
            Some("image/webp")
        );
        assert_eq!(
            resolve_image_type(Some("image/JPEG; charset=binary"), b"junk"),
            Some("image/jpeg")
        );
        assert_eq!(
            resolve_image_type(Some("image/jpg"), b"junk"),
            Some("image/jpeg")
        );
    }

    #[test]
    fn image_type_falls_back_to_sniffing() {
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 0];
        assert_eq!(
            resolve_image_type(Some("application/octet-stream"), &jpeg),
            Some("image/jpeg")
        );
        assert_eq!(resolve_image_type(None, &jpeg), Some("image/jpeg"));

        let mut webp = b"RIFF\0\0\0\0WEBP".to_vec();
        webp.extend_from_slice(b"VP8 ");
        assert_eq!(resolve_image_type(None, &webp), Some("image/webp"));

        let mut heic = vec![0, 0, 0, 0x18];
        heic.extend_from_slice(b"ftypheic");
        assert_eq!(resolve_image_type(None, &heic), Some("image/heic"));
    }

    #[test]
    fn image_type_rejects_non_images() {
        // 防盗链页面、JSON 错误体都不能被当成图片放行。
        assert_eq!(
            resolve_image_type(Some("text/html"), b"<html>403</html>"),
            None
        );
        assert_eq!(
            resolve_image_type(Some("application/json"), b"{\"code\":1}"),
            None
        );
        assert_eq!(resolve_image_type(None, b""), None);
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
