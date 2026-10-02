//! The compaction decision: which judge, and why.

use std::fmt;

use crate::config::{Config, Fallback};
use crate::taint::TaintScan;

/// Who judges a compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Judge {
    /// The configured model backend.
    Model,
    /// The deterministic fact-rail rules: no model, no network.
    Rules,
    /// Claude Code's built-in summary.
    Builtin,
}

impl Judge {
    /// `model`, `rules` or `builtin`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Rules => "rules",
            Self::Builtin => "builtin",
        }
    }
}

impl fmt::Display for Judge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a [`Judge`] was chosen; the log vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Clean session, switched on, credentials present: the model judges.
    Ok,
    /// Tainted session judged by a model the operator declared local.
    /// Distinct from [`Reason::Ok`] so the relaxation stays auditable.
    OkLocal,
    /// The switch is off.
    SwitchedOff,
    /// No credentials for the model.
    NoKey,
    /// Tainted session and a backend not declared local.
    Tainted,
}

impl Reason {
    /// `ok`, `ok-local`, `switched-off`, `no-key` or `tainted`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::OkLocal => "ok-local",
            Self::SwitchedOff => "switched-off",
            Self::NoKey => "no-key",
            Self::Tainted => "tainted",
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outcome of [`decide`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Who judges.
    pub judge: Judge,
    /// Why.
    pub reason: Reason,
    /// For a tainted session (`tainted` or `ok-local`): `"<n> call(s): <sample>"`.
    pub detail: Option<String>,
}

/// Choose the judge for one compaction.
///
/// Precedence: `switched-off` beats `no-key` beats the taint decision. A
/// declared-local backend does not resurrect a switched-off or keyless session.
///
/// * switched off ⇒ [`Judge::Builtin`], `switched-off`;
/// * no credentials ⇒ the fallback judge, `no-key`;
/// * tainted and `backend_local` false ⇒ the fallback judge, `tainted`, with detail;
/// * tainted and `backend_local` true ⇒ [`Judge::Model`], `ok-local`, with detail;
/// * otherwise ⇒ [`Judge::Model`], `ok`.
///
/// The fallback judge is [`Judge::Rules`] (rules send nothing anywhere, so taint
/// is irrelevant to them), or [`Judge::Builtin`] when `fallback` is
/// [`Fallback::Summary`]. `credentials` is whether the configured backend can
/// be called; for [`crate::Backend::Rules`], which needs none, pass `true`.
///
/// ```
/// use factrail_policy::{Config, Judge, Reason, TaintScan, decide};
/// let config = Config::default();
/// let dirty = TaintScan { tainted: true, count: 1, sample: vec!["mcp__email-gateway__x".into()], sticky: false };
/// let v = decide(true, true, &dirty, &config);
/// assert_eq!((v.judge, v.reason), (Judge::Rules, Reason::Tainted));
/// assert_eq!(v.detail.as_deref(), Some("1 call(s): mcp__email-gateway__x"));
/// ```
pub fn decide(enabled: bool, credentials: bool, taint: &TaintScan, config: &Config) -> Verdict {
    let verdict = |judge, reason, detail| Verdict {
        judge,
        reason,
        detail,
    };
    if !enabled {
        return verdict(Judge::Builtin, Reason::SwitchedOff, None);
    }
    let fallback = match config.fallback {
        Fallback::Rules => Judge::Rules,
        Fallback::Summary => Judge::Builtin,
    };
    if !credentials {
        return verdict(fallback, Reason::NoKey, None);
    }
    if taint.tainted {
        let detail = Some(format!(
            "{} call(s): {}",
            taint.count,
            taint.sample.join(", ")
        ));
        return if config.backend_local {
            verdict(Judge::Model, Reason::OkLocal, detail)
        } else {
            verdict(fallback, Reason::Tainted, detail)
        };
    }
    verdict(Judge::Model, Reason::Ok, None)
}
