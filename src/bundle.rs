//! 多文件打包下载。
//!
//! 浏览器会在连续触发多个下载时拦截，用户只拿到第一个文件；打成一个 zip
//! 只触发一次下载，包内文件名也不会被浏览器改成 `xxx(1).jpg`。
//!
//! 代价是必须落盘：下载到临时目录 → 压缩 → 流式回传 → 延迟清理。

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path as FsPath, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::Response;
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::error::AppError;
use crate::AppState;

/// 一次最多打多少个文件。
const MAX_FILES: usize = 50;
/// 整个包的上限。
const MAX_TOTAL_BYTES: u64 = 500 * 1024 * 1024;
/// 单个文件的上限，防止某个地址是无限流。
const MAX_ONE_BYTES: u64 = 200 * 1024 * 1024;
/// 单文件下载超时。
const PER_FILE_SECS: u64 = 60;
/// 整体超时，与 nginx 的 `proxy_read_timeout` 对齐。
const OVERALL_SECS: u64 = 180;
/// 超过这个时间的残留目录会被惰性清掉。
const STALE_SECS: u64 = 600;
/// 响应发出后多久删除临时目录（要给客户端留出读流的时间）。
const CLEANUP_DELAY_SECS: u64 = 120;

#[derive(Deserialize)]
pub struct BundleRequest {
    /// 压缩包名，不含扩展名。
    #[serde(default)]
    pub name: Option<String>,
    pub files: Vec<BundleFile>,
}

#[derive(Deserialize)]
pub struct BundleFile {
    /// 文件直链，只允许 `ALLOWED_HOSTS` 内的域名。
    pub u: String,
    /// 包内文件名。
    #[serde(default)]
    pub n: Option<String>,
}

#[derive(Serialize)]
pub struct BundleResult {
    /// 取包凭证，形如 `bundle-1234-567890`。
    pub token: String,
    /// 成功打包的文件数。
    pub saved: usize,
    /// 失败的文件与原因，前端据此提示用户。
    pub skipped: Vec<String>,
}

pub async fn bundle(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<BundleResult>, AppError> {
    let request = parse_request(&headers, &body)?;
    let files = request.files;

    if files.is_empty() {
        return Err(AppError::BadRequest("没有要打包的文件".into()));
    }
    if files.len() > MAX_FILES {
        return Err(AppError::BadRequest(format!("一次最多 {MAX_FILES} 个文件")));
    }

    // 先把地址全部校验完，避免下到一半才发现非法。
    for file in &files {
        check_url(&state, &file.u)?;
    }

    let root = state.config.bundle_tmp.clone();
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|error| AppError::Internal(format!("创建临时目录失败：{error}")))?;
    cleanup_stale(&root);

    let dir = root.join(format!("bundle-{}-{}", std::process::id(), stamp()));
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|error| AppError::Internal(format!("创建任务目录失败：{error}")))?;

    let collected = tokio::time::timeout(
        Duration::from_secs(OVERALL_SECS),
        collect(&state, &dir, &files),
    )
    .await;

    let (files, skipped) = match collected {
        Ok(result) => result,
        Err(_) => {
            schedule_cleanup(dir);
            return Err(AppError::Upstream("打包超时，请减少文件数量".into()));
        }
    };

    if files.is_empty() {
        schedule_cleanup(dir);
        return Err(AppError::Upstream("所有文件都下载失败".into()));
    }

    let zip_path = dir.join("bundle.zip");
    let zip_target = zip_path.clone();
    let zip_files = files.clone();
    let zip_skipped = skipped.clone();
    tokio::task::spawn_blocking(move || write_zip(&zip_target, &zip_files, &zip_skipped))
        .await
        .map_err(|error| AppError::Internal(format!("压缩任务异常：{error}")))?
        .map_err(|error| AppError::Internal(format!("压缩失败：{error}")))?;

    // 不在这里直接回传 zip：前端需要先知道成功几个、失败几个。
    // 打包结果放在临时目录，由 `/api/bundle/{token}` 取走。
    let token = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string();
    schedule_cleanup(dir);

    Ok(Json(BundleResult {
        token,
        saved: files.len(),
        skipped,
    }))
}

