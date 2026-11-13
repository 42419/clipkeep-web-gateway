//! 运行配置：只从环境变量读取，缺失关键项直接退出。
//!
//! 设计原则：进程启动时就校验完，不要等到第一个请求才发现密钥没配。

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// 默认放行的媒体域名。抖音直链会在这几个域之间跳，别只写一个。
const DEFAULT_ALLOWED_HOSTS: &str = "douyin.com,douyinvod.com,douyinpic.com,douyincdn.com,\
douyinstatic.com,bytecdn.cn,byteimg.com,zijieapi.com,snssdk.com";

/// 媒体交付方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    /// 302 跳到 CDN 直链，服务器不出流量。
    Redirect,
    /// 服务端流式转发，必然可用但吃带宽。
    Proxy,
}

impl DeliveryMode {
    fn parse(raw: &str) -> Self {
        // 默认 proxy：redirect 会让浏览器把 video/mp4 内联播放，
        // 对一个下载工具来说"点了不下载"是更严重的错误，宁可吃带宽。
        match raw.trim().to_ascii_lowercase().as_str() {
            "redirect" => Self::Redirect,
            _ => Self::Proxy,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub dtk_base: String,
    pub dtk_key: String,
    /// 前端首次输入的口令，只用来换 session cookie。
    pub site_token: String,
    /// cookie 签名密钥；留空则由 site_token 派生。
    pub sign_secret: String,
    pub allowed_hosts: Vec<String>,
    pub static_dir: PathBuf,
    pub delivery: DeliveryMode,
    /// 每个 IP 每分钟允许的解析次数；0 表示不限。
    pub parse_rate_per_min: u32,
    pub session_ttl: Duration,
    /// 是否信任 X-Forwarded-For（放在 nginx 后面才开）。
    pub trust_proxy: bool,
    /// cookie 是否加 Secure；只有 HTTPS 部署才该打开。
    pub cookie_secure: bool,
    pub dtk_timeout: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("缺少必填环境变量 {0}")]
    Missing(&'static str),
    #[error("{name} 格式不合法：{value}")]
    Invalid { name: &'static str, value: String },
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_raw = opt("BIND").unwrap_or_else(|| "0.0.0.0:8443".to_string());
        let bind: SocketAddr = bind_raw.parse().map_err(|_| ConfigError::Invalid {
            name: "BIND",
            value: bind_raw,
        })?;

        let dtk_base = required("DTK_BASE")?;
        let dtk_base = dtk_base.trim_end_matches('/').to_string();
        if !dtk_base.starts_with("http://") && !dtk_base.starts_with("https://") {
            return Err(ConfigError::Invalid {
                name: "DTK_BASE",
                value: dtk_base,
            });
        }

        let dtk_key = required("DTK_API_KEY")?;
        let site_token = required("SITE_TOKEN")?;
        if site_token.len() < 8 {
            return Err(ConfigError::Invalid {
                name: "SITE_TOKEN",
                value: "至少 8 个字符".to_string(),
            });
        }

        let sign_secret = opt("SIGN_SECRET")
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| format!("clipkeep-gateway::{site_token}"));

        let allowed_hosts = opt("ALLOWED_HOSTS")
            .unwrap_or_else(|| DEFAULT_ALLOWED_HOSTS.to_string())
            .split(',')
            .map(|item| item.trim().to_ascii_lowercase())
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>();
        if allowed_hosts.is_empty() {
            return Err(ConfigError::Invalid {
                name: "ALLOWED_HOSTS",
                value: "不能为空".to_string(),
            });
        }

        let parse_rate_per_min = opt("PARSE_RATE_PER_MIN")
            .map(|raw| raw.parse::<u32>())
            .transpose()
            .map_err(|_| ConfigError::Invalid {
                name: "PARSE_RATE_PER_MIN",
                value: "需要非负整数".to_string(),
            })?
            .unwrap_or(20);

        let ttl_hours = opt("SESSION_TTL_HOURS")
            .map(|raw| raw.parse::<u64>())
            .transpose()
            .map_err(|_| ConfigError::Invalid {
                name: "SESSION_TTL_HOURS",
                value: "需要正整数".to_string(),
            })?
            .unwrap_or(720);

        Ok(Self {
            bind,
            dtk_base,
            dtk_key,
            site_token,
            sign_secret,
            allowed_hosts,
            static_dir: PathBuf::from(opt("STATIC_DIR").unwrap_or_else(|| "./static".to_string())),
            delivery: DeliveryMode::parse(&opt("DELIVERY").unwrap_or_default()),
            parse_rate_per_min,
            session_ttl: Duration::from_secs(ttl_hours.max(1) * 3600),
            trust_proxy: flag("TRUST_PROXY"),
            cookie_secure: flag("COOKIE_SECURE"),
            dtk_timeout: Duration::from_secs(45),
        })
    }

    /// 只放行白名单域及其子域，避免服务被当成开放代理。
    pub fn host_allowed(&self, host: &str) -> bool {
        let host = host.trim().to_ascii_lowercase();
        if host.is_empty() {
            return false;
        }
        self.allowed_hosts
            .iter()
            .any(|item| host == *item || host.ends_with(&format!(".{item}")))
    }
}

fn required(name: &'static str) -> Result<String, ConfigError> {
    opt(name)
        .filter(|value| !value.trim().is_empty())
        .ok_or(ConfigError::Missing(name))
}

fn opt(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

fn flag(name: &str) -> bool {
    opt(name)
        .map(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_defaults_to_proxy() {
        // 没配或写错都落到 proxy：宁可吃带宽，也不要"点了只播放"。
        assert_eq!(DeliveryMode::parse(""), DeliveryMode::Proxy);
        assert_eq!(DeliveryMode::parse("bogus"), DeliveryMode::Proxy);
        assert_eq!(DeliveryMode::parse("PROXY"), DeliveryMode::Proxy);
    }

    #[test]
    fn delivery_accepts_redirect() {
        assert_eq!(DeliveryMode::parse("redirect"), DeliveryMode::Redirect);
        assert_eq!(DeliveryMode::parse(" Redirect "), DeliveryMode::Redirect);
    }
}
