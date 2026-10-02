//! Evaluation and training data for factrail compaction.
//!
//! What a compaction must not lose is what the agent goes on to use. This crate
//! measures exactly that, from any transcript, with no hand-made answer key:
//!
//! * [`facts`] finds the tokens a tool result introduced and the agent later
//!   wrote itself, and labels each call by hindsight.
//! * [`metric`] compacts a transcript at a cut and counts the facts still
//!   visible, against the erase rule this project replaces under the same
//!   decisions; deciders are the rails alone, a hindsight oracle, remembered
//!   answers, or any live judge behind a callback.
//! * [`sim`] streams a session through repeated compactions.
//! * [`synthetic`] makes a deterministic corpus, so the [`gate`] runs anywhere
//!   without private data.
//! * [`corpus`] loads real Claude Code sessions, skipping every tainted one and
//!   naming each by a hash.
//! * [`dataset`] exports Tev1-format records (hindsight or teacher labels) for
//!   training a local judge.
//!
//! ```
//! use factrail_eval::{metric::{evaluate, Decider, EvalOptions}, synthetic::corpus};
//! let report = evaluate(&corpus(1, 2), &mut Decider::Rules, &EvalOptions::default()).unwrap();
//! assert!(report.totals.rate >= report.totals.rate_erase);
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod corpus;
pub mod dataset;
pub mod facts;
pub mod gate;
pub mod metric;
pub mod sim;
pub mod synthetic;
