//! A liveness read of the System One proxy: which vendor backend is answering
//! right now. It is a plain `GET`, carries no state and no key, and exists so
//! `/factrail status` can name the live backend without sending a judge request.

use std::time::Duration;

use serde_json::Value;

/// What the proxy's `GET /health` reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Health {
    /// The active vendor backend (`typesafe`, `subq`, `laya`, `kev`, …).
    pub backend: String,
    /// Whether the proxy says that backend runs on this network, when it says.
    pub local: Option<bool>,
}

/// The `/health` URL on the same origin as `endpoint`, or `None` when
/// `endpoint` is not an `http(s)://host[:port]/…` URL.
fn health_url(endpoint: &str) -> Option<String> {
    let (scheme, rest) = endpoint.split_once("://")?;
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|a| !a.is_empty())?;
    matches!(scheme, "http" | "https").then(|| format!("{scheme}://{authority}/health"))
}

/// Reads `/health` from the origin of `endpoint` (the judge's configured URL)
/// within `timeout`.
///
/// # Errors
///
/// A short reason when the URL is unusable, the proxy is unreachable or slow,
/// answers a non-2xx status, or does not name a backend. Callers fail open.
///
/// ```no_run
/// # async fn demo() {
/// use std::time::Duration;
/// let live = factrail_backend::health("http://127.0.0.1:8094/v1/systemone", Duration::from_secs(2)).await;
/// if let Ok(h) = live {
///     println!("judged by {}", h.backend);
/// }
/// # }
/// ```
pub async fn health(endpoint: &str, timeout: Duration) -> Result<Health, String> {
    let url = health_url(endpoint).ok_or_else(|| "endpoint is not an http(s) URL".to_owned())?;
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;
    let response = client.get(&url).send().await.map_err(|e| {
        if e.is_timeout() {
            "timed out".to_owned()
        } else if e.is_connect() {
            "connection refused or unreachable".to_owned()
        } else {
            e.to_string()
        }
    })?;
    let code = response.status();
    if !code.is_success() {
        return Err(format!("HTTP {}", code.as_u16()));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| "health is not JSON".to_owned())?;
    let backend = body
        .get("backend")
        .and_then(Value::as_str)
        .filter(|b| !b.is_empty())
        .ok_or_else(|| "health names no backend".to_owned())?;
    Ok(Health {
        backend: backend.to_owned(),
        local: body.get("local").and_then(Value::as_bool),
    })
}

#[cfg(test)]
mod tests {
    use super::health_url;

    #[test]
    fn the_health_url_keeps_scheme_and_authority_only() {
        assert_eq!(
            health_url("http://127.0.0.1:8094/v1/systemone").as_deref(),
            Some("http://127.0.0.1:8094/health")
        );
        assert_eq!(
            health_url("https://proxy.lan?x=1").as_deref(),
            Some("https://proxy.lan/health")
        );
        assert_eq!(health_url("ftp://x/y"), None);
        assert_eq!(health_url("nonsense"), None);
        assert_eq!(health_url("http:///path"), None);
    }
}
