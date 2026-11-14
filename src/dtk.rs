//! DTK 透传。
//!
//! 刻意不做字段映射：DTK 的 `{success, data, error}` 原样回给前端，
//! 这样 Flutter 侧现成的 `dtk_mapper` 可以一行不改地继续用。

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;

use crate::config::Config;
use crate::error::AppError;

pub struct DtkClient {
    http: reqwest::Client,
    base: String,
    key: String,
}

impl DtkClient {
    pub fn new(config: &Config) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(config.dtk_timeout)
            .user_agent(crate::USER_AGENT)
            .build()?;
        Ok(Self {
            http,
            base: config.dtk_base.clone(),
            key: config.dtk_key.clone(),
        })
    }

    /// 解析链接；`wait=20` 超时后 DTK 会回任务对象，这里补两次轮询。
    pub async fn parse(&self, url: &str) -> Result<Value, AppError> {
        let body = self
            .post(
                "/api/v1/parse",
                &[("wait", "20"), ("lang", "zh")],
                serde_json::json!({ "url": url, "include_raw": true }),
            )
            .await?;

        let mut envelope = body;
        for _ in 0..2 {
            ensure_success(&envelope)?;
            let data = envelope.get("data").cloned().unwrap_or(Value::Null);
            if !data.is_object() {
                return Err(AppError::Upstream("解析结果为空".into()));
            }
            // 带 media / content_id 的才是内容，其余是任务对象。
            if data.get("media").is_some() || data.get("content_id").is_some() {
                finalize(&mut envelope);
                return Ok(envelope);
            }
            let Some(task_id) = data.get("task_id").and_then(Value::as_str) else {
                finalize(&mut envelope);
                return Ok(envelope);
            };
            envelope = self
                .get(
                    &format!("/api/v1/tasks/{task_id}"),
                    &[("wait", "20"), ("lang", "zh")],
                )
                .await?;
        }
        ensure_success(&envelope)?;
        finalize(&mut envelope);
        Ok(envelope)
    }

    /// 透传任务查询，前端自己判断状态。
    pub async fn task(&self, id: &str) -> Result<Value, AppError> {
        self.get(
            &format!("/api/v1/tasks/{id}"),
            &[("wait", "20"), ("lang", "zh")],
        )
        .await
    }

    async fn post(
        &self,
        path: &str,
        query: &[(&str, &str)],
        body: Value,
    ) -> Result<Value, AppError> {
        let request = self
            .http
            .post(format!("{}{path}", self.base))
            .query(query)
            .header("X-API-Key", &self.key)
            .header("Accept", "application/json")
            .json(&body);

        self.send(request).await
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, AppError> {
        let request = self
            .http
            .get(format!("{}{path}", self.base))
            .query(query)
            .header("X-API-Key", &self.key)
            .header("Accept", "application/json");

        self.send(request).await
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value, AppError> {
        let response = request.send().await?;
        let status = response.status();

        // DTK 的错误信封也带 JSON body，先读出来再判断，别把 4xx 直接丢掉。
        let bytes = response.bytes().await?;
        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|_| AppError::Upstream(format!("解析服务返回非 JSON（HTTP {status}）")))?;

        if status.is_server_error() {
            return Err(AppError::Upstream(error_message(&body)));
        }
        Ok(body)
    }
}

/// 只保留 `www.douyin.com/aweme/v1/*` 开头的播放接口地址。
///
/// `douyinvod.com` 直链实测 403（签名一次性或已过期），play 接口每次换一张新票。
/// 某个流里一条 play 地址都没有时原样保留，避免把还能用的流删空。
fn prefer_play_urls(envelope: &mut Value) {
    let Some(data) = envelope.get_mut("data").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(media) = data.get_mut("media").and_then(Value::as_object_mut) else {
        return;
    };

    if let Some(video) = media.get_mut("video") {
        filter_node(video);
    }
    if let Some(streams) = media.get_mut("streams").and_then(Value::as_array_mut) {
        for stream in streams {
            filter_node(stream);
        }
    }
    // 实况图：每张图额外带一条短视频，同样只保留 play 地址，否则下载会 403。
    if let Some(images) = media.get_mut("images").and_then(Value::as_array_mut) {
        for image in images {
            if let Some(video) = image.get_mut("video") {
                filter_node(video);
            }
        }
    }
}

