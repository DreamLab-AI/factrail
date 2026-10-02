//! The transcript model.
//!
//! [`Message`] is the shape Claude Code's function hooks hand a `session.compact`
//! hook (`SessionMessage`, less the stored tool record), so a hook's transcript
//! deserialises into it unchanged. Other transcript formats convert into it
//! (see [`crate::formats`]).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Who wrote a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The user, or the harness on the user's turn (tool results arrive here).
    User,
    /// The model.
    Assistant,
}

/// A `tool_use` block of an assistant message.
///
/// `text` and `is_error` mirror the call's outcome once the transcript holds it;
/// Claude Code attaches them, other formats may not.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolUse {
    /// The call's id, unique across the session.
    pub tool_use_id: String,
    /// The tool's name (`Read`, `Bash`, `mcp__server__tool`).
    pub tool: String,
    /// The arguments the model gave it, in the order the model wrote them.
    #[serde(default)]
    pub input: Map<String, Value>,
    /// The result as the model read it, when the transcript mirrors it here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// True when the tool reported an error.
    #[serde(rename = "isError", default, skip_serializing_if = "is_false")]
    pub is_error: bool,
}

/// A `tool_result` block of a user message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// The id of the call this result answers.
    pub tool_use_id: String,
    /// The result as the model read it (text blocks joined).
    #[serde(default)]
    pub text: String,
    /// True when the tool reported an error.
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

/// One transcript message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who wrote it.
    pub role: Role,
    /// Its text blocks joined; empty when it has none.
    #[serde(default)]
    pub text: String,
    /// The tool calls of an assistant message.
    #[serde(rename = "toolUses", default)]
    pub tool_uses: Vec<ToolUse>,
    /// The tool results of a user message.
    #[serde(rename = "toolResults", default, skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<ToolResult>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Message {
    /// A message with text only.
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
            tool_uses: Vec::new(),
            tool_results: Vec::new(),
        }
    }

    /// Characters of text, tool input (as compact JSON) and tool output the message holds:
    /// the measure every reduction ratio in this crate is taken in.
    ///
    /// ```
    /// use factrail_core::{Message, Role};
    /// assert_eq!(Message::text(Role::User, "héllo").chars(), 5);
    /// ```
    pub fn chars(&self) -> usize {
        let mut total = crate::text::clen(&self.text);
        for tool in &self.tool_uses {
            total += crate::text::clen(&input_json(&tool.input));
        }
        for result in &self.tool_results {
            total += crate::text::clen(&result.text);
        }
        total
    }
}

/// Characters of a whole transcript, as [`Message::chars`] counts them.
pub fn transcript_chars(messages: &[Message]) -> usize {
    messages.iter().map(Message::chars).sum()
}

/// Compact JSON of a tool input, keys in the model's order.
pub fn input_json(input: &Map<String, Value>) -> String {
    serde_json::to_string(input).unwrap_or_else(|_| "[unserializable input]".to_owned())
}
