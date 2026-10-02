//! Verbatim, fact-keeping context compaction for coding-agent transcripts.
//!
//! A long agent session must shed context. The usual answer, an LLM-written
//! summary, is lossy: a path, an exact error, an id or a count disappears while
//! it still matters. This crate never rewrites user or assistant text. It asks
//! a *typed-decision judge* two questions per old tool call — does the call still
//! matter, and must its full output stay verbatim? — and reduces only what the
//! judge lets go, by *fact rails* that keep what a re-run would not give back.
//!
//! # The method
//!
//! 1. [`Plan::new`] pairs tool calls with their results, pins the first message
//!    and the newest ones, and fits the whole conversation — results omitted,
//!    arguments capped — into the judge's token budget ([`fit_state`]), splitting
//!    very long sessions into windows that each carry the goal and the pinned tail.
//! 2. The caller sends each [`JudgeRequest`] to a judge (a System One endpoint, a
//!    local Tev-format model, or a recorded answer set) and collects one `noul`
//!    probability per question. This crate does no I/O.
//! 3. [`Plan::finish`] turns answers into [`Action`]s and applies them. Kept calls
//!    stay verbatim. A reduced *reproducible read* becomes a one-line re-run note;
//!    a reduced *observation* keeps its head, its fact lines and its tail
//!    ([`rails`]). Fact lines are chosen by learned token value and share one
//!    budget across the compaction ([`value`]). Tiers escalate result by result
//!    until the window's need is met; only past a hard line do the oldest results
//!    give way. [`Plan::finish_without_judge`] runs the same rails with no judge.
//!
//! ```
//! use factrail_core::{CompactOptions, Message, Plan, Role, pressure};
//!
//! let transcript: Vec<Message> = serde_json::from_str(r#"[
//!   {"role": "user", "text": "why is the deploy failing?", "toolUses": []},
//!   {"role": "assistant", "text": "", "toolUses": [
//!     {"tool_use_id": "t1", "tool": "Bash", "input": {"command": "kubectl get pods"}}]},
//!   {"role": "user", "text": "", "toolUses": [], "toolResults": [
//!     {"tool_use_id": "t1", "text": "api-7d9f  CrashLoopBackOff  restarts 14", "isError": false}]}
//! ]"#).unwrap();
//!
//! let plan = Plan::new(transcript, CompactOptions { preserve_recent: 0, ..Default::default() }, &Default::default()).unwrap();
//! assert_eq!(plan.requests().len(), 1);            // one request: state + two questions
//! let outcome = plan.finish_without_judge(pressure(None, None, 0).reduction);
//! assert_eq!(outcome.messages.len(), 3);           // messages are never removed
//! ```
//!
//! # Provenance
//!
//! A clean Rust re-implementation of `fast-jev-compaction` (the method),
//! `jev-factkeep-compaction` (fact rails) and `hermes-jev-compaction` (learned
//! token value, pooled budget, metadata egress), all MIT; see the repository's
//! `NOTICE`. Lengths are counted in Unicode scalar values throughout.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod apply;
pub mod calls;
pub mod formats;
pub mod model;
pub mod plan;
pub mod rails;
pub mod redact;
pub mod state;
pub mod tev;
pub mod text;
pub mod tokens;
pub mod value;

pub use apply::{Action, Reduction, Selection};
pub use calls::{ToolCall, collect_tool_calls, goal_from_messages, is_pinned};
pub use model::{Message, Role, ToolResult, ToolUse, transcript_chars};
pub use plan::{
    AnswerError, CallAnswer, CallDecision, CompactOptions, DecisionReason, JudgeRequest, Outcome,
    Plan, PlanError, Pressure, Question, RAIL_FLOOR, Stats, pressure, questions_for,
    transcript_tokens,
};
pub use state::{Egress, FittedState, TooLarge, fit_state, metadata_input};
pub use tokens::estimate_tokens;
