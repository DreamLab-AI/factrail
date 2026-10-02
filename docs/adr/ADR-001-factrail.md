---
id: ADR-001
title: One Rust engine for verbatim compaction, fact rails by default, a local judge as a first-class backend
date: 2026-10-02
decision_status: accepted
implementation_status: complete
activation_status: staged
supersedes: []
owner: jjohare
review_trigger: a held-out fact rate below the committed baseline, a Claude Code function-hook API change, or a local judge whose replay rate falls below the rules-only rate
---

# ADR-001 — One Rust engine for verbatim compaction

## Context

The agentbox `jev-compaction` plugin (agentbox ADR-2093) vendored `fast-jev-compaction`'s
TypeScript library, which *erases* every call the judge scores stale. Two forks since went
further. `jev-factkeep-compaction` (Claude Code) replaced erasure with fact rails and
measured 304/336 preregistered facts kept against upstream's 36/336 across six blind
rounds. `hermes-jev-compaction` (Hermes Agent, Python) added the learned token-value
selector, a pooled fact budget, metadata-only egress, a no-model fallback and a hard
eviction line. In our estate, 42 compactions after the 180k trigger landed showed the
cost of the old rule. A compaction that could not reduce far enough fell through to the
lossy built-in summary, and the email taint fence sent every email-touching session there
too.

Together published Tev1 (Qwen3.5-4B, open weights) and its recipe. A local typed-decision
judge became practical. Our own sovereign façade (agentbox ADR-2094) speaks the same System
One wire format.

## Decision

1. **One engine in Rust**: `factrail-core` (pure), `factrail-policy` (estate policy),
   `factrail-backend` (judges and stores), `factrail-eval` (measurement and data) and the
   `factrail` binary. The Claude Code plugin is a protocol shim (`docs/protocol.md`) that runs
   the binary through `$.process.run`, so every rule is tested in one language and the same
   engine serves Hermes and Codex transcripts.
2. **Fact rails replace erasure.** Nothing is erased. Reads become re-run notes; observations
   keep head, fact lines and tail; fact lines are chosen by learned token value from one pooled
   budget. Every reduced output is saved, redacted, under `$XDG_CACHE_HOME/factrail/outputs`.
3. **Rules are the fallback, not the summary.** A session no model may see (taint), a missing
   key, or an unreachable judge gets the same rails with no model (`fallback = "rules"`). The
   built-in summary runs only when switched off, when nothing can be reduced, or when an
   operator sets `fallback = "summary"`. The taint fence still decides *egress* exactly as
   before: a tainted transcript reaches a model only when the backend is *declared* local.
4. **Pressure, not a fixed ratio.** The rails aim for 5/6 of the trigger. The oldest results
   give way only past the trigger itself. `minReductionRatio` stays as an optional override.
5. **Three judges.** System One (TypeSafe cloud or the sovereign façade), a Tev-format model on
   any OpenAI-compatible endpoint, and none (rules). The Tev prompt lives in `factrail_core::tev`,
   shared by the judge and the dataset export.
6. **Evaluation without an answer key.** A fact is a token a result introduced that the agent
   later wrote itself. `factrail eval` replays any corpus at cuts, against the erase rule under
   the same decisions. A deterministic synthetic corpus backs a committed gate the nightly dream
   cycle runs.
7. **Training labels by hindsight.** Whether a result was later used is a label no vendor
   produced, so a local judge can be trained from our own sessions. Teacher labels from a vendor
   judge are opt-in, pending that vendor's terms.

## Consequences

- More context stays resident than under erasure: the forks measured 42–56% of tokens
  remaining against 8–12%. That is the price of keeping facts. It is visible per compaction in
  the log line and is the first thing the review trigger watches.
- Email-tainted sessions are compacted by local rules instead of summarised. Nothing leaves the
  process on that path.
- The plugin needs a CLI host (`$.process.run`). The desktop and SDK hosts fail open to the
  built-in compaction.
- Replacing `jev-compaction` in agentbox is a separate change (the entrypoint registration and a
  manifest swap), recorded there.

## Verification

`cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`, the plugin's tests, and
`factrail eval --synthetic 40 --gate eval/baseline.json`.
