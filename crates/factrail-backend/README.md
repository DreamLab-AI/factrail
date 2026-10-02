# factrail-backend

The I/O half of [factrail](https://github.com/DreamLab-AI/factrail): the judges that
answer a [`factrail-core`](https://docs.rs/factrail-core) compaction plan, and the files
the `factrail` binary keeps.

## Judges

- **`SystemOneJudge`** — the TypeSafe System One wire format, hosted
  (`https://api.typesafe.ai/v1/systemone`) or behind a sovereign façade (keyless when local).
- **`TevJudge`** — a Tev1-format decision model (`togethercomputer/tev1`, open weights at
  `togethercomputer/Tev1-4B-experimental`) on any OpenAI-compatible `/chat/completions`
  endpoint. One call per question; the probability comes from the first token's
  log-probabilities. It is a much smaller model than Jev, trained on short states, and is
  not claimed to match it — measure it before trusting it.

`ask_all` runs a round concurrently under one deadline and drops every in-flight request
when it passes.

## Data boundary

Only the request's state and questions leave the process, after credential redaction (every
known credential shape plus the judge's own key). Tool-result contents never leave: the state
describes a result only by its size.

## Stores

| store | path | modes (unix) |
|---|---|---|
| `OutputStore` | `$XDG_CACHE_HOME/factrail/outputs/<session>/<tool_use_id>.txt` | dir 0700, file 0600 |
| `AnswerStore` | `$XDG_CACHE_HOME/factrail/answers/<session>.json` | dir 0700, file 0600 |
| `DecisionLog` | `$XDG_DATA_HOME/factrail/decisions/<YYYY-MM-DD>.jsonl` | dir 0700, file 0600 |

Saved outputs are redacted, capped at 8 MiB and expire after 30 days.

## Licence

MIT OR Apache-2.0, at your option. See `NOTICE` in the repository for provenance.
