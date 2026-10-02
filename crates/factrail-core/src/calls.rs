//! Tool calls paired with their results.

use serde_json::{Map, Value};

use crate::model::{Message, Role};
use crate::text::truncate;

/// A tool call paired with its result by `tool_use_id`.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    /// Short id used in the judge's state and question names (`t1`, `t2`, …).
    pub id: String,
    /// The call's session-unique id.
    pub tool_use_id: String,
    /// The tool's name.
    pub tool: String,
    /// The call's arguments.
    pub input: Map<String, Value>,
    /// Index of the message holding the `tool_use` block.
    pub call_index: usize,
    /// Index of the message holding the `tool_result` block.
    pub result_index: usize,
    /// Length of the result, in `char`s.
    pub result_chars: usize,
    /// True when the tool reported an error.
    pub is_error: bool,
    /// In the first message or the newest preserved ones: never a candidate.
    pub pinned: bool,
}

/// True for the first message and the newest `preserve_recent` messages.
///
/// ```
/// use factrail_core::is_pinned;
/// assert!(is_pinned(0, 10, 2));
/// assert!(!is_pinned(7, 10, 2));
/// assert!(is_pinned(8, 10, 2));
/// ```
pub fn is_pinned(index: usize, total: usize, preserve_recent: usize) -> bool {
    index == 0 || index + preserve_recent >= total
}

/// Pairs every `tool_use` with its `tool_result`. A call without a result is not
/// a candidate: there is nothing to drop yet.
pub fn collect_tool_calls(messages: &[Message], preserve_recent: usize) -> Vec<ToolCall> {
    let mut results = std::collections::HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        for result in &message.tool_results {
            results.insert(result.tool_use_id.as_str(), (index, result));
        }
    }
    let total = messages.len();
    let mut calls = Vec::new();
    for (call_index, message) in messages.iter().enumerate() {
        for tool in &message.tool_uses {
            let Some(&(result_index, result)) = results.get(tool.tool_use_id.as_str()) else {
                continue;
            };
            calls.push(ToolCall {
                id: format!("t{}", calls.len() + 1),
                tool_use_id: tool.tool_use_id.clone(),
                tool: tool.tool.clone(),
                input: tool.input.clone(),
                call_index,
                result_index,
                result_chars: crate::text::clen(&result.text),
                is_error: result.is_error,
                pinned: is_pinned(call_index, total, preserve_recent)
                    || is_pinned(result_index, total, preserve_recent),
            });
        }
    }
    calls
}

/// The last three user prompts (user messages carrying text and no tool results),
/// each cut to 500 `char`s: the default goal a judge is told the session is pursuing.
pub fn goal_from_messages(messages: &[Message]) -> String {
    let prompts: Vec<&Message> = messages
        .iter()
        .filter(|m| m.role == Role::User && !m.text.trim().is_empty() && m.tool_results.is_empty())
        .collect();
    let start = prompts.len().saturating_sub(3);
    prompts[start..]
        .iter()
        .map(|m| truncate(&m.text, 500))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ToolResult, ToolUse};

    fn call_msg(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            text: String::new(),
            tool_uses: vec![ToolUse {
                tool_use_id: id.into(),
                tool: "Bash".into(),
                input: Map::new(),
                text: None,
                is_error: false,
            }],
            tool_results: vec![],
        }
    }
    fn result_msg(id: &str, text: &str) -> Message {
        Message {
            role: Role::User,
            text: String::new(),
            tool_uses: vec![],
            tool_results: vec![ToolResult {
                tool_use_id: id.into(),
                text: text.into(),
                is_error: false,
            }],
        }
    }

    #[test]
    fn pairs_calls_and_pins_ends() {
        let mut ms = vec![Message::text(Role::User, "go")];
        for i in 0..5 {
            ms.push(call_msg(&format!("c{i}")));
            ms.push(result_msg(&format!("c{i}"), "out"));
        }
        ms.push(call_msg("dangling"));
        let calls = collect_tool_calls(&ms, 4);
        assert_eq!(calls.len(), 5);
        assert_eq!(calls[0].id, "t1");
        assert!(!calls[0].pinned);
        assert!(calls[4].pinned); // messages 9, 10 are in the newest 4 of 12
    }

    #[test]
    fn goal_is_last_three_prompts() {
        let ms: Vec<Message> = (0..5)
            .map(|i| Message::text(Role::User, format!("p{i}")))
            .collect();
        assert_eq!(goal_from_messages(&ms), "p2\np3\np4");
    }
}
