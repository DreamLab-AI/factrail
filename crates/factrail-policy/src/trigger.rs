//! When to compact: the token trigger, hysteresis after a compaction, the
//! cache-warm nudge, and pruning of per-session store entries.
//!
//! Thresholds follow JavaScript number semantics where the behaviour depends on
//! them: an infinite threshold (no usable trigger configured) is `None`, and
//! every comparison against it treats it as unreachable.

use std::fmt;

use serde::Serialize;
use serde_json::Value;

use crate::coerce::finite_number;
use crate::config::Config;

/// Per-session store entries (`taint:*`, `baseline:*`) older than this — thirty
/// days, in milliseconds — are pruned at session start.
pub const SESSION_STATE_TTL_MS: i64 = 30 * 24 * 3600 * 1000;

fn positive(n: f64) -> Option<f64> {
    (n.is_finite() && n > 0.0).then_some(n)
}

/// The trigger in tokens: the smaller of `compact_at_percent` of the window and
/// `compact_at_tokens`. Either side that is unset (`≤ 0`) or unknown (no
/// window) drops out; `None` when both do, meaning "never".
///
/// The absolute figure is what stops a large window delaying compaction: 60 %
/// of a million tokens is 600k tokens of cache reads per turn first.
///
/// ```
/// use factrail_policy::{Config, trigger_tokens};
/// let config = Config::default(); // 60 %, 180k
/// assert_eq!(trigger_tokens(Some(1_000_000.0), &config), Some(180_000.0));
/// assert_eq!(trigger_tokens(Some(200_000.0), &config), Some(120_000.0));
/// assert_eq!(trigger_tokens(None, &config), Some(180_000.0));
/// ```
pub fn trigger_tokens(window: Option<f64>, config: &Config) -> Option<f64> {
    let by_percent = window
        .and_then(positive)
        .zip(positive(config.compact_at_percent))
        .map(|(w, p)| (w * p / 100.0).floor());
    let absolute = positive(config.compact_at_tokens);
    match (by_percent, absolute) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// How far context must grow past the post-compaction size before compacting
/// again: `rearm_tokens` when positive, else a quarter of the threshold
/// (rounded up), else 0 when there is no threshold.
///
/// ```
/// use factrail_policy::rearm_gap;
/// assert_eq!(rearm_gap(Some(180_000.0), 40_000.0), 40_000.0);
/// assert_eq!(rearm_gap(Some(180_000.0), 0.0), 45_000.0);
/// ```
pub fn rearm_gap(threshold: Option<f64>, rearm_tokens: f64) -> f64 {
    positive(rearm_tokens)
        .or_else(|| threshold.map(|t| (t / 4.0).ceil()))
        .unwrap_or(0.0)
}

/// Why [`should_compact`] decided as it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TriggerReason {
    /// Context is below the trigger.
    BelowThreshold,
    /// At or above the trigger, but not grown by the re-arm gap since the last compaction.
    Hysteresis,
    /// At or above both the trigger and the re-arm point: compact.
    Threshold,
    /// No token figure; the percentage reached the trigger and no compaction happened yet.
    Percent,
    /// No usable usage figure.
    NoUsage,
}

impl TriggerReason {
    /// `below-threshold`, `hysteresis`, `threshold`, `percent` or `no-usage`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BelowThreshold => "below-threshold",
            Self::Hysteresis => "hysteresis",
            Self::Threshold => "threshold",
            Self::Percent => "percent",
            Self::NoUsage => "no-usage",
        }
    }
}

impl fmt::Display for TriggerReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outcome of [`should_compact`]. Serialises `None` thresholds as `null`,
/// as `JSON.stringify` does for `Infinity`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct TriggerVerdict {
    /// Compact now.
    pub run: bool,
    /// Why.
    pub reason: TriggerReason,
    /// The trigger in tokens; `None` = never.
    pub threshold: Option<f64>,
    /// The size context must reach: the trigger, or the baseline plus the
    /// re-arm gap when that is larger. `None` = never.
    pub need: Option<f64>,
}

/// The `turn.complete` decision.
///
/// `tokens` is the live context, `baseline` the size the last compaction left.
/// Compacts when context reaches the trigger **and** has grown by the re-arm gap
/// since the last compaction; without the second clause a compaction that cannot
/// get under the trigger re-runs every turn, each a judging round plus a full
/// prompt-cache rewrite. With no token figure, `percent` alone may trigger, but
/// only before any compaction: a missing figure never re-triggers on its own.
///
/// ```
/// use factrail_policy::{Config, TriggerReason, should_compact};
/// let config = Config::default();
/// let v = should_compact(Some(200_000.0), None, Some(1_000_000.0), Some(190_000.0), &config);
/// assert_eq!((v.run, v.reason, v.need), (false, TriggerReason::Hysteresis, Some(230_000.0)));
/// ```
pub fn should_compact(
    tokens: Option<f64>,
    percent: Option<f64>,
    window: Option<f64>,
    baseline: Option<f64>,
    config: &Config,
) -> TriggerVerdict {
    let threshold = trigger_tokens(window, config);
    let gap = rearm_gap(threshold, config.rearm_tokens);
    let baseline = baseline.and_then(positive);
    let need = match baseline {
        Some(b) => threshold.map(|t| t.max(b + gap)),
        None => threshold,
    };
    let below = |n: f64, bound: Option<f64>| bound.is_none_or(|b| n < b);
    let (run, reason) = match tokens.filter(|t| t.is_finite()) {
        Some(t) if below(t, threshold) => (false, TriggerReason::BelowThreshold),
        Some(t) if below(t, need) => (false, TriggerReason::Hysteresis),
        Some(_) => (true, TriggerReason::Threshold),
        None => match percent.filter(|p| p.is_finite()) {
            Some(p) if p >= config.compact_at_percent && baseline.is_none() => {
                (true, TriggerReason::Percent)
            }
            _ => (false, TriggerReason::NoUsage),
        },
    };
    TriggerVerdict {
        run,
        reason,
        threshold,
        need,
    }
}

