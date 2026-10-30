//! Device tokens and the auth extractor.
//!
//! Tokens are 256-bit random values shown once at issue time. Only their
//! SHA-256 is stored; with that much entropy a fast hash is sufficient.

use crate::routes::ApiError;
use crate::AppState;
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

const TOKEN_PREFIX: &str = "mcs_";
/// Failed auth attempts allowed per client per window.
const MAX_FAILURES: u32 = 20;
const WINDOW_SECS: i64 = 300;

pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("{TOKEN_PREFIX}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Authenticated device.
pub struct Device {
    pub name: String,
}

#[derive(Default)]
pub struct FailureLimiter {
    inner: Mutex<HashMap<String, (i64, u32)>>,
}

impl FailureLimiter {
    fn blocked(&self, key: &str) -> bool {
        let now = crate::now();
        let map = self.inner.lock().expect("limiter lock");
        matches!(map.get(key), Some((start, n)) if now - start < WINDOW_SECS && *n >= MAX_FAILURES)
    }

    fn fail(&self, key: &str) {
        let now = crate::now();
        let mut map = self.inner.lock().expect("limiter lock");
        if map.len() > 10_000 {
            map.retain(|_, (start, _)| now - *start < WINDOW_SECS);
        }
        let e = map.entry(key.to_string()).or_insert((now, 0));
        if now - e.0 >= WINDOW_SECS {
            *e = (now, 0);
        }
        e.1 += 1;
    }
}

fn client_key(parts: &Parts, state: &AppState) -> String {
    if state.cfg.trust_cf_header {
        if let Some(ip) = parts.headers.get("cf-connecting-ip").and_then(|v| v.to_str().ok()) {
            return ip.to_string();
        }
    }
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .unwrap_or_else(|| "unknown".into())
}

impl FromRequestParts<Arc<AppState>> for Device {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Self, Self::Rejection> {
        let key = client_key(parts, state);
        if state.limiter.blocked(&key) {
            return Err(ApiError::new(axum::http::StatusCode::TOO_MANY_REQUESTS, "too_many_failures"));
        }
        let token = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|t| t.starts_with(TOKEN_PREFIX) && t.len() < 100);
        let Some(token) = token else {
            state.limiter.fail(&key);
            return Err(ApiError::unauthorized());
        };
        match state.db.device_for_token(&hash_token(token)) {
            Ok(Some(name)) => Ok(Device { name }),
            Ok(None) => {
                state.limiter.fail(&key);
                Err(ApiError::unauthorized())
            }
            Err(e) => Err(ApiError::internal(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique_and_hashed() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
        assert!(a.starts_with(TOKEN_PREFIX));
        assert_eq!(hash_token(&a).len(), 64);
        assert_ne!(hash_token(&a), a);
    }

    #[test]
    fn limiter_blocks_after_threshold() {
        let l = FailureLimiter::default();
        for _ in 0..MAX_FAILURES {
            assert!(!l.blocked("1.2.3.4"));
            l.fail("1.2.3.4");
        }
        assert!(l.blocked("1.2.3.4"));
        assert!(!l.blocked("5.6.7.8"));
    }
}
