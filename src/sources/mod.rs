//! Flight data sources behind a common trait, with automatic failover.

pub mod adsbdb;
pub mod adsbx;
pub use adsbx::AdsbxSource;
pub mod manager;
pub mod opensky;
pub mod poller;

use crate::model::aircraft::Aircraft;
use crate::model::bbox::BBox;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Maximum radius supported by the ADS-B Exchange v2 style APIs.
pub const MAX_RADIUS_NM: u32 = 250;

/// Area to query: a circle (for ADS-B APIs) and its bounding box (for OpenSky).
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub lat: f64,
    pub lon: f64,
    pub radius_nm: u32,
    pub bbox: BBox,
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("HTTP status {0}")]
    Status(u16),
    #[error("rate limited")]
    RateLimited(Option<Duration>),
    #[error("bad response: {0}")]
    Parse(String),
}

/// A provider of live aircraft positions.
pub trait FlightSource: Send + Sync {
    fn name(&self) -> &'static str;
    /// Minimum spacing between two requests to this provider.
    fn min_interval(&self) -> Duration;
    fn fetch<'a>(&'a self, query: &'a Query) -> BoxFuture<'a, Result<Vec<Aircraft>, SourceError>>;
}

/// Seconds to wait from a 429 response: standard `Retry-After` (delta-seconds)
/// or OpenSky's `X-Rate-Limit-Retry-After-Seconds`.
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    [
        reqwest::header::RETRY_AFTER.as_str(),
        "x-rate-limit-retry-after-seconds",
    ]
    .iter()
    .filter_map(|h| headers.get(*h))
    .filter_map(|v| v.to_str().ok())
    .find_map(|v| v.trim().parse::<u64>().ok())
    .map(Duration::from_secs)
}

/// GET a URL and return the body text, mapping 429 / errors to `SourceError`.
pub(crate) async fn get_text(client: &reqwest::Client, url: &str) -> Result<String, SourceError> {
    let resp = client.get(url).send().await?;
    let status = resp.status();
    if status.as_u16() == 429 {
        return Err(SourceError::RateLimited(retry_after(resp.headers())));
    }
    if !status.is_success() {
        return Err(SourceError::Status(status.as_u16()));
    }
    Ok(resp.text().await?)
}

/// HTTP client for unit tests (installs the same rustls provider as `main`).
#[cfg(test)]
pub fn test_client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    #[test]
    fn parses_retry_after_headers() {
        let mut h = HeaderMap::new();
        assert_eq!(retry_after(&h), None);
        h.insert(
            "x-rate-limit-retry-after-seconds",
            HeaderValue::from_static("120"),
        );
        assert_eq!(retry_after(&h), Some(Duration::from_secs(120)));
        h.insert("retry-after", HeaderValue::from_static("7"));
        assert_eq!(retry_after(&h), Some(Duration::from_secs(7)));
        let mut d = HeaderMap::new();
        d.insert(
            "retry-after",
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(retry_after(&d), None);
    }
}
