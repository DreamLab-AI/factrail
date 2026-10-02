//! Transcript formats converted into [`Message`]s.
//!
//! * Claude Code session files (`~/.claude/projects/<project>/<session>.jsonl`):
//!   one JSON entry per line; an assistant turn is streamed as several entries
//!   sharing `message.id`, one content block each, and parallel tool results
//!   arrive as separate user entries. Both are merged back into one message.
//!   Subagent (sidechain) and harness-injected (meta) entries are skipped.
//! * OpenAI-style chat (Hermes Agent, Codex and most OpenAI-compatible agents):
//!   `role` / `content` / `tool_calls`, with results as `role: "tool"` messages.
//!   System and developer texts are returned apart, since they are not turns.

use serde_json::{Map, Value};

use crate::model::{Message, Role, ToolResult, ToolUse};

fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p {
                Value::String(s) => Some(s.as_str()),
                Value::Object(o)
                    if o.get("type")
                        .and_then(Value::as_str)
                        .is_none_or(|t| t == "text") =>
                {
                    o.get("text").and_then(Value::as_str)
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn as_map(v: Option<&Value>) -> Map<String, Value> {
    match v {
        Some(Value::Object(m)) => m.clone(),
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(m)) => m,
            _ => {
                let mut m = Map::new();
                m.insert("raw".into(), Value::String(s.clone()));
                m
            }
        },
        _ => Map::new(),
    }
}

/// Parses a Claude Code session file. Lines that are not JSON, and entries that
/// are not user or assistant turns of the main conversation, are skipped.
///
/// ```
/// use factrail_core::formats::from_claude_jsonl;
/// let jsonl = r#"{"type":"user","message":{"role":"user","content":"list it"}}
/// {"type":"assistant","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"Sure."}]}}
/// {"type":"assistant","message":{"id":"m1","role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}
/// {"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"a\nb"}]}}"#;
/// let ms = from_claude_jsonl(jsonl);
/// assert_eq!(ms.len(), 3);
/// assert_eq!(ms[1].text, "Sure.");
/// assert_eq!(ms[1].tool_uses[0].tool, "Bash");
/// assert_eq!(ms[2].tool_results[0].text, "a\nb");
/// ```
pub fn from_claude_jsonl(jsonl: &str) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    let mut last_assistant_id: Option<String> = None;
    let mut last_was_results = false;
    for line in jsonl.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(kind, "user" | "assistant")
            || entry.get("isSidechain").and_then(Value::as_bool) == Some(true)
            || entry.get("isMeta").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let Some(message) = entry.get("message") else {
            continue;
        };
        let content = message.get("content").cloned().unwrap_or(Value::Null);
        if kind == "assistant" {
            let id = message.get("id").and_then(Value::as_str).map(str::to_owned);
            let mut text = String::new();
            let mut uses = Vec::new();
            if let Value::Array(blocks) = &content {
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            text.push_str(b.get("text").and_then(Value::as_str).unwrap_or(""))
                        }
                        Some("tool_use") => uses.push(ToolUse {
                            tool_use_id: b
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                            tool: b
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                            input: as_map(b.get("input")),
                            text: None,
                            is_error: false,
                        }),
                        _ => {}
                    }
                }
            } else {
                text = content_text(&content);
            }
            let merge = id.is_some()
                && id == last_assistant_id
                && out.last().is_some_and(|m| m.role == Role::Assistant);
            if merge {
                let m = out.last_mut().expect("checked");
                m.text.push_str(&text);
                m.tool_uses.extend(uses);
            } else {
                out.push(Message {
                    role: Role::Assistant,
                    text,
                    tool_uses: uses,
                    tool_results: Vec::new(),
                });
            }
            last_assistant_id = id;
            last_was_results = false;
            continue;
        }
        let mut text = String::new();
        let mut results = Vec::new();
        match &content {
            Value::Array(blocks) => {
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("tool_result") => results.push(ToolResult {
                            tool_use_id: b
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned(),
                            text: content_text(b.get("content").unwrap_or(&Value::Null)),
                            is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                        }),
                        Some("text") => {
                            text.push_str(b.get("text").and_then(Value::as_str).unwrap_or(""))
                        }
                        _ => {}
                    }
                }
            }
            other => text = content_text(other),
        }
        last_assistant_id = None;
        if results.is_empty() && text.is_empty() {
            continue;
        }
        let only_results = !results.is_empty() && text.trim().is_empty();
        if only_results && last_was_results && out.last().is_some_and(|m| m.role == Role::User) {
            out.last_mut()
                .expect("checked")
                .tool_results
                .extend(results);
        } else {
            out.push(Message {
                role: Role::User,
                text,
                tool_uses: Vec::new(),
                tool_results: results,
            });
        }
        last_was_results = only_results;
    }
    attach_outcomes(&mut out);
    out
}

