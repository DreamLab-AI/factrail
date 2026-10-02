# factrail-eval

Measures what a compaction must not lose — the facts the agent goes on to use —
from any transcript, with no hand-made answer key.

- `facts`: tokens a tool result introduced and the agent later wrote itself;
  hindsight labels per call.
- `metric`: compact at a cut, count the facts still visible, against the erase
  rule factrail replaces under the same decisions.
- `sim`: repeated compactions of one session at several lengths.
- `synthetic`: a deterministic corpus, so the `gate` runs without private data.
- `corpus`: real Claude Code sessions, tainted ones skipped, names hashed.
- `dataset`: Tev1-format training records from hindsight (or opt-in teacher) labels.

Licensed MIT OR Apache-2.0.
