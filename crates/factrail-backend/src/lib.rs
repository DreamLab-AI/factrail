//! The I/O half of [factrail](https://docs.rs/factrail-core): the judges that
//! answer a compaction plan's requests, and the stores the `factrail` binary
//! writes.
//!
//! `factrail-core` is pure: [`Plan::new`](factrail_core::Plan::new) builds
//! [`JudgeRequest`](factrail_core::JudgeRequest)s and
//! [`Plan::finish`](factrail_core::Plan::finish) applies the answers. This crate
//! sits between the two.
//!
//! # Judges
//!
//! A [`Judge`] turns one request into one `noul` probability per question:
//!
//! * [`SystemOneJudge`] speaks the TypeSafe System One wire format — the hosted
//!   endpoint, or a sovereign façade serving the same format (keyless when
//!   local). One HTTP call answers every question of a request.
//! * [`TevJudge`] drives a Tev1-format decision model (`togethercomputer/tev1`)
//!   on any OpenAI-compatible `/chat/completions` endpoint. It answers one
//!   multiple-choice question per call and reads the probability off the
//!   first-token log-probabilities. It is a smaller model than Jev, trained on
//!   short states, and is **not** claimed to match it.
//!
//! [`ask_all`] runs a whole round concurrently under one deadline; past it every
//! in-flight request is dropped, so a late answer can never be used.
//!
//! # The data boundary
//!
//! What leaves the process is the request's state and questions, after
//! [`redact_value`](factrail_core::redact::redact_value) has masked every
//! credential shape and the judge's own API key ([`Judge::outgoing`] shows the
//! exact payload). Tool-result *contents* never leave: the state describes each
//! result only by its size and error flag. Tool *arguments* leave as
//! `factrail-core` fitted them (capped JSON, or only their shape under
//! [`Egress::Metadata`](factrail_core::Egress::Metadata)).
//!
//! # Stores
//!
//! Plain files under the XDG directories ([`Paths`]), written with `std::fs`:
//!
//! | store | path | modes (unix) |
//! |---|---|---|
//! | [`OutputStore`] | `$XDG_CACHE_HOME/factrail/outputs/<session>/<tool_use_id>.txt` | dir 0700, file 0600 |
//! | [`AnswerStore`] | `$XDG_CACHE_HOME/factrail/answers/<session>.json` | dir 0700, file 0600 |
//! | [`DecisionLog`] | `$XDG_DATA_HOME/factrail/decisions/<YYYY-MM-DD>.jsonl` | dir 0700, file 0600 |
//!
//! Saved outputs are redacted before they are written and expire after 30 days.
//!
//! ```
//! use std::time::Duration;
//! use factrail_backend::{Judge, Paths, SystemOneJudge};
//!
//! let judge = Judge::SystemOne(SystemOneJudge::new(
//!     Some("http://127.0.0.1:8090/v1/systemone".into()), // a local façade, no key
//!     None,
//!     None,
//!     Duration::from_secs(30),
//! ));
//! assert_eq!(judge.info().kind, "systemone");
//! assert_eq!(judge.info().model, "jev-latest");
//!
//! let paths = Paths::under("/tmp/factrail-example");
//! assert!(paths.cache.ends_with("cache/factrail"));
//! ```

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod judge;
pub mod log;
pub mod store;

pub use judge::{
    Answered, Judge, JudgeError, JudgeInfo, Outgoing, SYSTEM_ONE_MODEL, SYSTEM_ONE_URL,
    SystemOneJudge, TEV_CONCURRENCY, TevJudge, ask_all,
};
pub use log::{DecisionLog, DecisionRecord, RecordedRequest, civil_date};
pub use store::{
    ANSWER_CAP, AnswerStore, EXPIRE_INTERVAL, OUTPUT_CAP_BYTES, OUTPUT_MAX_AGE, OutputStore, Paths,
    sanitise,
};
