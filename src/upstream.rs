//! reqwest-based upstream client.

use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures_util::Stream;
use serde_json::Value;

/// Boxed upstream byte stream (so callers can return/retry it without Rust 2024
/// `impl Trait` lifetime-capture issues).
pub type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

use crate::config::Provider;
use crate::error::BridgeError;

/// Max time to receive the *response headers* (streaming) — not the body, so a
/// long stream is never cut. A miss = `UpstreamUnreachable` (retryable).
const HEADERS_TIMEOUT: Duration = Duration::from_secs(30);
/// Whole-request timeout for non-streaming calls.
const JSON_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct Upstream {
    client: reqwest::Client,
}

impl Upstream {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("build reqwest client");
        Self { client }
    }

    fn request(&self, provider: &Provider, path: &str, body: &Value) -> reqwest::RequestBuilder {
        let url = format!("{}{}", provider.base_url.trim_end_matches('/'), path);
        let mut rb = self.client.post(&url).json(body);
        if let Some(key) = &provider.api_key {
            rb = rb.bearer_auth(key);
        }
        for (k, v) in &provider.extra_headers {
            rb = rb.header(k, v);
        }
        rb
    }

    /// POST and return the response body as a byte stream (for SSE).
    pub async fn post_stream(
        &self,
        provider: &Provider,
        path: &str,
        body: &Value,
    ) -> Result<ByteStream, BridgeError> {
        let send = self.request(provider, path, body).send();
        let resp = match tokio::time::timeout(HEADERS_TIMEOUT, send).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => return Err(BridgeError::UpstreamUnreachable(e.to_string())),
            Err(_) => {
                return Err(BridgeError::UpstreamUnreachable(
                    "timeout waiting for response headers".into(),
                ));
            }
        };
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(BridgeError::Upstream {
                status: status.as_u16(),
                message: truncate(&text, 500),
            });
        }
        Ok(Box::pin(resp.bytes_stream()))
    }

    /// POST and return the parsed JSON body (non-streaming).
    pub async fn post_json(
        &self,
        provider: &Provider,
        path: &str,
        body: &Value,
    ) -> Result<Value, BridgeError> {
        let resp = self
            .request(provider, path, body)
            .timeout(JSON_TIMEOUT)
            .send()
            .await
            .map_err(|e| BridgeError::UpstreamUnreachable(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(BridgeError::Upstream {
                status: status.as_u16(),
                message: truncate(&text, 500),
            });
        }
        let text = resp
            .text()
            .await
            .map_err(|e| BridgeError::UpstreamUnreachable(e.to_string()))?;
        serde_json::from_str(&text)
            .map_err(|e| BridgeError::Internal(format!("bad upstream JSON: {e}")))
    }
}

impl Default for Upstream {
    fn default() -> Self {
        Self::new()
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