/// 把打包好的 zip 交给浏览器；取过就删，避免临时目录长期堆积。
pub async fn bundle_download(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, AppError> {
    if !valid_token(&token) {
        return Err(AppError::BadRequest("打包编号不合法".into()));
    }

    let dir = state.config.bundle_tmp.join(&token);
    let zip_path = dir.join("bundle.zip");
    if !zip_path.is_file() {
        return Err(AppError::NotFound("打包结果已过期，请重新点击保存".into()));
    }

    let handle = tokio::fs::File::open(&zip_path)
        .await
        .map_err(|error| AppError::Internal(format!("读取压缩包失败：{error}")))?;

    schedule_cleanup(dir);

    let name = sanitize_name(params.get("n").map(String::as_str).unwrap_or("ClipKeep"));
    let disposition = content_disposition(&name);
    let stream = ReaderStream::new(handle);

    Response::builder()
        .header(header::CONTENT_TYPE, "application/zip")
        .header(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&disposition)
                .map_err(|error| AppError::Internal(format!("响应头非法：{error}")))?,
        )
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .map_err(|error| AppError::Internal(format!("构建响应失败：{error}")))
}

// ---------- 内部 ----------

/// 浏览器无法用 XHR 触发「带 Content-Disposition 的下载」，前端改用隐藏表单提交，
/// 所以这里同时接受 JSON 与 `application/x-www-form-urlencoded`（`payload` 字段）。
fn parse_request(headers: &HeaderMap, body: &[u8]) -> Result<BundleRequest, AppError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json");

    if content_type.contains("application/x-www-form-urlencoded") {
        let text = String::from_utf8_lossy(body);
        let mut payload = None;
        for (key, value) in url::form_urlencoded::parse(text.as_bytes()) {
            if key == "payload" {
                payload = Some(value.into_owned());
            }
        }
        let payload = payload.ok_or_else(|| AppError::BadRequest("缺少 payload 字段".into()))?;
        return serde_json::from_str(&payload)
            .map_err(|error| AppError::BadRequest(format!("payload 解析失败：{error}")));
    }

    serde_json::from_slice(body)
        .map_err(|error| AppError::BadRequest(format!("请求体解析失败：{error}")))
}

fn check_url(state: &AppState, raw: &str) -> Result<(), AppError> {
    let url = url::Url::parse(raw).map_err(|_| AppError::BadRequest("文件地址不合法".into()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(AppError::BadRequest("只支持 http(s) 地址".into()));
    }
    let host = url.host_str().unwrap_or_default();
    if !state.config.host_allowed(host) {
        return Err(AppError::BadRequest(format!("域名不在允许列表：{host}")));
    }
    Ok(())
}

/// 逐个下载，失败的进跳过名单而不是整体失败。
async fn collect(
    state: &AppState,
    dir: &FsPath,
    files: &[BundleFile],
) -> (Vec<(PathBuf, String)>, Vec<String>) {
    let mut saved: Vec<(PathBuf, String)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut total = 0u64;

    for (index, file) in files.iter().enumerate() {
        let fallback = format!("file_{}", index + 1);
        let raw_name = file
            .n
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(&fallback);
        let name = uniquify(&saved, sanitize_name(raw_name));

        match fetch_one(state, dir, &file.u, &name, total).await {
            Ok(size) => {
                total += size;
                saved.push((dir.join(&name), name));
            }
            Err(reason) => skipped.push(format!("{name}：{reason}")),
        }

        if total >= MAX_TOTAL_BYTES {
            skipped.push("已达总大小上限，剩余文件未处理".to_string());
            break;
        }
    }
    (saved, skipped)
}

async fn fetch_one(
    state: &AppState,
    dir: &FsPath,
    url: &str,
    name: &str,
    used: u64,
) -> Result<u64, String> {
    let request = state
        .media
        .get(url)
        .header(header::REFERER, "https://www.douyin.com/")
        .header(header::USER_AGENT, crate::USER_AGENT);

    let response = tokio::time::timeout(Duration::from_secs(PER_FILE_SECS), request.send())
        .await
        .map_err(|_| "请求超时".to_string())?
        .map_err(|error| format!("请求失败：{error}"))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status().as_u16()));
    }

    let path = dir.join(name);
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(|error| format!("写入失败：{error}"))?;

    let mut written = 0u64;
    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("读取中断：{error}"))?
    {
        written += chunk.len() as u64;
        if written > MAX_ONE_BYTES || used + written > MAX_TOTAL_BYTES {
            drop(file);
            let _ = tokio::fs::remove_file(&path).await;
            return Err("超过大小上限".to_string());
        }
        file.write_all(&chunk)
            .await
            .map_err(|error| format!("写入失败：{error}"))?;
    }
    let _ = file.flush().await;

    if written == 0 {
        let _ = tokio::fs::remove_file(&path).await;
        return Err("内容为空".to_string());
    }
    Ok(written)
}

