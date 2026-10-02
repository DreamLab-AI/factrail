//! Facts by hindsight: what a transcript's own later turns show the agent needed.
//!
//! A *fact* is a candidate token ([`factrail_core::value::toks`]: an id, a
//! version, a count, a path) that a tool result introduced and that the agent
//! later wrote itself — in a tool call's arguments or in its own reply. That is
//! the evidence a compaction must not destroy: the agent demonstrably acted on
//! it. No one has to pre-register anything, so every transcript is a test.

use std::collections::{HashMap, HashSet};

use factrail_core::model::input_json;
use factrail_core::value::toks;
use factrail_core::{Message, Role};

/// One fact: a token, where it came from and where the agent used it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fact {
    /// The token.
    pub token: String,
    /// The `tool_use_id` of the result that introduced it first.
    pub source: String,
    /// Index of the message holding that result.
    pub introduced: usize,
    /// Indices of the assistant messages that used it afterwards, ascending.
    pub uses: Vec<usize>,
}

impl Fact {
    /// True when the agent uses the fact at or after message `cut`, having learnt it before.
    pub fn needed_across(&self, cut: usize) -> bool {
        self.introduced < cut && self.uses.iter().any(|&u| u >= cut)
    }
}

/// The text an assistant message writes: its reply and its calls' arguments.
pub fn written_by_agent(message: &Message) -> String {
    let mut out = message.text.clone();
    for t in &message.tool_uses {
        out.push('\n');
        out.push_str(&input_json(&t.input));
    }
    out
}

/// Every fact of a transcript, in order of introduction.
///
/// ```
/// use factrail_eval::facts::facts;
/// use factrail_core::Message;
/// let ms: Vec<Message> = serde_json::from_str(r#"[
///   {"role":"assistant","text":"","toolUses":[{"tool_use_id":"a","tool":"Bash","input":{"command":"deploy"}}]},
///   {"role":"user","text":"","toolUses":[],"toolResults":[{"tool_use_id":"a","text":"release rel-48213 queued","isError":false}]},
///   {"role":"assistant","text":"","toolUses":[{"tool_use_id":"b","tool":"Bash","input":{"command":"status rel-48213"}}]}
/// ]"#).unwrap();
/// let f = facts(&ms);
/// assert_eq!(f.len(), 1);
/// assert_eq!(f[0].token, "rel-48213");
/// assert_eq!(f[0].uses, vec![2]);
/// ```
pub fn facts(messages: &[Message]) -> Vec<Fact> {
    let mut intro: HashMap<String, (String, usize)> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut uses: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, m) in messages.iter().enumerate() {
        match m.role {
            Role::User => {
                for r in &m.tool_results {
                    for t in toks(&r.text) {
                        if !intro.contains_key(&t) {
                            intro.insert(t.clone(), (r.tool_use_id.clone(), i));
                            order.push(t);
                        }
                    }
                }
            }
            Role::Assistant => {
                for t in toks(&written_by_agent(m)) {
                    if intro.get(&t).is_some_and(|(_, k)| *k < i) {
                        uses.entry(t).or_default().push(i);
                    }
                }
            }
        }
    }
    order
        .into_iter()
        .filter_map(|t| {
            let u = uses.remove(&t)?;
            let (source, introduced) = intro.remove(&t)?;
            Some(Fact {
                token: t,
                source,
                introduced,
                uses: u,
            })
        })
        .collect()
}

/// What a compacted transcript still shows the agent: its text, and the set of
/// candidate tokens in it for a fast first check.
pub struct Visible {
    text: String,
    tokens: HashSet<String>,
}

impl Visible {
    /// Indexes `messages`.
    pub fn new(messages: &[Message]) -> Self {
        let text = visible_text(messages);
        let tokens = toks(&text);
        Self { text, tokens }
    }

    /// True when `token` is visible: tokenised the same way, or anywhere as a substring.
    pub fn shows(&self, token: &str) -> bool {
        self.tokens.contains(token) || self.text.contains(token)
    }
}

/// Everything a compacted transcript still shows the agent, as one string.
pub fn visible_text(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        out.push_str(&m.text);
        out.push('\n');
        for t in &m.tool_uses {
            out.push_str(&input_json(&t.input));
            out.push('\n');
        }
        for r in &m.tool_results {
            out.push_str(&r.text);
            out.push('\n');
        }
    }
    out
}

/// Hindsight labels for one call at one cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Label {
    /// The agent later used a fact first introduced by this call's result, or re-ran the same call.
    pub keep_result: bool,
    /// `keep_result`, or the agent later wrote a token of this call's arguments.
    pub keep_call: bool,
}

/// Labels every call of `messages[..cut]` by what `messages[cut..]` shows.
pub fn labels(messages: &[Message], cut: usize) -> HashMap<String, Label> {
    let all = facts(messages);
    let needed: HashSet<&str> = all
        .iter()
        .filter(|f| f.needed_across(cut))
        .map(|f| f.source.as_str())
        .collect();
    let later: Vec<&Message> = messages[cut.min(messages.len())..]
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .collect();
    let later_text: String = later
        .iter()
        .map(|m| written_by_agent(m))
        .collect::<Vec<_>>()
        .join("\n");
    let reran: HashSet<(String, String)> = later
        .iter()
        .flat_map(|m| {
            m.tool_uses
                .iter()
                .map(|t| (t.tool.clone(), input_json(&t.input)))
        })
        .collect();
    let mut out = HashMap::new();
    for m in &messages[..cut.min(messages.len())] {
        for t in &m.tool_uses {
            let keep_result = needed.contains(t.tool_use_id.as_str())
                || reran.contains(&(t.tool.clone(), input_json(&t.input)));
            let keep_call = keep_result
                || toks(&input_json(&t.input))
                    .iter()
                    .any(|x| later_text.contains(x.as_str()));
            out.insert(
                t.tool_use_id.clone(),
                Label {
                    keep_result,
                    keep_call,
                },
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms() -> Vec<Message> {
        serde_json::from_str(
            r#"[
          {"role":"user","text":"go","toolUses":[]},
          {"role":"assistant","text":"","toolUses":[{"tool_use_id":"a","tool":"Bash","input":{"command":"curl http://h/jobs"}}]},
          {"role":"user","text":"","toolUses":[],"toolResults":[{"tool_use_id":"a","text":"job job-77123 failed at step build_42x","isError":false}]},
          {"role":"assistant","text":"","toolUses":[{"tool_use_id":"b","tool":"Read","input":{"file_path":"/src/config_main.rs"}}]},
          {"role":"user","text":"","toolUses":[],"toolResults":[{"tool_use_id":"b","text":"fn main() {}","isError":false}]},
          {"role":"assistant","text":"Retrying job-77123.","toolUses":[{"tool_use_id":"c","tool":"Read","input":{"file_path":"/src/config_main.rs"}}]}
        ]"#,
        )
        .unwrap()
    }

    #[test]
    fn facts_and_labels() {
        let m = ms();
        let f = facts(&m);
        assert_eq!(
            f.iter().map(|x| x.token.as_str()).collect::<Vec<_>>(),
            vec!["job-77123"]
        );
        assert!(f[0].needed_across(5));
        assert!(!f[0].needed_across(2));
        let l = labels(&m, 5);
        assert!(l["a"].keep_result);
        assert!(l["b"].keep_result, "re-read later");
        assert!(l["b"].keep_call);
    }
}
