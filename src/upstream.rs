//! reqwest-based upstream client.

use bytes::Bytes;
use futures_util::Stream;
use serde_json::Value;

use crate::config::Provider;
use crate::error::BridgeError;

#[derive(Clone)]
pub struct Upstream {
    client: reqwest::Client,
}

impl Upstream {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
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
    ) -> Result<impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static, BridgeError> {
        let resp = self
            .request(provider, path, body)
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
        Ok(resp.bytes_stream())
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
            .send()
            .await
            .map_err(|e| BridgeError::UpstreamUnreachable(e.to_string()))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(BridgeError::Upstream {
                status: status.as_u16(),
                message: truncate(&text, 500),
            });
        }
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