fn write_zip(
    target: &FsPath,
    files: &[(PathBuf, String)],
    skipped: &[String],
) -> std::io::Result<()> {
    let handle = std::fs::File::create(target)?;
    let mut zip = zip::ZipWriter::new(handle);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);

    for (path, name) in files {
        zip.start_file(name.as_str(), options)?;
        let mut source = std::fs::File::open(path)?;
        std::io::copy(&mut source, &mut zip)?;
    }

    if !skipped.is_empty() {
        zip.start_file("_skipped.txt", options)?;
        let mut text = String::from("以下文件未能打包：\n\n");
        for line in skipped {
            text.push_str(line);
            text.push('\n');
        }
        zip.write_all(text.as_bytes())?;
    }

    zip.finish()?;
    Ok(())
}

fn schedule_cleanup(dir: PathBuf) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(CLEANUP_DELAY_SECS)).await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
    });
}

/// 惰性清理：每次请求顺手扫掉超时的残留目录，防止异常退出后越积越多。
fn cleanup_stale(root: &FsPath) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        if now
            .duration_since(modified)
            .map(|age| age.as_secs() > STALE_SECS)
            .unwrap_or(false)
        {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// token 只可能是 `bundle-<pid>-<纳秒>`；收紧格式以杜绝路径穿越。
fn valid_token(token: &str) -> bool {
    token.starts_with("bundle-")
        && token.len() < 64
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'-')
}

fn stamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0)
}

/// 去掉路径分隔符等危险字符，避免写出目录或覆盖 zip 内部结构。
fn sanitize_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            control if control.is_control() => '_',
            other => other,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        return "clipkeep".to_string();
    }
    trimmed.chars().take(120).collect()
}

fn uniquify(existing: &[(PathBuf, String)], name: String) -> String {
    if !existing.iter().any(|(_, used)| used == &name) {
        return name;
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((left, right)) => (left.to_string(), format!(".{right}")),
        None => (name.clone(), String::new()),
    };
    for index in 2..1000 {
        let candidate = format!("{stem}_{index}{extension}");
        if !existing.iter().any(|(_, used)| used == &candidate) {
            return candidate;
        }
    }
    name
}

fn content_disposition(name: &str) -> String {
    // `filename=` 只能是 ASCII；中文走 `filename*=UTF-8''`。
    let ascii: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii() && !ch.is_ascii_control() {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String =
        url::form_urlencoded::byte_serialize(format!("{name}.zip").as_bytes()).collect();
    format!("attachment; filename=\"{ascii}.zip\"; filename*=UTF-8''{encoded}")
}
