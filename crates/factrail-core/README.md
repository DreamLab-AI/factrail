# factrail-core

Verbatim, fact-keeping context compaction for coding-agent transcripts. Pure Rust,
no I/O, no async runtime.

A typed-decision judge is asked two questions about every old tool call — does the
call still matter, must its output stay verbatim? — and only what it lets go is
reduced. User and assistant text is never rewritten. Reduced results go through
**fact rails**: a reproducible read becomes a one-line re-run note; an observation
keeps its head, its fact lines (errors, HTTP codes, paths, versions, ids, counts,
receipts) and its tail, with lines chosen by learned token value from one budget
pooled across the compaction. Nothing is erased.

```rust,ignore
use factrail_core::{CompactOptions, Plan, pressure};

let plan = Plan::new(transcript, CompactOptions::default(), &Default::default())?;
// Send each request's `state` and `questions_json()` to a judge, collect one
// map of question name -> probability per request, then:
let outcome = plan.finish(&answers, pressure(None, None, 0).reduction)?;
```

`Plan::finish_without_judge` runs the same rails with no model: the deterministic
path for a session no model may see.

Provenance: a clean re-implementation of `fast-jev-compaction`,
`jev-factkeep-compaction` and `hermes-jev-compaction` (all MIT); see `NOTICE`.
Licensed MIT OR Apache-2.0.