/// Mirrors each result's text and error flag onto its `tool_use`, as Claude Code's
/// session messages carry them.
fn attach_outcomes(messages: &mut [Message]) {
    let outcomes: std::collections::HashMap<String, (String, bool)> = messages
        .iter()
        .flat_map(|m| {
            m.tool_results
                .iter()
                .map(|r| (r.tool_use_id.clone(), (r.text.clone(), r.is_error)))
        })
        .collect();
    for m in messages.iter_mut() {
        for t in &mut m.tool_uses {
            if let Some((text, is_error)) = outcomes.get(&t.tool_use_id) {
                t.text = Some(text.clone());
                t.is_error = *is_error;
            }
        }
    }
}

/// An OpenAI-style chat transcript as [`Message`]s, plus its system and
/// developer texts. Consecutive `tool` messages become one user message.
///
/// ```
/// use factrail_core::formats::from_openai_chat;
/// let chat = serde_json::json!([
///   {"role": "system", "content": "You are a coding agent."},
///   {"role": "user", "content": "run tests"},
///   {"role": "assistant", "content": null, "tool_calls": [
///     {"id": "c1", "type": "function", "function": {"name": "terminal", "arguments": "{\"command\":\"npm test\"}"}}]},
///   {"role": "tool", "tool_call_id": "c1", "content": "12 passed"}
/// ]);
/// let (ms, system) = from_openai_chat(chat.as_array().unwrap());
/// assert_eq!(system, vec!["You are a coding agent."]);
/// assert_eq!(ms[1].tool_uses[0].input["command"], "npm test");
/// assert_eq!(ms[2].tool_results[0].text, "12 passed");
/// ```
pub fn from_openai_chat(chat: &[Value]) -> (Vec<Message>, Vec<String>) {
    let mut out: Vec<Message> = Vec::new();
    let mut system = Vec::new();
    let mut last_tool = false;
    for m in chat {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        let text = content_text(m.get("content").unwrap_or(&Value::Null));
        match role {
            "system" | "developer" => {
                system.push(text);
                last_tool = false;
            }
            "assistant" => {
                let uses = m
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|c| {
                                let f = c.get("function").unwrap_or(c);
                                ToolUse {
                                    tool_use_id: c
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_owned(),
                                    tool: f
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or("unknown_tool")
                                        .to_owned(),
                                    input: as_map(
                                        f.get("arguments").or_else(|| c.get("arguments")),
                                    ),
                                    text: None,
                                    is_error: false,
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(Message {
                    role: Role::Assistant,
                    text,
                    tool_uses: uses,
                    tool_results: Vec::new(),
                });
                last_tool = false;
            }
            "tool" => {
                let result = ToolResult {
                    tool_use_id: m
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    text,
                    is_error: m.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                };
                if last_tool {
                    out.last_mut()
                        .expect("a tool message precedes")
                        .tool_results
                        .push(result);
                } else {
                    out.push(Message {
                        role: Role::User,
                        text: String::new(),
                        tool_uses: Vec::new(),
                        tool_results: vec![result],
                    });
                }
                last_tool = true;
            }
            _ => {
                out.push(Message::text(Role::User, text));
                last_tool = false;
            }
        }
    }
    attach_outcomes(&mut out);
    (out, system)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_merges_parallel_results_and_skips_sidechains() {
        let jsonl = [
            r#"{"type":"user","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"a","name":"Read","input":{"file_path":"x"}}]}}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"b","name":"Read","input":{"file_path":"y"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"a","content":[{"type":"text","text":"A"}]}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"b","content":"B","is_error":true}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"id":"s","content":[{"type":"text","text":"sub"}]}}"#,
            r#"{"type":"user","isMeta":true,"message":{"content":"Caveat"}}"#,
            r#"{"type":"attachment"}"#,
            "not json",
        ]
        .join("\n");
        let ms = from_claude_jsonl(&jsonl);
        assert_eq!(ms.len(), 3);
        assert_eq!(ms[1].tool_uses.len(), 2);
        assert_eq!(ms[2].tool_results.len(), 2);
        assert!(ms[2].tool_results[1].is_error);
        assert_eq!(ms[1].tool_uses[1].text.as_deref(), Some("B"));
    }

    #[test]
    fn openai_groups_tool_messages_and_parses_arguments() {
        let chat = serde_json::json!([
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "c1", "function": {"name": "read_file", "arguments": "{\"path\":\"a\"}"}},
                {"id": "c2", "function": {"name": "terminal", "arguments": "not json"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": [{"type": "text", "text": "A"}]},
            {"role": "tool", "tool_call_id": "c2", "content": "B"}
        ]);
        let (ms, _) = from_openai_chat(chat.as_array().unwrap());
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[1].tool_results.len(), 2);
        assert_eq!(ms[0].tool_uses[1].input["raw"], "not json");
    }
}
