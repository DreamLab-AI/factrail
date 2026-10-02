# factrail

Verbatim, fact-keeping context compaction for coding agents, in Rust.

When a long agent session must shed context, the usual answer is an LLM-written summary,
and summaries lose exactly what matters: a path, an exact error, an id, a count. factrail
never rewrites what the user or the agent wrote. It asks a typed-decision judge two
questions about each old tool call: does the call still matter, and must its output stay
verbatim? Only what the judge lets go is reduced, and reduced through **fact rails**:

- a **reproducible read** (a file read, a search, `git log`) becomes a one-line re-run note;
- an **observation** (an HTTP call, a test run, a log, a deploy) keeps its head, its fact
  lines (errors, status codes, paths, versions, ids, counts, receipts) and its tail. The
  fact lines are chosen by learned token value, from one budget pooled across the whole
  compaction;
- nothing is erased, and every reduced output is saved, secrets masked, one read away.

## Measured

`factrail eval` replays transcripts, compacts them at the half-way and three-quarter
marks, and counts the **facts the agent went on to use**: tokens a tool result introduced
that the agent later wrote itself. Nobody prepares an answer key. Each run is compared
with the erase-and-truncate rule factrail replaces, *under the same decisions*.

| corpus | judge | facts kept in context | erase rule | reduction |
|---|---|---|---|---|
| 40 of our own Claude Code sessions (tainted ones excluded) | none (rules) | **86.2%** (8,421 / 9,773) | 73.7% | 38% |
| the same | hindsight oracle (upper bound) | 100% | 100% | 17% |
| 40 synthetic sessions (the committed gate) | none (rules) | 93.7% | 68.7% | 75% |

Of the facts the rules dropped from context on real sessions, 78% came from reproducible
reads, which a re-run gives back. The rest sit in the saved outputs. Over repeated
compactions (`--sim`), 89% are kept at 0.8 windows and 85% at 1.5. Some real sessions are
dominated by conversation text, which factrail never rewrites, so a session 1.5 windows
long can still overfill its window. These are measurements on one estate's sessions on
2026-10-02, not a guarantee.

## Layout

| crate | what |
|---|---|
| `factrail-core` | the pure engine: transcript model and formats, state fitting, questions, fact rails, token value, pooling, redaction, the Tev prompt |
| `factrail-policy` | estate policy: taint fence, switch, token trigger with hysteresis, cache-warm timing, scope |
| `factrail-backend` | judges (System One, Tev on any OpenAI-compatible server) and stores (saved outputs, remembered answers, decision log) |
| `factrail-eval` | hindsight facts and labels, the replay metric, session simulation, synthetic corpus, gate, Tev1 dataset export |
| `factrail` | the binary: the Claude Code hook protocol, `compact`, `eval`, `dataset`, `outputs` |
| `plugin/` | the Claude Code plugin: a shim that runs `factrail hook <event>` ([protocol](docs/protocol.md)) |

## Use

```sh
cargo build --release
# replay your own sessions, no model
./target/release/factrail eval --claude-projects ~/.claude/projects --max 40 --sim
# the committed gate
./target/release/factrail eval --synthetic 40 --gate eval/baseline.json
# compact one transcript (.jsonl from Claude Code, hook messages, or OpenAI chat)
./target/release/factrail compact session.jsonl --out compacted.json
# training records for a local judge, labelled by hindsight
./target/release/factrail dataset --claude-projects ~/.claude/projects --out data/tev-$(date +%F)
```

Judges and running one locally: [docs/local-judge.md](docs/local-judge.md). The decision
record: [docs/adr/ADR-001-factrail.md](docs/adr/ADR-001-factrail.md).

## The boundary

A session that used a fenced tool (by default the private email gateway, Gmail and the
`email-search` skill) is never sent to a model unless the endpoint is *declared* local.
Instead the rules compact it, and nothing leaves the process. The judge's state carries
tool-result sizes, never their contents. Requests are redacted for credential shapes, and
`egress = "metadata"` sends only the shape of tool arguments. Tainted sessions are never
written to the decision log or a dataset.

## Licence and provenance

MIT OR Apache-2.0. factrail re-implements, in Rust, rules first published under MIT by
`fast-jev-compaction`, `jev-factkeep-compaction` and `hermes-jev-compaction`; see
[NOTICE](NOTICE) and `LICENSES/`.
