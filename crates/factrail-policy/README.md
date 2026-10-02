# factrail-policy

The estate policy of [factrail](https://github.com/DreamLab-AI/factrail) context
compaction, as pure functions: no I/O, no async, no clock or environment reads.

- **Options** — `Config::from_options` resolves the plugin's options object, numbers
  given as JSON numbers or as numeric strings (shell-projected configuration).
- **Taint fence** — a session that has used an email tool (configurable prefixes and
  skills) is never sent to a judge off this network. Taint is sticky across
  compactions, and locality (`backendLocal`) is declared by the operator, never
  inferred from a URL.
- **Judge** — `decide` chooses the model, the deterministic fact-rail rules (which
  send nothing anywhere) or Claude Code's built-in summary, with a named reason.
- **Trigger** — a token trigger as well as a percentage, with hysteresis after each
  compaction, and a cache-warm nudge timed against the prompt-cache TTL.
- **Scope** — only the main conversation's real compactions are judged.

```rust
use factrail_policy::{Config, Judge, ToolRef, decide, scan_taint};

let config = Config::default();
let tools = [ToolRef { tool: "mcp__email-gateway__ask_email".into(), skill: None }];
let verdict = decide(true, true, &scan_taint(&tools, &config), &config);
assert_eq!(verdict.judge, Judge::Rules);
assert_eq!(verdict.reason.as_str(), "tainted");
```

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT licence](LICENSE-MIT) at your option.
