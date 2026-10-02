//! # factrail-policy
//!
//! The estate policy of [factrail](https://github.com/DreamLab-AI/factrail)
//! compaction: every decision made **before** anything can leave the machine.
//! The crate is pure — no I/O, no async, no clock or environment reads — so
//! each rule is a function of its arguments and can be asserted in a test.
//!
//! | concern | items |
//! |---|---|
//! | options | [`Config::from_options`], [`list_option`], [`compaction_timeout_ms`] |
//! | taint fence | [`taints_tool`], [`skill_taints`], [`scan_taint`], [`merge_taint`], [`taint_record`], [`taint_write`] |
//! | switch | [`resolve_enabled`], [`SwitchCommand`] |
//! | judge | [`decide`] → [`Verdict`] |
//! | trigger | [`trigger_tokens`], [`rearm_gap`], [`should_compact`] |
//! | cache warmth | [`cache_ttl_seconds`], [`nudge_delay_ms`], [`should_arm_nudge`], [`CacheWarm`] |
//! | housekeeping | [`expired_session_keys`], [`compact_scope`], [`turn_may_trigger`] |
//!
//! ## The data boundary
//!
//! A session that has used an email tool (any prefix in `taintTools`, any skill
//! in `taintSkills`) is *tainted*: its transcript must not be sent to a judge
//! that is off this network. Three properties of the fence are load-bearing.
//!
//! **Locality is declared, never inferred.** [`Config::backend_local`] is
//! `true` only when the operator's resolved configuration says so — JSON `true`
//! or the exact string `"true"` a shell projection carries. It is never derived
//! from `baseUrl`: "looks like a LAN address" is a guess that a DNS name, a
//! proxy or a redirect can make wrong, and a hostname is not evidence of where
//! bytes come to rest. Saying nothing keeps the fence shut. When the fence does
//! open, the outcome is reported as `ok-local`, never `ok`, so the relaxation
//! stays auditable in the log.
//!
//! **Taint is sticky.** The first tainting call is recorded in the plugin
//! store and [`merge_taint`] honours that record for the rest of the session.
//! A built-in summary can absorb email content and leave no tool call behind,
//! so a later clean scan is not evidence of cleanliness.
//!
//! **Rules ignore taint.** The deterministic fact-rail judge ([`Judge::Rules`])
//! sends nothing anywhere, so a tainted or keyless session is judged by rules
//! rather than handed to Claude Code's lossy built-in summary. Setting
//! `fallback` to `"summary"` restores the older behaviour.
//!
//! ## Example
//!
//! ```
//! use factrail_policy::{Config, Judge, Reason, ToolRef, decide, merge_taint, scan_taint};
//! use serde_json::json;
//!
//! let config = Config::from_options(json!({ "backendLocal": true }).as_object().unwrap());
//! let tools: Vec<ToolRef> =
//!     serde_json::from_value(json!([{ "tool": "Read" }, { "tool": "mcp__email-gateway__ask_email" }])).unwrap();
//!
//! let taint = merge_taint(None, scan_taint(&tools, &config));
//! let verdict = decide(true, true, &taint, &config);
//! assert_eq!(verdict.judge, Judge::Model);
//! assert_eq!(verdict.reason.as_str(), "ok-local");
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod coerce;
pub mod config;
mod decide;
mod scope;
mod switch;
mod taint;
mod trigger;

pub use config::{
    Backend, CacheWarm, Config, DEFAULT_COMPACTION_TIMEOUT_MS, DEFAULT_TAINT_SKILLS,
    DEFAULT_TAINT_TOOLS, Egress, Fallback, compaction_timeout_ms, list_option,
};
pub use decide::{Judge, Reason, Verdict, decide};
pub use scope::{Scope, compact_scope, turn_may_trigger};
pub use switch::{SwitchCommand, resolve_enabled};
pub use taint::{
    TaintScan, ToolRef, baseline_key, merge_taint, scan_taint, skill_taints, sticky_tainted,
    taint_key, taint_record, taint_write, taints_tool,
};
pub use trigger::{
    SESSION_STATE_TTL_MS, TriggerReason, TriggerVerdict, cache_ttl_seconds, expired_session_keys,
    nudge_delay_ms, rearm_gap, should_arm_nudge, should_compact, trigger_tokens,
};

/// Compiles and runs the README's example as a doctest.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
