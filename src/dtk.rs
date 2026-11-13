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
                return Ok(envelope);
            }
            let Some(task_id) = data.get("task_id").and_then(Value::as_str) else {
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
