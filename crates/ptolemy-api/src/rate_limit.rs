// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Rate limiting middleware using token bucket algorithm.

use axum::{
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use time::OffsetDateTime;

/// Rate limiter configuration.
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<RateLimiterInner>>,
    max_requests: u64,
    window_secs: i64,
}

struct RateLimiterInner {
    buckets: HashMap<IpAddr, Bucket>,
}

struct Bucket {
    count: u64,
    window_start: i64,
}

impl RateLimiter {
    /// Create a new rate limiter.
    /// `max_requests` per `window_secs` seconds per IP.
    pub fn new(max_requests: u64, window_secs: i64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RateLimiterInner {
                buckets: HashMap::new(),
            })),
            max_requests,
            window_secs,
        }
    }

    async fn check(&self, ip: IpAddr) -> bool {
        let mut inner = self.inner.lock().await;
        let now = OffsetDateTime::now_utc().unix_timestamp();

        let bucket = inner.buckets.entry(ip).or_insert(Bucket {
            count: 0,
            window_start: now,
        });

        // Reset window if expired
        if now - bucket.window_start >= self.window_secs {
            bucket.count = 0;
            bucket.window_start = now;
        }

        if bucket.count >= self.max_requests {
            return false;
        }

        bucket.count += 1;
        true
    }
}

/// Axum middleware that enforces rate limits.
pub async fn rate_limit_middleware(
    request: Request,
    next: Next,
) -> Response {
    // Extract IP from connection info or X-Forwarded-For
    let ip = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));

    // Get rate limiter from extensions
    let limiter = request
        .extensions()
        .get::<RateLimiter>()
        .cloned();

    if let Some(limiter) = limiter {
        if !limiter.check(ip).await {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [("Retry-After", "60")],
                "rate limit exceeded",
            )
                .into_response();
        }
    }

    next.run(request).await
}