/// The prompt-cache TTL in seconds. A positive `cache_ttl_seconds` wins.
/// Otherwise a session reporting rate-limit windows is on a subscription (1 h
/// cache writes); one without, holding an `ANTHROPIC_API_KEY`, is billed per
/// token at the 5-minute default; anything else is assumed 1 h.
///
/// ```
/// use factrail_policy::{Config, cache_ttl_seconds};
/// let config = Config::default();
/// assert_eq!(cache_ttl_seconds(0, true, &config), 300.0);
/// assert_eq!(cache_ttl_seconds(2, true, &config), 3600.0);
/// ```
pub fn cache_ttl_seconds(rate_limits: usize, has_api_key: bool, config: &Config) -> f64 {
    match positive(config.cache_ttl_seconds) {
        Some(ttl) => ttl,
        None if rate_limits > 0 => 3600.0,
        None if has_api_key => 300.0,
        None => 3600.0,
    }
}

/// How long a session must sit idle before the cache-warm nudge fires, in
/// milliseconds: the TTL less the margin, or half the TTL when the margin would
/// take more than half of it, so a 5-minute cache still compacts while warm.
/// NaN and negative inputs count as 0; the result is rounded and saturates.
///
/// ```
/// use factrail_policy::nudge_delay_ms;
/// assert_eq!(nudge_delay_ms(3600.0, 300.0), 3_300_000);
/// assert_eq!(nudge_delay_ms(300.0, 300.0), 150_000);
/// ```
pub fn nudge_delay_ms(ttl_s: f64, margin_s: f64) -> u64 {
    let clamp = |n: f64| if n.is_nan() { 0.0 } else { n.max(0.0) };
    let (ttl, margin) = (clamp(ttl_s), clamp(margin_s));
    let seconds = if ttl > 2.0 * margin {
        ttl - margin
    } else {
        ttl / 2.0
    };
    (seconds * 1000.0).round() as u64
}

/// Arm the cache-warm nudge for an idle session? Only at or above the floor (a
/// small context re-prefills cheaply), and only once grown by `gap` past the
/// last compaction, so a freshly compacted session is not compacted again the
/// moment it goes quiet.
///
/// ```
/// use factrail_policy::should_arm_nudge;
/// assert!(should_arm_nudge(Some(120_000.0), 100_000.0, None, 40_000.0));
/// assert!(!should_arm_nudge(Some(120_000.0), 100_000.0, Some(110_000.0), 40_000.0));
/// ```
pub fn should_arm_nudge(tokens: Option<f64>, floor: f64, baseline: Option<f64>, gap: f64) -> bool {
    let Some(tokens) = tokens.filter(|t| t.is_finite()) else {
        return false;
    };
    if tokens < floor {
        return false;
    }
    !matches!(baseline.and_then(positive), Some(b) if tokens < b + gap)
}

/// Which per-session store keys have expired. Only `taint:*` and `baseline:*`
/// keys are considered; one expires when its value carries no readable `at`
/// (milliseconds since the epoch, a number or numeric string) or is older than
/// `ttl_ms`. Other keys are never pruned.
///
/// ```
/// use factrail_policy::{SESSION_STATE_TTL_MS, expired_session_keys};
/// use serde_json::json;
/// let now = SESSION_STATE_TTL_MS * 2;
/// let entries = vec![
///     ("taint:old".to_string(), json!({ "at": 0 })),
///     ("taint:new".to_string(), json!({ "at": now - 1000 })),
///     ("enabled".to_string(), json!(true)),
/// ];
/// assert_eq!(expired_session_keys(&entries, now, SESSION_STATE_TTL_MS), ["taint:old"]);
/// ```
pub fn expired_session_keys(entries: &[(String, Value)], now_ms: i64, ttl_ms: i64) -> Vec<String> {
    entries
        .iter()
        .filter(|(key, _)| key.starts_with("taint:") || key.starts_with("baseline:"))
        .filter(|(_, value)| match finite_number(value.get("at")) {
            Some(at) => now_ms as f64 - at > ttl_ms as f64,
            None => true,
        })
        .map(|(key, _)| key.clone())
        .collect()
}
