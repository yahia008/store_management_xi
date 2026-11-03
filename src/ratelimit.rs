//! Per-client request limits (NFR-20).
//!
//! Counters are rows in Postgres, one per bucket and time window, so every application instance shares
//! the same count (NFR-06) instead of each keeping its own in memory. A client over its limit gets
//! `429 Too Many Requests` with a `Retry-After` header and the request never reaches a handler, so a
//! rejected request cannot create a duplicate transaction or skip a permission check.
//!
//! Three kinds of limit apply:
//! * `auth`: sign-in, password reset, MFA and first-time setup, counted per IP address. These are the
//!   endpoints worth guessing at, so they get the tightest limit.
//! * `write` / `read`: per signed-in user (identified by a hash of the bearer token), writes stricter.
//! * `ip`: a generous ceiling per IP address for everything, so rotating made-up tokens does not escape.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use crate::error::ApiError;
use crate::state::AppState;

const AUTH_PATHS: [&str; 6] = [
    "/api/v1/auth/login",
    "/api/v1/auth/password/forgot",
    "/api/v1/auth/password/reset",
    "/api/v1/auth/mfa/verify",
    "/api/v1/auth/mfa/confirm",
    "/api/v1/setup",
];

fn client_ip(req: &Request, trust_proxy: bool) -> String {
    if trust_proxy {
        // The proxy appends the address it saw; earlier entries can be forged by the client, so use the last.
        if let Some(ip) = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.rsplit(',').next()).map(str::trim).filter(|v| !v.is_empty()) {
            return ip.to_string();
        }
    }
    req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_string()).unwrap_or_else(|| "unknown".into())
}

/// Adds one hit to a bucket for the current window; returns (hits so far, seconds until the window ends).
async fn hit(state: &AppState, bucket: &str) -> Result<(i32, i64), sqlx::Error> {
    let w = state.config.rate_window_seconds.max(1) as f64;
    let row: (i32, i64) = sqlx::query_as(
        "INSERT INTO rate_limits (bucket, window_start, hits)
         VALUES ($1, to_timestamp(floor(extract(epoch from now())::float8 / $2) * $2), 1)
         ON CONFLICT (bucket, window_start) DO UPDATE SET hits = rate_limits.hits + 1
         RETURNING hits, GREATEST(1, ceil(extract(epoch from (window_start + make_interval(secs => $2) - now()))))::bigint",
    )
    .bind(bucket)
    .bind(w)
    .fetch_one(&state.pool)
    .await?;
    if row.0 == 1 {
        // A new bucket: a good moment to forget windows that ended long ago.
        let _ = sqlx::query("DELETE FROM rate_limits WHERE window_start < now() - interval '2 days'").execute(&state.pool).await;
    }
    Ok(row)
}

pub async fn limit(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let cfg = &state.config;
    let path = req.uri().path().to_string();
    if !cfg.rate_limit_enabled || path == "/health" {
        return next.run(req).await;
    }
    let ip = client_ip(&req, cfg.trust_proxy_headers);
    let reading = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let bearer = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::trim).filter(|t| !t.is_empty());

    let mut buckets: Vec<(String, i32)> = Vec::new();
    if AUTH_PATHS.contains(&path.as_str()) {
        buckets.push((format!("auth:{ip}"), cfg.rate_auth_per_window));
    } else {
        if let Some(token) = bearer {
            let who = hex::encode(&Sha256::digest(token.as_bytes())[..8]);
            buckets.push(if reading { (format!("read:{who}"), cfg.rate_read_per_window) } else { (format!("write:{who}"), cfg.rate_write_per_window) });
        }
        buckets.push((format!("ip:{ip}"), cfg.rate_ip_per_window));
    }
    for (bucket, max) in buckets {
        match hit(&state, &bucket).await {
            Ok((hits, retry)) if hits > max => {
                tracing::warn!("rate limit hit: {bucket} ({hits} > {max})");
                return ApiError::RateLimited(retry).into_response();
            }
            Ok(_) => {}
            // If the counter itself fails, let the request through: the limiter must not take the service down.
            Err(e) => tracing::error!("rate limiter error: {e}"),
        }
    }
    next.run(req).await
}
