# The hook protocol (version 1)

The Claude Code plugin under `plugin/` holds no policy and no compaction logic. Each hook
it serves runs the `factrail` binary once through `$.process.run(argv, { stdin })`,
writes one JSON object to its stdin and reads one JSON object from its stdout. The
binary inherits the session's environment, so credentials (`TYPESAFE_API_KEY`,
`FACTRAIL_API_KEY`) are read by the binary and never cross the pipe.

Every request carries `"protocol": 1`. Every response carries `"protocol": 1` and
`"ok": true`, or `"ok": false` with `"error"`. The plugin treats a non-zero exit, a
timeout, unparsable stdout, a protocol mismatch or `"ok": false` exactly as it treats
a refusal: the built-in behaviour runs (`next(event)`) and one log line says why.
The binary exits 0 whenever it wrote a response, including `"ok": false`.

`config` in every request is the plugin's `userConfig` options exactly as the host
passed them; the binary resolves defaults, so the plugin never interprets an option.

## Messages

`messages` uses the engine's `SessionMessage` shape, less `toolUses[].result`
(the stored record, unused and often large), which the plugin strips:

```json
{ "role": "assistant", "text": "", "handle": "h17",
  "toolUses": [{ "tool_use_id": "toolu_1", "tool": "Bash", "input": {"command": "ls"},
                 "text": "a\nb", "isError": false }],
  "toolResults": [{ "tool_use_id": "toolu_1", "text": "a\nb", "isError": false }] }
```

A response's message list mixes two forms. `{"keep": i}` means "the engine's own
message `i` of the request, handle and all"; anything else is a rebuilt message with
no `handle`, which the engine reads as built.

## `factrail hook compact` — `session.compact`

Request:

```json
{ "protocol": 1, "config": {},
  "session": { "id": "…", "cwd": "/…", "trigger": "auto", "agentId": null },
  "instructions": null,
  "messages": [],
  "usage": { "tokens": 190000, "window": 1000000 },
  "store": { "enabled": null, "taint": null } }
```

Response:

```json
{ "protocol": 1, "ok": true,
  "action": "install",
  "messages": [{ "keep": 0 }, { "role": "user", "text": "…", "toolUses": [], "toolResults": [] }],
  "reason": "ok",
  "log": "factrail: kept 212/240 messages verbatim, no summary (…)",
  "toast": true,
  "store": { "taint": null, "last": "…" } }
```

`action` is one of:

| action | the plugin does |
|---|---|
| `install` | returns `{ messages }` mapped back through `keep` |
| `builtin` | calls `next(event)`, or `next({ ...event, instructions })` when `instructions` is present |
| `skip` | returns `{ skip: reason }` (a precompute: nothing is installed) |

`store` lists the plugin-store writes the plugin must make: `taint` (written to
`taint:<session id>` when not null) and `last` (the last outcome line). `reason`
is the log vocabulary: `ok`, `ok-local`, `ok-rules`, `switched-off`, `tainted`,
`no-key`, `below-gate`, `deadline`, `error`, `precompute`, `subagent`.

## `factrail hook turn` — `turn.complete`

Request: `config`, `usage` (`tokens`, `percent`, `window`), `baseline` (the stored
`baseline:<session id>` value or null), `rateLimits` (count), `hasApiKey`
(`ANTHROPIC_API_KEY` present), `event` (`reason`, `agentId`), `tools` (see below),
`store.taint`, `store.enabled`, `now` (ms since epoch).

Response: `compact` (bool), `reason`, `threshold`, `need`, `baseline` (a value to
write to `baseline:<session id>`, or null), `taint` (a value to write to
`taint:<session id>`, or null), `nudge` (`{ "delayMs": n, "mode": "compact" | "notify",
"tokens": n }` or null), `log` (a line, or null).

`tools` is the session's tool calls reduced to what the taint rule reads:
`[{ "tool": "mcp__x__y", "skill": null }]`, `skill` being `input.skill ?? input.name`
of a `Skill` call. The plugin sends this rather than the transcript, so a per-turn
check costs bytes, not megabytes.

## `factrail hook taint` — `tool.call` and `skill.prompt`

Request: `config`, `tool` (or null), `skill` (or null), `store.taint`, `now`.
Response: `taint` (a value to write, or null).

## `factrail hook status` — `/factrail on|off|status`

Request: `config`, `args`, `store` (`enabled`, `taint`, `last`), `usage`, `baseline`,
`tools`, `hasKey` (bool: a key the binary can see; the plugin sends nothing, the
binary checks its own environment).
Response: `text`, and `store.enabled` (bool to write, or null).

## Files the binary writes

| path | what | mode |
|---|---|---|
| `$XDG_CACHE_HOME/factrail/outputs/<session>/<tool_use_id>.txt` | full output of each reduced result, secrets masked | dir 0700, file 0600 |
| `$XDG_CACHE_HOME/factrail/answers/<session>.json` | the judge's answers per `tool_use_id`, so a call is never asked about twice; capped at 20,000, regenerable | 0600 |
| `$XDG_DATA_HOME/factrail/decisions/<YYYY-MM-DD>.jsonl` | each judged request and its answers, for evaluation and training; never for a tainted session | 0600 |

Saved outputs older than 30 days are deleted at most once a day. `$XDG_*` default to
`~/.cache` and `~/.local/share`.