fn filter_node(node: &mut Value) {
    let Some(obj) = node.as_object_mut() else {
        return;
    };

    let mut plays: Vec<String> = Vec::new();
    for key in ["url", "urls", "url_list"] {
        match obj.get(key) {
            Some(Value::String(raw)) => collect_play(raw, &mut plays),
            Some(Value::Array(items)) => {
                for item in items {
                    if let Some(raw) = item.as_str() {
                        collect_play(raw, &mut plays);
                    }
                }
            }
            _ => {}
        }
    }

    if plays.is_empty() {
        return;
    }

    let all = Value::Array(plays.iter().cloned().map(Value::String).collect());
    obj.insert("url".to_string(), Value::String(plays[0].clone()));
    obj.insert("urls".to_string(), all.clone());
    if obj.contains_key("url_list") {
        obj.insert("url_list".to_string(), all);
    }
}

fn collect_play(raw: &str, out: &mut Vec<String>) {
    if is_play_url(raw) && !out.iter().any(|item| item == raw) {
        out.push(raw.to_string());
    }
}

fn is_play_url(raw: &str) -> bool {
    raw.starts_with("https://www.douyin.com/aweme/v1/")
        || raw.starts_with("http://www.douyin.com/aweme/v1/")
}

/// 响应出厂前的统一加工：先去水印，再筛链接，最后补帧率。
fn finalize(envelope: &mut Value) {
    drop_watermarked(envelope);
    prefer_play_urls(envelope);
    enrich_fps(envelope);
}

