//! 极简内存限流：按 IP 计窗口内请求数，够用且不要依赖 Redis。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct RateLimiter {
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

struct Bucket {
    start: Instant,
    count: u32,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// `limit == 0` 表示不限流。
    pub fn allow(&self, ip: IpAddr, limit: u32, window: Duration) -> bool {
        if limit == 0 {
            return true;
        }
        let now = Instant::now();
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // 内存上限兜底：超过 4096 个来源就清掉过期窗口，避免被扫端口撑爆。
        if buckets.len() > 4096 {
            buckets.retain(|_, bucket| now.duration_since(bucket.start) < window);
        }

        let bucket = buckets.entry(ip).or_insert(Bucket {
            start: now,
            count: 0,
        });
        if now.duration_since(bucket.start) >= window {
            bucket.start = now;
            bucket.count = 0;
        }
        if bucket.count >= limit {
            return false;
        }
        bucket.count += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_limit() {
        let limiter = RateLimiter::new();
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        let window = Duration::from_secs(60);
        for _ in 0..3 {
            assert!(limiter.allow(ip, 3, window));
        }
        assert!(!limiter.allow(ip, 3, window));
    }

    #[test]
    fn zero_means_unlimited() {
        let limiter = RateLimiter::new();
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        for _ in 0..100 {
            assert!(limiter.allow(ip, 0, Duration::from_secs(1)));
        }
    }

    #[test]
    fn separate_ips_are_independent() {
        let limiter = RateLimiter::new();
        let window = Duration::from_secs(60);
        let a: IpAddr = "1.1.1.1".parse().unwrap();
        let b: IpAddr = "2.2.2.2".parse().unwrap();
        assert!(limiter.allow(a, 1, window));
        assert!(!limiter.allow(a, 1, window));
        assert!(limiter.allow(b, 1, window));
    }
}
