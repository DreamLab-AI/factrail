# Dream-cycle ledger

One row per night the dream engine runs on this repository. The `gate` evaluator replays
the synthetic corpus against `eval/baseline.json`. A baseline moves only in a commit that
says why.

| date | slot | verdict | finding |
|---|---|---|---|
| 2026-10-03 | hook-policy | VETOED: Given a session whose only fence-relevant event is a `Skill` call loadin | NONE | NONE | yes | BLOCKED-ENV |  | e958dccddec6 |  |  |  |
| 2026-10-04 | evaluation-and-data | No candidate patch; baseline gate, lint and tests passed | NONE | NONE | yes | INCONCLUSIVE |  | ec7fee20a5f5 |  |  |  |
| 2026-10-04 | evaluation-and-data | VETOED: Given `facts::labels` tests each argument token against later assistant  | NONE | NONE | yes | INCONCLUSIVE |  | 6e7f4cc0dc24 |  |  |  |
| 2026-10-05 | fact-rails | Tier-0 read_keep hypothesis untested: no candidate patch; baseline all passed | NONE | NONE | yes | INCONCLUSIVE |  | 81946cf04eff |  |  |  |
| 2026-10-06 | token-value-and-pooling | VETOED: Given a token that `toks` admits from a result (e.g. `9f3c2ab1`), when ` | NONE | VETOED | yes | REJECT |  | 834133dd7868 |  |  |  |
| 2026-10-07 | judge-and-egress | Egress::Metadata leaks no argument value at any fit_state stage; pinned by test | NONE | https://github.com/DreamLab-AI/factrail/pull/1 | yes | ACCEPT |  | 6fb5c147007d |  |  |  |
| 2026-10-08 | hook-policy | VETOED: Skill-call taint routed through skill_taints; qualified skill names now… | NONE | NONE | yes | BLOCKED-ENV |  | 591bca951eb8 |  |  |  |
| 2026-10-09 | evaluation-and-data | Manifest now counts records per split AND label source; teacher share witnessed | NONE | https://github.com/DreamLab-AI/factrail/pull/2 | yes | ACCEPT |  | 81c488912ce7 |  |  |  |
| 2026-10-10 | fact-rails | VETOED: taints_tool delegates Skill matching to skill_taints; qualified names… | NONE | NONE | yes | BLOCKED-ENV |  | 22e18ac94194 |  |  |  |
| 2026-10-11 | token-value-and-pooling | ctx.input/ctx.user naming strictly raises a token's value; pinned by test | NONE | https://github.com/DreamLab-AI/factrail/pull/3 | yes | ACCEPT |  | e832955e5bd3 |  |  |  |
| 2026-10-13 | hook-policy | Skill-call taint routed via skill_taints; qualified skill names now taint | NONE | https://github.com/DreamLab-AI/factrail/pull/4 | yes | ACCEPT |  | 39e868464946 |  |  |  |
| 2026-10-14 | evaluation-and-data | corpus::tainted fences qualified skill loads; pinned by test for ADR-001 routing | NONE | https://github.com/DreamLab-AI/factrail/pull/5 | yes | ACCEPT |  | 21b5791ab57a |  |  |  |
| 2026-10-15 | fact-rails | Errored reproducible reads rail as observations, never rerun notes; pinned | NONE | https://github.com/DreamLab-AI/factrail/pull/6 | yes | ACCEPT |  | 2f9099b288df |  |  |  |