/// 剔除 `watermark != false` 的流。
///
/// 典型是 720×720、码率缺失、体积反而更大的烧录水印版。整条内容只有水印流时
/// 原样保留，宁可给一条有水印的，也不要返回空列表。
fn drop_watermarked(envelope: &mut Value) {
    let Some(media) = envelope
        .get_mut("data")
        .and_then(|data| data.get_mut("media"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    let Some(streams) = media.get_mut("streams").and_then(Value::as_array_mut) else {
        return;
    };

    let kept: Vec<Value> = streams
        .iter()
        .filter(|stream| !is_watermarked(stream))
        .cloned()
        .collect();
    if kept.is_empty() {
        return;
    }
    streams.clear();
    streams.extend(kept);
}

fn is_watermarked(node: &Value) -> bool {
    match node.get("watermark") {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => text.eq_ignore_ascii_case("true") || text.trim() == "1",
        Some(Value::Number(number)) => number.as_i64() == Some(1),
        _ => false,
    }
}

/// DTK 归一化后的 `streams[]` 不带 fps，帧率只存在于 `raw.video.bit_rate[].FPS`。
///
/// 用 (宽, 高, 码率) 三元组对齐，同分辨率不同码率的档位不会被张冠李戴。
fn enrich_fps(envelope: &mut Value) {
    let fps_map = build_fps_map(envelope);
    if fps_map.is_empty() {
        return;
    }

    let Some(media) = envelope
        .get_mut("data")
        .and_then(|data| data.get_mut("media"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };

    if let Some(streams) = media.get_mut("streams").and_then(Value::as_array_mut) {
        for stream in streams {
            fill_fps(stream, &fps_map);
        }
    }
    if let Some(video) = media.get_mut("video") {
        fill_fps(video, &fps_map);
    }
}

fn build_fps_map(envelope: &Value) -> HashMap<(i64, i64, i64), i64> {
    let mut map = HashMap::new();
    let Some(items) = envelope
        .pointer("/data/raw/video/bit_rate")
        .and_then(Value::as_array)
    else {
        return map;
    };

    for item in items {
        let play = item.get("play_addr");
        let width =
            as_i64(play.and_then(|node| node.get("width"))).or_else(|| as_i64(item.get("width")));
        let height =
            as_i64(play.and_then(|node| node.get("height"))).or_else(|| as_i64(item.get("height")));
        let bitrate = as_i64(item.get("bit_rate")).or_else(|| as_i64(item.get("bitrate")));
        let fps = as_i64(item.get("FPS")).or_else(|| as_i64(item.get("fps")));

        if let (Some(width), Some(height), Some(bitrate), Some(fps)) = (width, height, bitrate, fps)
        {
            if fps > 0 {
                map.insert((width, height, bitrate), fps);
            }
        }
    }
    map
}

fn fill_fps(node: &mut Value, fps_map: &HashMap<(i64, i64, i64), i64>) {
    let Some(obj) = node.as_object_mut() else {
        return;
    };
    // 上游已经给了就不覆盖。
    if as_i64(obj.get("fps")).is_some_and(|value| value > 0) {
        return;
    }

    let width = as_i64(obj.get("width"));
    let height = as_i64(obj.get("height"));
    let bitrate = as_i64(obj.get("bitrate")).or_else(|| as_i64(obj.get("bit_rate")));
    let (Some(width), Some(height), Some(bitrate)) = (width, height, bitrate) else {
        return;
    };

    if let Some(fps) = fps_map.get(&(width, height, bitrate)) {
        obj.insert("fps".to_string(), Value::from(*fps));
    }
}

/// 数字可能以 int / float / 字符串三种形态出现，统一收成 i64。
fn as_i64(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|float| float as i64)),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn ensure_success(envelope: &Value) -> Result<(), AppError> {
    if envelope.get("success").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    Err(AppError::Upstream(error_message(envelope)))
}

fn error_message(body: &Value) -> String {
    if let Some(error) = body.get("error") {
        if let Some(message) = error.get("message").and_then(Value::as_str) {
            if !message.is_empty() {
                return message.to_string();
            }
        }
        if let Some(code) = error.get("code").and_then(Value::as_str) {
            if !code.is_empty() {
                return code.to_string();
            }
        }
    }
    "解析服务返回错误".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PLAY: &str = "https://www.douyin.com/aweme/v1/play/?video_id=1&sign=2";

    #[test]
    fn filters_live_photo_video() {
        // 实况图挂在 media.images[].video 上，漏掉就会拿到 douyinvod 直链。
        let mut envelope = json!({ "data": { "media": {
            "images": [
                {
                    "url": "https://p3.douyinpic.com/a.jpg",
                    "video": {
                        "url": "https://v26-web.douyinvod.com/x",
                        "urls": ["https://v26-web.douyinvod.com/x", PLAY]
                    }
                }
            ]
        } } });

        prefer_play_urls(&mut envelope);

        let video = &envelope["data"]["media"]["images"][0]["video"];
        assert_eq!(video["url"], PLAY);
        assert_eq!(video["urls"].as_array().expect("urls").len(), 1);
    }

    #[test]
    fn keeps_only_play_urls() {
        let mut envelope = json!({
            "success": true,
            "data": { "media": { "streams": [{
                "url": "https://v11-weba.douyinvod.com/a",
                "urls": [
                    "https://v11-weba.douyinvod.com/a",
                    "https://v26-web.douyinvod.com/b",
                    PLAY
                ]
            }] } }
        });

        prefer_play_urls(&mut envelope);

        let stream = &envelope["data"]["media"]["streams"][0];
        assert_eq!(stream["url"], PLAY);
        assert_eq!(stream["urls"].as_array().expect("urls").len(), 1);
        assert_eq!(stream["urls"][0], PLAY);
    }

    #[test]
    fn keeps_original_when_no_play() {
        let mut envelope = json!({ "data": { "media": { "video": {
            "url": "https://v11-weba.douyinvod.com/a",
            "urls": ["https://v11-weba.douyinvod.com/a"]
        } } } });

        prefer_play_urls(&mut envelope);

        let video = &envelope["data"]["media"]["video"];
        assert_eq!(video["url"], "https://v11-weba.douyinvod.com/a");
        assert_eq!(video["urls"].as_array().expect("urls").len(), 1);
    }

    #[test]
    fn dedupes_same_play_url() {
        let mut envelope = json!({ "data": { "media": { "video": {
            "url": PLAY,
            "urls": [PLAY, PLAY]
        } } } });

        prefer_play_urls(&mut envelope);

        let urls = envelope["data"]["media"]["video"]["urls"]
            .as_array()
            .expect("urls");
        assert_eq!(urls.len(), 1);
    }

    #[test]
    fn leaves_non_media_payload_untouched() {
        let mut envelope = json!({ "success": true, "data": { "task_id": "abc" } });
        let before = envelope.clone();

        prefer_play_urls(&mut envelope);

        assert_eq!(envelope, before);
    }

    #[test]
    fn drops_watermarked_stream() {
        let mut envelope = json!({
            "data": { "media": { "streams": [
                {"width": 1080, "height": 1920, "bitrate": 1995135, "watermark": false},
                {"width": 720, "height": 720, "watermark": true}
            ] } }
        });

        drop_watermarked(&mut envelope);

        let streams = envelope["data"]["media"]["streams"]
            .as_array()
            .expect("streams");
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0]["width"], 1080);
    }

    #[test]
    fn keeps_streams_when_all_watermarked() {
        let mut envelope = json!({
            "data": { "media": { "streams": [
                {"width": 720, "height": 720, "watermark": true}
            ] } }
        });

        drop_watermarked(&mut envelope);

        assert_eq!(
            envelope["data"]["media"]["streams"]
                .as_array()
                .expect("streams")
                .len(),
            1
        );
    }

    #[test]
    fn drops_watermarked_with_string_flag() {
        let mut envelope = json!({
            "data": { "media": { "streams": [
                {"width": 1080, "height": 1920, "watermark": false},
                {"width": 720, "height": 720, "watermark": "true"}
            ] } }
        });

        drop_watermarked(&mut envelope);

        assert_eq!(
            envelope["data"]["media"]["streams"]
                .as_array()
                .expect("streams")
                .len(),
            1
        );
    }

    #[test]
    fn fills_fps_from_raw() {
        let mut envelope = json!({
            "data": {
                "media": { "streams": [
                    {"width": 1080, "height": 1920, "bitrate": 1995135, "url": PLAY},
                    {"width": 720, "height": 1280, "bitrate": 1488242, "fps": 60, "url": PLAY}
                ] },
                "raw": { "video": { "bit_rate": [
                    {"play_addr": {"width": 1080, "height": 1920}, "bit_rate": 1995135, "FPS": 30},
                    {"play_addr": {"width": 720, "height": 1280}, "bit_rate": 1488242, "FPS": 30}
                ] } }
            }
        });

        enrich_fps(&mut envelope);

        let streams = envelope["data"]["media"]["streams"]
            .as_array()
            .expect("streams");
        assert_eq!(streams[0]["fps"], 30);
        // 上游已有 60，不能被 raw 的 30 覆盖。
        assert_eq!(streams[1]["fps"], 60);
    }

    #[test]
    fn fps_matches_by_bitrate() {
        let mut envelope = json!({
            "data": {
                "media": { "streams": [
                    {"width": 720, "height": 1280, "bitrate": 1488242},
                    {"width": 720, "height": 1280, "bitrate": 644435}
                ] },
                "raw": { "video": { "bit_rate": [
                    {"play_addr": {"width": 720, "height": 1280}, "bit_rate": 1488242, "FPS": 30},
                    {"play_addr": {"width": 720, "height": 1280}, "bit_rate": 644435, "FPS": 60}
                ] } }
            }
        });

        enrich_fps(&mut envelope);

        let streams = envelope["data"]["media"]["streams"]
            .as_array()
            .expect("streams");
        assert_eq!(streams[0]["fps"], 30);
        assert_eq!(streams[1]["fps"], 60);
    }

    #[test]
    fn fps_tolerates_string_numbers() {
        let mut envelope = json!({
            "data": {
                "media": { "streams": [
                    {"width": 720, "height": 1280, "bitrate": "644435"}
                ] },
                "raw": { "video": { "bit_rate": [
                    {"play_addr": {"width": "720", "height": "1280"}, "bit_rate": "644435", "FPS": "50"}
                ] } }
            }
        });

        enrich_fps(&mut envelope);

        assert_eq!(envelope["data"]["media"]["streams"][0]["fps"], 50);
    }
}
