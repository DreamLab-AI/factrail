//! Calls whose arguments carry the inline stub, and which of them broke.
//!
//! factrail up to 0.1.0 cut the arguments of earlier calls to `head` + `…[N chars]`,
//! and a model that saw enough of them wrote the same shape into new calls: a Bash
//! command or a file body that ends in the marker. This module scans Claude Code
//! transcripts for such calls and counts those whose result shows the call broke
//! (a shell parse error, an unknown command, or the stub echoed back). It is the
//! acceptance check for the fix: sessions started after it should have none.
//!
//! ```
//! use factrail_eval::stubs::scan_jsonl;
//! let stub = format!("cat a.txt\u{2026}[{} chars]", 90);
//! let jsonl = [
//!     serde_json::json!({"timestamp": "2026-11-09T10:00:00Z", "message": {"content": [
//!         {"type": "tool_use", "id": "u1", "name": "Bash", "input": {"command": stub}}]}}),
//!     serde_json::json!({"timestamp": "2026-11-09T10:00:01Z", "message": {"content": [
//!         {"type": "tool_result", "tool_use_id": "u1", "content": "bash: unexpected EOF"}]}}),
//! ]
//! .map(|v| v.to_string())
//! .join("\n");
//! let scan = scan_jsonl(&jsonl, None);
//! assert_eq!((scan.stubbed, scan.broken), (1, 1));
//! assert_eq!(scan_jsonl(&jsonl, Some("2026-11-10")).stubbed, 0);
//! ```

use std::collections::HashSet;

use serde_json::Value;

use factrail_core::rails::argument_stub;

/// How far into a result to look for a sign the call broke.
pub const RESULT_HEAD_CHARS: usize = 400;

/// What a broken stubbed call's result starts with.
const BROKEN_SIGNS: &[&str] = &[
    "unexpected EOF",
    "command not found",
    "SyntaxError",
    "unknown command",
];

/// One transcript's count.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileScan {
    /// Calls whose arguments carry the stub.
    pub stubbed: usize,
    /// Of those, calls whose result shows they broke.
    pub broken: usize,
}

/// Scan one Claude Code `.jsonl` transcript.
///
/// `since`, when given, keeps only calls whose entry `timestamp` is at or after it.
/// Claude Code writes UTC RFC 3339 timestamps (`2026-11-09T14:30:25.123Z`), so a
/// date (`2026-11-09`) or any prefix of that form compares correctly as text.
/// Entries without a timestamp are kept only when `since` is absent. Lines that
/// are not JSON are skipped.
pub fn scan_jsonl(text: &str, since: Option<&str>) -> FileScan {
    let mut stubbed: HashSet<String> = HashSet::new();
    let mut out = FileScan::default();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(parts) = entry
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        let recent = match since {
            None => true,
            Some(s) => entry
                .get("timestamp")
                .and_then(Value::as_str)
                .is_some_and(|t| t >= s),
        };
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("tool_use") if recent => {
                    let input = part.get("input").unwrap_or(&Value::Null);
                    if let (true, Some(id)) =
                        (argument_stub(input), part.get("id").and_then(Value::as_str))
                    {
                        stubbed.insert(id.to_owned());
                        out.stubbed += 1;
                    }
                }
                Some("tool_result") => {
                    let id = part.get("tool_use_id").and_then(Value::as_str);
                    if id.is_some_and(|id| stubbed.contains(id))
                        && broke(part.get("content").unwrap_or(&Value::Null))
                    {
                        out.broken += 1;
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Whether a result's head shows the call broke.
fn broke(content: &Value) -> bool {
    let text = match content {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let head: String = text.chars().take(RESULT_HEAD_CHARS).collect();
    BROKEN_SIGNS.iter().any(|s| head.contains(s)) || argument_stub(&Value::String(head))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stub() -> String {
        format!("python3 -c 'print(1)\u{2026}[{} chars]", 300)
    }

    fn lines(entries: &[Value]) -> String {
        entries
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn call(ts: &str, id: &str, command: &str) -> Value {
        json!({"timestamp": ts, "message": {"content": [
            {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": command}}]}})
    }

    fn result(id: &str, content: Value) -> Value {
        json!({"timestamp": "2026-11-09T12:00:00Z", "message": {"content": [
            {"type": "tool_result", "tool_use_id": id, "content": content}]}})
    }

    #[test]
    fn counts_stubbed_and_broken_separately() {
        let text = lines(&[
            call("2026-11-09T10:00:00Z", "a", &stub()),
            result(
                "a",
                json!([{"type": "text", "text": "SyntaxError: unterminated string"}]),
            ),
            call("2026-11-09T10:01:00Z", "b", &stub()),
            result("b", json!("1")),
            call("2026-11-09T10:02:00Z", "c", "ls"),
            result("c", json!("bash: foo: command not found")),
        ]);
        assert_eq!(
            scan_jsonl(&text, None),
            FileScan {
                stubbed: 2,
                broken: 1
            }
        );
    }

    #[test]
    fn an_echoed_stub_is_broken() {
        let text = lines(&[
            call("2026-11-09T10:00:00Z", "a", &stub()),
            result("a", json!(stub())),
        ]);
        assert_eq!(scan_jsonl(&text, None).broken, 1);
    }

    #[test]
    fn since_drops_older_calls_and_untimed_entries() {
        let mut untimed = call("x", "u", &stub());
        untimed.as_object_mut().unwrap().remove("timestamp");
        let text = lines(&[
            call("2026-11-08T23:59:59Z", "old", &stub()),
            result("old", json!("unexpected EOF")),
            untimed,
            call("2026-11-09T00:00:00Z", "new", &stub()),
            result("new", json!("unexpected EOF")),
        ]);
        assert_eq!(
            scan_jsonl(&text, None),
            FileScan {
                stubbed: 3,
                broken: 2
            }
        );
        assert_eq!(
            scan_jsonl(&text, Some("2026-11-09")),
            FileScan {
                stubbed: 1,
                broken: 1
            }
        );
    }

    #[test]
    fn junk_lines_are_skipped() {
        assert_eq!(
            scan_jsonl("not json\n{}\n{\"message\":{}}", None),
            FileScan::default()
        );
    }
}
