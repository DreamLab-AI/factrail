//! Which compactions and turns are factrail's to handle at all.

/// Whose compaction a `session.compact` dispatch is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The main conversation's real compaction: the only one judged.
    Main,
    /// A precompute: installs nothing, so sending the history ahead of time
    /// would be egress for no compaction. Answered with a skip.
    Precompute,
    /// A subagent's or fork's own transcript (a fork carries the parent's
    /// history again): left to the built-in compaction.
    Subagent,
}

impl Scope {
    /// `main`, `precompute` or `subagent`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Precompute => "precompute",
            Self::Subagent => "subagent",
        }
    }
}

/// Classify a compaction. A non-empty `agent_id` makes it a subagent's — even a
/// precompute, since a fork's precompute is still the fork's; otherwise the
/// `precompute` trigger is a precompute and every other trigger is main.
///
/// ```
/// use factrail_policy::{Scope, compact_scope};
/// assert_eq!(compact_scope(Some("auto"), None), Scope::Main);
/// assert_eq!(compact_scope(Some("precompute"), None), Scope::Precompute);
/// assert_eq!(compact_scope(Some("precompute"), Some("a1")), Scope::Subagent);
/// ```
pub fn compact_scope(trigger: Option<&str>, agent_id: Option<&str>) -> Scope {
    if agent_id.is_some_and(|a| !a.is_empty()) {
        Scope::Subagent
    } else if trigger == Some("precompute") {
        Scope::Precompute
    } else {
        Scope::Main
    }
}

/// May this completed turn trigger an automatic compaction? Only a completed
/// answer (`reason == "answer"`) from the main loop (no `agent_id`).
///
/// ```
/// use factrail_policy::turn_may_trigger;
/// assert!(turn_may_trigger(None, Some("answer")));
/// assert!(!turn_may_trigger(Some("a1"), Some("answer")));
/// assert!(!turn_may_trigger(None, Some("aborted")));
/// ```
pub fn turn_may_trigger(agent_id: Option<&str>, reason: Option<&str>) -> bool {
    agent_id.is_none_or(str::is_empty) && reason == Some("answer")
}
