//! DTK 透传。
//!
//! 刻意不做字段映射：DTK 的 `{success, data, error}` 原样回给前端，
//! 这样 Flutter 侧现成的 `dtk_mapper` 可以一行不改地继续用。

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
                prefer_play_urls(&mut envelope);
                return Ok(envelope);
            }
            let Some(task_id) = data.get("task_id").and_then(Value::as_str) else {
                prefer_play_urls(&mut envelope);
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
        prefer_play_urls(&mut envelope);
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
}
