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

## Disposition — 2026-10-17

ADR-001 completed by PR #4 (hook-policy, 2026-10-13): all skill-name taint matching now goes
through `skill_taints` (qualification-tolerant, fail-closed). The 2026-10-03 "already
implemented" assessment did not hold at `6bd15844`: the `Skill`-call branch of `taints_tool`
(`crates/factrail-policy/src/taint.rs`) was a bare-equality second matcher until PR #4, so a
plugin-qualified call such as `agentbox:email-search` did not taint while its expansion did.
`implementation_status` stays `complete`, now true. Pinned by
`a_qualified_skill_call_taints_exactly_like_its_expansion` (`crates/factrail-policy/tests/policy.rs`);
PR #5 extends the same fence to `corpus::tainted` in `factrail-eval`.

## Amendment — 2026-11-09: arguments are never rewritten

Reduction rewrote the arguments of `drop_call` calls (cut to 200 chars) and of every
reproducible read (160 chars) to an inline stub: the head, an ellipsis, a bracketed count
of chars. In long sessions most long arguments in context took that shape, and the model
reproduced it in new long arguments: Bash commands that failed with `unexpected EOF` or
`command not found`, files written with the stub in them. A scan of one estate's Claude
Code transcripts on 2026-11-09 found 443 broken calls in 72 sessions (agentbox ADR-2121,
"Finding — 2026-11-09"). The tools do not truncate; the copy is in-context imitation.

Decision: `apply` reduces results only. `Action::DropCall` is kept for recorded decisions
and judge answers, and is applied exactly as `DropResult`. `brief_input`, `brief_map`,
`INPUT_BRIEF_CHARS` and `RERUN_INPUT_CHARS` are removed. The `taint` hook, already called
on every tool call, now also receives the call's arguments and refuses a call whose
arguments carry the stub (`rails::argument_stub`), because pre-fix transcripts keep
seeding the copy until their stubs leave the context. The refusal names the stub in words,
never in its shape. The replay metric (`factrail-eval`) still models `DropCall` as removing
the call; it is left unchanged so the promotion-gate baselines stay comparable.

Pinned by `drop_call_keeps_the_arguments` (`crates/factrail-core/src/apply.rs`), the
`argument_stub` doctest, `taint_event_and_status` (`crates/factrail/tests/hook.rs`) and the
plugin's `taint at the source` tests.
