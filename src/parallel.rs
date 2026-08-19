use std::time::Duration;

use bytes::Bytes;
use reqwest::StatusCode;

use crate::model::SearchRequest;

#[derive(Clone)]
pub struct ParallelClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ParallelError {
    #[error("parallel search failed ({status}): {body}")]
    Upstream { status: StatusCode, body: String },
    #[error("parallel request error: {0}")]
    Transport(#[from] reqwest::Error),
}

impl ParallelClient {
    pub fn new(base_url: String, api_key: String, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(5))
            .tcp_keepalive(Duration::from_secs(60))
            .pool_idle_timeout(Duration::from_secs(90))
            .http2_keep_alive_interval(Duration::from_secs(30))
            .http2_keep_alive_while_idle(true)
            .user_agent("search-cache/0.1")
            .build()
            .expect("reqwest client");
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }

    /// Returns the upstream response bytes verbatim (validated as JSON) so
    /// cached entries can be served without re-serialization.
    pub async fn search(&self, request: &SearchRequest) -> Result<Bytes, ParallelError> {
        let response = self
            .http
            .post(format!("{}/v1/search", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("content-type", "application/json")
            .json(request)
            .send()
            .await?;

        let status = response.status();
        let body = response.bytes().await?;
        if !status.is_success() {
            return Err(ParallelError::Upstream {
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }

        if let Err(err) = serde_json::from_slice::<serde::de::IgnoredAny>(&body) {
            return Err(ParallelError::Upstream {
                status: StatusCode::BAD_GATEWAY,
                body: format!("invalid JSON from Parallel: {err}"),
            });
        }
        Ok(body)
    }
}
