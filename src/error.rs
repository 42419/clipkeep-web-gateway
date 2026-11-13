//! 统一错误：所有失败都收敛成 `{"detail": "..."}`，前端只看一种形状。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    Unauthorized(&'static str),
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    TooManyRequests(String),
    #[error("{0}")]
    Upstream(String),
    #[error("内部错误：{0}")]
    Internal(String),
}

impl AppError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::Upstream(_) => StatusCode::BAD_GATEWAY,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "internal error");
            // 细节只进日志，不回给客户端。
            return (status, Json(json!({ "detail": "服务器内部错误" }))).into_response();
        }
        (status, Json(json!({ "detail": self.to_string() }))).into_response()
    }
}

impl From<reqwest::Error> for AppError {
    fn from(value: reqwest::Error) -> Self {
        Self::Upstream(format!("上游请求失败：{}", short(&value)))
    }
}

/// reqwest 的 Display 会带完整 URL（含签名参数），日志和响应里都不该出现。
fn short(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        return "超时".to_string();
    }
    if error.is_connect() {
        return "连接失败".to_string();
    }
    if error.is_decode() {
        return "响应解析失败".to_string();
    }
    error
        .status()
        .map(|status| format!("HTTP {status}"))
        .unwrap_or_else(|| "网络错误".to_string())
}
