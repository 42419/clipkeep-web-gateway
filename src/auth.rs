//! 口令校验与会话 cookie。
//!
//! 只做一件事：前端输一次口令，换一张带签名和过期时间的 cookie，
//! 之后所有请求（含浏览器原生下载）自动带上，URL 里不再出现任何密钥。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub const COOKIE_NAME: &str = "ck_session";
const VERSION: &str = "v1";

/// 常数时间比较，避免用 `==` 按字节短路泄漏前缀长度。
pub fn verify_secret(input: &str, expected: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    constant_time_eq(input.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

fn sign(secret: &str, payload: &str) -> Option<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(payload.as_bytes());
    Some(mac.finalize().into_bytes().to_vec())
}

/// 生成 `v1.<过期秒>.<签名>`。
pub fn issue_session(secret: &str, ttl: Duration) -> Option<(String, u64)> {
    let exp = now_secs() + ttl.as_secs().max(60);
    let payload = format!("{VERSION}\n{exp}");
    let tag = sign(secret, &payload)?;
    let value = format!("{VERSION}.{exp}.{}", URL_SAFE_NO_PAD.encode(tag));
    Some((value, ttl.as_secs().max(60)))
}

/// 校验 cookie；返回剩余有效秒数。
pub fn verify_session(secret: &str, value: &str) -> Option<u64> {
    let mut parts = value.split('.');
    if parts.next()? != VERSION {
        return None;
    }
    let exp: u64 = parts.next()?.parse().ok()?;
    let tag = URL_SAFE_NO_PAD.decode(parts.next()?).ok()?;
    if parts.next().is_some() {
        return None;
    }

    let payload = format!("{VERSION}\n{exp}");
    let expected = sign(secret, &payload)?;
    if !constant_time_eq(&expected, &tag) {
        return None;
    }

    let now = now_secs();
    if exp <= now {
        return None;
    }
    Some(exp - now)
}

/// 拼 Set-Cookie；`secure` 只在 HTTPS 部署时打开。
pub fn session_cookie(value: &str, max_age: u64, secure: bool) -> String {
    let mut cookie =
        format!("{COOKIE_NAME}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

pub fn clear_cookie(secure: bool) -> String {
    session_cookie("", 0, secure)
}

/// 从 Cookie 头里取指定名字；不引第三方 cookie 解析库。
pub fn read_cookie<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        if key.trim() == name {
            Some(value.trim())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_roundtrip() {
        let (value, ttl) = issue_session("secret", Duration::from_secs(3600)).unwrap();
        let left = verify_session("secret", &value).unwrap();
        assert!(left <= ttl && left > ttl - 5);
    }

    #[test]
    fn session_rejects_tampering() {
        let (value, _) = issue_session("secret", Duration::from_secs(3600)).unwrap();
        assert!(verify_session("other", &value).is_none());

        let mut parts: Vec<&str> = value.split('.').collect();
        parts[1] = "9999999999";
        assert!(verify_session("secret", &parts.join(".")).is_none());
    }

    #[test]
    fn session_expires() {
        let secret = "secret";
        let exp = now_secs() - 10;
        let payload = format!("{VERSION}\n{exp}");
        let tag = sign(secret, &payload).unwrap();
        let value = format!("{VERSION}.{exp}.{}", URL_SAFE_NO_PAD.encode(tag));
        assert!(verify_session(secret, &value).is_none());
    }

    #[test]
    fn cookie_parsing() {
        let header = "a=1; ck_session=abc.def.ghi; b=2";
        assert_eq!(read_cookie(header, COOKIE_NAME), Some("abc.def.ghi"));
        assert_eq!(read_cookie(header, "missing"), None);
    }

    #[test]
    fn secret_compare() {
        assert!(verify_secret("abc", "abc"));
        assert!(!verify_secret("abc", "abd"));
        assert!(!verify_secret("", "abc"));
        assert!(!verify_secret("abc", ""));
    }
}
