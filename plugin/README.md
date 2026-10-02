# factrail — Claude Code plugin

A thin function-hook shim over the native `factrail` binary. Every policy and compaction
decision (taint, trigger, hysteresis, cache-warm timing, which judge runs, what is kept)
lives in the binary. For each hook the plugin runs `<binary> hook <name>` once through
`$.process.run`, writes one JSON request to its stdin, reads one JSON response from its
stdout and applies it. The contract is [`../docs/protocol.md`](../docs/protocol.md)
(protocol version 1).

## What each hook does

| hook | runs | applies |
|---|---|---|
| `session.start` | — | registers `/factrail`; prunes this plugin's `taint:`/`baseline:` store keys whose `at` is over 30 days old (see below) |
| `tool.call`, `skill.prompt` | `factrail hook taint` | writes `taint:<session>` when the binary returns one. Never blocks the call; any failure is logged and swallowed |
| `command.run` `/factrail on\|off\|status` | `factrail hook status` | writes `enabled` when the binary returns a boolean; shows its `text`, or says the binary failed and why |
| `turn.start` | — | bumps the turn generation and cancels any pending cache-warm timer |
| `turn.complete` (main loop only) | `factrail hook turn` | writes `baseline:`/`taint:`; logs `log`; on `compact` runs `$.session.compact()`; on `nudge` arms one `$.clock.after(delayMs)` timer that toasts (`notify`) or compacts (`compact`) only if no newer turn began and nothing is compacting |
| `session.compact` | `factrail hook compact` | `install` → `{ messages }` with each `{keep: i}` mapped to the engine's own message `i`; `builtin` → `next(event)` (with the binary's `instructions` if given); `skip` → `{ skip }`. Writes `taint:` and `last`, logs and toasts |

**Fail-open.** A non-zero exit, a timeout, a spawn failure, unparsable stdout, `protocol`
other than `1`, `ok` other than `true`, or a response of the wrong shape is a failure; a
failed compaction runs Claude Code's built-in compaction (`next(event)`) with one
`factrail:` log line, and a failed turn or taint check changes nothing. An `install` list
is checked whole before it is returned: every `keep` in range and used once, every built
message `user`/`assistant` with string `text`, a `toolUses` array and no `handle`. One
defect rejects the list, so a malformed answer can never corrupt a session.

**Hysteresis.** After any compaction of the main conversation that is not a skip, the
plugin writes `baseline:<session>` = `{ pending: true, at }`; the binary reads the
post-compaction size at the next turn. A subagent's own compaction leaves it alone.

**Store pruning stays in the plugin.** It is housekeeping of this plugin's store, not
policy: the binary only ever sees one session's entries, so it cannot see the dead
sessions that need pruning. The rule is the old plugin's: a `taint:`/`baseline:` entry
whose `at` is missing or older than 30 days is deleted at session start.

**Timeouts.** `compact`: the binary's own deadline (`compactionTimeoutMs`, default
15 000 ms) plus 10 s for it to fall back and answer, capped at ten minutes. `taint`: 5 s.
`turn`, `status`: 10 s.

## Options (`userConfig`)

The plugin forwards these to the binary untouched, as `config` in every request; the
binary resolves every default. Defaults below match the binary's.

| option | default | meaning |
|---|---|---|
| `binary` | `factrail` | executable the hooks run; agentbox projects `/opt/agentbox/bin/factrail` |
| `apiKey` | — | sensitive; unset ⇒ the binary reads `FACTRAIL_API_KEY` / `TYPESAFE_API_KEY` |
| `enabledByDefault` | `true` | starting position of `/factrail` until first flipped |
| `backend` | `systemone` | `systemone`, `tev` or `rules` (no model, nothing sent) |
| `baseUrl` | — | backend endpoint; unset ⇒ the backend's default |
| `backendLocal` | `false` | declared, never inferred: true lets a tainted session reach the backend |
| `model` | — | model id; unset ⇒ the backend's default |
| `egress` | `full` | `full` or `metadata` (shapes and sizes only); any other value ⇒ `metadata` |
| `fallback` | `rules` | when the model may not be used: `rules` or `summary` (built-in) |
| `taintTools` | email gateway, Gmail | tool prefixes that taint: never sent to a non-local model; the local rules compact it instead |
| `taintSkills` | `email-search` | skills whose load taints, likewise |
| `keepThreshold` | `0.5` | keep probability below which an item is removed or truncated |
| `preserveRecentMessages` | `6` | newest messages never judged |
| `maxStateTokens` | `25000` | state budget per backend request |
| `maxRequestTokens` | `30000` | whole-request budget |
| `truncateHeadChars` | `200` | head kept of a reduced result |
| `compactAtPercent` | `60` | trigger ceiling as % of the window |
| `compactAtTokens` | `180000` | absolute trigger; effective = min of the two |
| `rearmTokens` | `40000` | growth past the post-compaction size before re-triggering |
| `cacheWarm` | `compact` | idle-before-cache-expiry action: `compact`, `notify`, `off` |
| `cacheWarmFloorTokens` | `100000` | no nudge below this |
| `cacheTtlSeconds` | `0` | prompt-cache TTL; 0 ⇒ detected |
| `cacheTtlMarginSeconds` | `300` | nudge fires this long before expiry |
| `compactionTimeoutMs` | `15000` | deadline on the backend round |
| `saveFullOutputs` | `true` | keep each reduced output under `$XDG_CACHE_HOME/factrail/outputs` |
| `recordDecisions` | `true` | append judged requests to `$XDG_DATA_HOME/factrail/decisions`; never for a tainted session |

## Install

Function hooks need `CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1` in `~/.claude/settings.json`'s
`env`. Register this directory's parent as a marketplace and enable the plugin:

```sh
claude plugin marketplace add <path-to-factrail-checkout>
claude plugin install factrail@<marketplace-name>
```

The `factrail` binary must be on `PATH`, or set the `binary` option to its path.

## Development

```sh
cd plugin
npm install --include=dev                 # TypeScript 5 into plugin/node_modules (gitignored)
npx tsc -p tsconfig.json                  # type-check; needs .claude/types/claude-code.d.ts (below)
claude plugin test .                      # engine tests (fake binary beneath the hooks) + protocol unit tests
claude plugin validate --strict .         # manifest and hooks module
CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 claude --plugin-dir .
```

`tests/factrail.test.ts` drives the hooks under the real engine with `process.run`
answered by a scripted fake binary; `tests/protocol.test.ts` covers `hooks/protocol.ts`,
the pure request/response half. `.claude/types/claude-code.d.ts` is the engine's type
declaration. Claude Code writes it into a plugin's `.claude/types/` when you author
hooks with it; it is Claude Code's file, so it is not committed. Only `tsc` needs it:
`claude plugin test` and `claude plugin validate` run without it. Tested under
Claude Code 2.1.285.
