# Running a local judge

factrail asks a judge two questions per old tool call. Three judges are supported
(`backend` in the plugin's options):

| backend | where it runs | what leaves the machine |
|---|---|---|
| `systemone` | TypeSafe's cloud (default URL), or any endpoint speaking the System One wire format, such as the agentbox sovereign façade | the redacted state and questions, unless `baseUrl` points at a local service |
| `tev` | a Tev-format decision model on any OpenAI-compatible server | nothing, when that server is local |
| `rules` | nowhere: no model | nothing |

`backendLocal = true` is a *declaration* that the endpoint is on your own network. It is
the only thing that lets an email-tainted session reach a model, so it is never inferred
from a URL.

## Tev1 on vLLM

Tev1-4B-experimental is a LoRA fine-tune of Qwen3.5-4B, published with open weights at
`togethercomputer/Tev1-4B-experimental`. It returns one option letter. factrail reads the
letter's log-probabilities to get the probability that a statement holds.

```sh
vllm serve togethercomputer/Tev1-4B-experimental \
  --served-model-name tev1 --max-model-len 8192 --enable-prefix-caching
```

Then set the plugin's options to:

```json
{ "backend": "tev", "baseUrl": "http://<host>:8000/v1", "model": "tev1", "backendLocal": true }
```

Prefix caching matters. Every question about one compaction carries the same state, so
only the short question tail is new work.

**Know what you are running.** Tev1 was trained on short states (2,048-token sequences)
and general classification data, not on compaction. Our states are fitted to
`maxStateTokens` (25,000 by default). Lower it for this judge, for example
`"maxStateTokens": 6000`, and measure the result before trusting it:

```sh
factrail eval --claude-projects ~/.claude/projects --max 40 --judge tev \
  --base-url http://<host>:8000/v1 --model tev1
```

The replay reports the fact rate next to the rules-only and erase-rule rates. If a judge
keeps fewer facts than `--judge rules`, it is worse than no judge at all.

## Training a successor

`factrail dataset --claude-projects ~/.claude/projects --out data/tev-YYYY-MM-DD` writes
Tev1-format records labelled by hindsight. A result is labelled "keep" when the agent
later used a fact it introduced, or re-ran the call. Tainted sessions never enter the
dataset. Splits are by session.

The records go through tev1's own toolchain to be rendered with the model's chat
template and trained (`examples/train_together.py --data <dir>/instruction`, or a local
LoRA run on the same settings). Rendering the template needs the model's tokenizer,
which is why it stays in that Python toolchain.

`--teacher` adds labels from recorded judge answers. Check the judge vendor's terms
before training on its outputs.

## In the nightly cycle

`dream.config.json` declares the evaluators the dream engine runs on the annexe:

- `tests`: the workspace tests.
- `gate`: the synthetic replay against `eval/baseline.json`. It is deterministic, needs
  no private data, and its output depends on the rails code, so it is a live evaluator.

A night that changes the rails must pass the gate. The local-judge loop runs on the
machine that holds the sessions:

1. Export the day's hindsight dataset.
2. Train weekly.
3. Replay the candidate against the current judge and the rules.
4. Promote the candidate only when its fact rate is no lower.
