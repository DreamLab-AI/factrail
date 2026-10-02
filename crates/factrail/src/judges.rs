//! Building a judge from configuration, and what counts as having credentials.

use std::time::Duration;

use factrail_backend::{Judge, SystemOneJudge, TevJudge};
use factrail_policy::{Backend, Config};

/// Per-request timeout: the whole round has its own deadline on top.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The System One key: configuration first, then `FACTRAIL_API_KEY`, then `TYPESAFE_API_KEY`.
pub fn system_one_key(config: &Config) -> Option<String> {
    config
        .api_key
        .clone()
        .or_else(|| std::env::var("FACTRAIL_API_KEY").ok())
        .or_else(|| std::env::var("TYPESAFE_API_KEY").ok())
        .filter(|k| !k.is_empty())
}

/// The key for a Tev endpoint, when it needs one: configuration, then `FACTRAIL_TEV_API_KEY`.
pub fn tev_key(config: &Config) -> Option<String> {
    config
        .api_key
        .clone()
        .or_else(|| std::env::var("FACTRAIL_TEV_API_KEY").ok())
        .filter(|k| !k.is_empty())
}

/// Whether the configured model judge can be reached at all. System One needs a
/// key, unless a declared-local `baseUrl` serves it keyless (the sovereign
/// façade); a Tev endpoint needs a `baseUrl`; the rules need nothing and are
/// handled before this is asked.
pub fn has_credentials(config: &Config) -> bool {
    match config.backend {
        Backend::SystemOne => {
            system_one_key(config).is_some() || (config.backend_local && config.base_url.is_some())
        }
        Backend::Tev => config.base_url.is_some(),
        Backend::Rules => false,
    }
}

/// The model judge for `config`, or `None` for the rules backend or a Tev backend without an endpoint.
pub fn build(config: &Config) -> Option<Judge> {
    match config.backend {
        Backend::SystemOne => Some(Judge::SystemOne(SystemOneJudge::new(
            config.base_url.clone(),
            system_one_key(config),
            config.model.clone(),
            REQUEST_TIMEOUT,
        ))),
        Backend::Tev => Some(Judge::Tev(TevJudge::new(
            config.base_url.clone()?,
            tev_key(config),
            config.model.clone().unwrap_or_else(|| "tev1".to_owned()),
            8,
            true,
            REQUEST_TIMEOUT,
        ))),
        Backend::Rules => None,
    }
}
