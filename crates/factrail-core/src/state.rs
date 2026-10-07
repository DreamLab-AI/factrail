//! The state a judge sees: the whole conversation, tool results omitted, fitted
//! into a token budget.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::calls::{ToolCall, goal_from_messages, is_pinned};
use crate::model::{Message, Role, input_json};
use crate::text::{abridge, clen, truncate};
use crate::tokens::estimate_tokens;

/// The framing every request carries, telling the judge what the history is and
/// what a "no" costs.
pub const STATE_CONTEXT: &str = "A coding assistant conversation is being compacted to free context. `history` is the whole conversation so far, oldest first; tool outputs are replaced by a short `result` note and long texts may be abridged. Each question asks whether one tool call, or the full output of that call, still needs to stay in the history verbatim. Whatever is not kept is deleted permanently, but the assistant can always re-run a tool or re-read a file.";

/// Successive caps on the serialised tool input included per call.
const INPUT_CHARS: [usize; 3] = [1000, 200, 60];
const TEXT_HEAD: usize = 400;
const TEXT_TAIL: usize = 150;

/// What of a tool call's arguments may leave the process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Egress {
    /// Arguments as JSON, capped per fitting stage (1000, 200, then 60 `char`s).
    #[default]
    Full,
    /// Only the arguments' shape: kind, key names, item count and length. No
    /// argument value is serialised into the request.
    Metadata,
}

/// The arguments' shape without their values, for [`Egress::Metadata`].
///
/// ```
/// use factrail_core::metadata_input;
/// let input = serde_json::json!({"path": "/etc/hosts", "limit": 5});
/// let meta = metadata_input(input.as_object().unwrap());
/// assert_eq!(meta, serde_json::json!({"kind": "object", "keys": ["limit", "path"], "chars": 31}));
/// ```
pub fn metadata_input(input: &Map<String, Value>) -> Value {
    let mut keys: Vec<&String> = input.keys().collect();
    keys.sort();
    serde_json::json!({ "kind": "object", "keys": keys, "chars": clen(&input_json(input)) })
}

/// The fitted state, ready to send.
#[derive(Clone, Debug, PartialEq)]
pub struct FittedState {
    /// `{ context, goal, history }` as JSON.
    pub state: Value,
    /// Estimated tokens of the serialised state.
    pub tokens: usize,
    /// Which fitting stage produced it, for diagnostics.
    pub stage: String,
}

/// The history could not be fitted even after every stage.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("history too large for the judge (~{tokens} tokens after truncation, limit {limit})")]
pub struct TooLarge {
    /// Tokens left after the last stage.
    pub tokens: usize,
    /// The budget it had to fit.
    pub limit: usize,
}

/// What fitting needs to know.
#[derive(Clone, Debug)]
pub struct FitOptions {
    /// Token ceiling for the state.
    pub max_state_tokens: usize,
    /// Newest messages pinned (the first is always pinned).
    pub preserve_recent: usize,
    /// The goal line; empty takes [`goal_from_messages`].
    pub goal: String,
    /// What of tool arguments may be sent.
    pub egress: Egress,
}

#[derive(Clone, Debug, Serialize)]
struct CallLine {
    id: String,
    tool: String,
    input: String,
    result: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
enum EntryCalls {
    Full(Vec<CallLine>),
    Compact(Vec<String>),
}

#[derive(Clone, Debug, Serialize)]
struct Entry {
    i: usize,
    role: Role,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<EntryCalls>,
}

fn input_text(call: &ToolCall, limit: usize, egress: Egress) -> String {
    match egress {
        Egress::Full => truncate(&input_json(&call.input), limit),
        Egress::Metadata => metadata_input(&call.input).to_string(),
    }
}

fn result_note(call: &ToolCall) -> String {
    format!(
        "{}, {} chars (omitted)",
        if call.is_error { "error" } else { "ok" },
        call.result_chars
    )
}

/// One call as a single line, for when the structured form costs too much.
fn compact_call(call: &ToolCall, egress: Egress) -> String {
    let input = match egress {
        Egress::Full => call
            .input
            .iter()
            .map(|(key, value)| {
                let text = match value {
                    Value::String(s) => s.clone(),
                    other => {
                        let mut one = Map::new();
                        one.insert(key.clone(), other.clone());
                        truncate(&input_json(&one), 200)
                    }
                };
                format!(
                    "{key}={}",
                    text.split_whitespace().collect::<Vec<_>>().join(" ")
                )
            })
            .collect::<Vec<_>>()
            .join(" "),
        Egress::Metadata => format!(
            "keys={}",
            call.input.keys().cloned().collect::<Vec<_>>().join(",")
        ),
    };
    format!(
        "{} {} {} → {} {}ch",
        call.id,
        call.tool,
        truncate(&input, INPUT_CHARS[2]),
        if call.is_error { "error" } else { "ok" },
        call.result_chars
    )
}

fn calls_by_message(calls: &[ToolCall]) -> std::collections::BTreeMap<usize, Vec<&ToolCall>> {
    let mut by = std::collections::BTreeMap::<usize, Vec<&ToolCall>>::new();
    for call in calls {
        by.entry(call.call_index).or_default().push(call);
    }
    by
}

fn history_entries(
    messages: &[Message],
    calls: &[ToolCall],
    input_chars: usize,
    egress: Egress,
) -> Vec<Entry> {
    let by = calls_by_message(calls);
    let mut entries = Vec::new();
    for (i, message) in messages.iter().enumerate() {
        let lines: Vec<CallLine> = by
            .get(&i)
            .map(|own| {
                own.iter()
                    .map(|c| CallLine {
                        id: c.id.clone(),
                        tool: c.tool.clone(),
                        input: input_text(c, input_chars, egress),
                        result: result_note(c),
                    })
                    .collect()
            })
            .unwrap_or_default();
        if message.text.trim().is_empty() && lines.is_empty() {
            continue;
        }
        entries.push(Entry {
            i,
            role: message.role,
            text: message.text.clone(),
            tool_calls: if lines.is_empty() {
                None
            } else {
                Some(EntryCalls::Full(lines))
            },
        });
    }
    entries
}

fn state_value(goal: &str, history: &[Entry]) -> Value {
    serde_json::json!({ "context": STATE_CONTEXT, "goal": goal, "history": history })
}

fn entry_tokens(entry: &Entry) -> usize {
    estimate_tokens(&serde_json::to_string(entry).unwrap_or_default()) + 1
}

/// Builds the judge's state from the whole conversation and shrinks it in stages
/// until it fits `max_state_tokens`: tool inputs are cut (1000, 200, 60 `char`s),
/// long texts are abridged oldest-first (pinned messages last), old messages
/// collapse to a note, old calls shrink to one line each, old messages carrying
/// no call are left out, and runs of old call-only messages are folded together.
///
/// # Errors
///
/// [`TooLarge`] when even the last stage does not fit.
pub fn fit_state(
    messages: &[Message],
    calls: &[ToolCall],
    options: &FitOptions,
) -> Result<FittedState, TooLarge> {
    let goal = if options.goal.is_empty() {
        goal_from_messages(messages)
    } else {
        options.goal.clone()
    };
    let base = estimate_tokens(&state_value(&goal, &[]).to_string());
    let limit = options.max_state_tokens;
    let total_messages = messages.len();
    let pinned = |e: &Entry| is_pinned(e.i, total_messages, options.preserve_recent);
    let done = |history: &[Entry], tokens: usize, stage: &str| FittedState {
        state: state_value(&goal, history),
        tokens,
        stage: stage.to_owned(),
    };

    let mut history = Vec::new();
    let mut per = Vec::new();
    let mut tokens = 0;
    for (stage, &cap) in INPUT_CHARS.iter().enumerate() {
        history = history_entries(messages, calls, cap, options.egress);
        per = history.iter().map(entry_tokens).collect::<Vec<_>>();
        tokens = base + per.iter().sum::<usize>();
        if tokens <= limit {
            let name = if stage == 0 {
                "full".to_owned()
            } else {
                format!("inputs<={cap}")
            };
            return Ok(done(&history, tokens, &name));
        }
    }

    let order: Vec<usize> = {
        let idx: Vec<usize> = (0..history.len()).collect();
        let mut o: Vec<usize> = idx
            .iter()
            .copied()
            .filter(|&k| !pinned(&history[k]))
            .collect();
        o.extend(idx.iter().copied().filter(|&k| pinned(&history[k])));
        o
    };
    let shrink = |history: &mut Vec<Entry>,
                  per: &mut Vec<usize>,
                  tokens: &mut usize,
                  k: usize,
                  f: &dyn Fn(&mut Entry)| {
        f(&mut history[k]);
        let now = entry_tokens(&history[k]);
        *tokens = *tokens + now - per[k];
        per[k] = now;
    };

    for &k in &order {
        if clen(&history[k].text) <= TEXT_HEAD + TEXT_TAIL + 40 {
            continue;
        }
        shrink(&mut history, &mut per, &mut tokens, k, &|e| {
            e.text = abridge(&e.text, TEXT_HEAD, TEXT_TAIL)
        });
        if tokens <= limit {
            return Ok(done(&history, tokens, "texts abridged"));
        }
    }

    for &k in &order {
        if pinned(&history[k]) || history[k].text.is_empty() {
            continue;
        }
        let original = clen(&messages[history[k].i].text);
        shrink(&mut history, &mut per, &mut tokens, k, &|e| {
            e.text = format!("[… {original} chars omitted …]")
        });
        if tokens <= limit {
            return Ok(done(&history, tokens, "old messages collapsed"));
        }
    }

    let by = calls_by_message(calls);
    for &k in &order {
        if pinned(&history[k]) {
            continue;
        }
        let Some(own) = by.get(&history[k].i) else {
            continue;
        };
        let lines: Vec<String> = own
            .iter()
            .map(|c| compact_call(c, options.egress))
            .collect();
        shrink(&mut history, &mut per, &mut tokens, k, &|e| {
            e.tool_calls = Some(EntryCalls::Compact(lines.clone()))
        });
        if tokens <= limit {
            return Ok(done(&history, tokens, "old calls compacted"));
        }
    }

    let mut left = std::collections::HashSet::new();
    for &k in &order {
        if pinned(&history[k]) || history[k].tool_calls.is_some() {
            continue;
        }
        left.insert(k);
        tokens -= per[k];
        if tokens <= limit {
            let kept: Vec<Entry> = history
                .iter()
                .enumerate()
                .filter(|(k, _)| !left.contains(k))
                .map(|(_, e)| e.clone())
                .collect();
            return Ok(done(&kept, tokens, "old messages left out"));
        }
    }

    let remaining: Vec<Entry> = history
        .into_iter()
        .enumerate()
        .filter(|(k, _)| !left.contains(k))
        .map(|(_, e)| e)
        .collect();
    let merged = merge_call_runs(remaining, &pinned);
    let tokens = base + merged.iter().map(entry_tokens).sum::<usize>();
    if tokens <= limit {
        return Ok(done(&merged, tokens, "old calls merged"));
    }
    Err(TooLarge { tokens, limit })
}

/// Folds runs of adjacent unpinned, text-free, one-line-call entries of one role
/// into one entry each, so the per-entry envelope is paid once per run.
fn merge_call_runs(history: Vec<Entry>, pinned: &dyn Fn(&Entry) -> bool) -> Vec<Entry> {
    let foldable = |e: &Entry| {
        !pinned(e) && e.text.is_empty() && matches!(e.tool_calls, Some(EntryCalls::Compact(_)))
    };
    let mut merged: Vec<Entry> = Vec::new();
    for entry in history {
        if let Some(previous) = merged.last_mut() {
            if foldable(previous) && foldable(&entry) && previous.role == entry.role {
                if let (Some(EntryCalls::Compact(a)), Some(EntryCalls::Compact(b))) =
                    (&mut previous.tool_calls, entry.tool_calls)
                {
                    a.extend(b);
                }
                continue;
            }
        }
        merged.push(entry);
    }
    merged
}

/// One state and the candidate calls asked about with it.
#[derive(Clone, Debug)]
pub struct StateGroup {
    /// The state sent with every batch of this group.
    pub state: FittedState,
    /// Candidate calls (by index into the plan's call list).
    pub calls: Vec<usize>,
}

/// The groups a set of candidates is asked in.
#[derive(Clone, Debug, Default)]
pub struct Grouping {
    /// One group when the history fits, else contiguous windows.
    pub groups: Vec<StateGroup>,
    /// Candidates no window could fit: they take the rails without a judge.
    pub floor: Vec<usize>,
    /// Diagnostic stage name.
    pub stage: String,
}

/// One state for every candidate when the history fits. When it does not (long
/// sessions), candidates split into contiguous windows, halving until each
/// window's state fits: a window keeps the goal, the first message, the pinned
/// tail and its own messages in full, and leaves the rest of the history out.
/// Relevance of an old call depends mostly on the goal and the recent turns,
/// which every window carries.
pub fn state_groups(
    messages: &[Message],
    calls: &[ToolCall],
    candidates: &[usize],
    options: &FitOptions,
) -> Grouping {
    if let Ok(state) = fit_state(messages, calls, options) {
        let stage = state.stage.clone();
        return Grouping {
            groups: vec![StateGroup {
                state,
                calls: candidates.to_vec(),
            }],
            floor: vec![],
            stage,
        };
    }
    let window_options = FitOptions {
        goal: if options.goal.is_empty() {
            goal_from_messages(messages)
        } else {
            options.goal.clone()
        },
        ..options.clone()
    };
    let mut grouping = Grouping::default();
    fn fit(
        group: &[usize],
        messages: &[Message],
        calls: &[ToolCall],
        options: &FitOptions,
        out: &mut Grouping,
    ) {
        let lo = group
            .iter()
            .map(|&c| calls[c].call_index)
            .min()
            .unwrap_or(0);
        let hi = group
            .iter()
            .map(|&c| calls[c].result_index)
            .max()
            .unwrap_or(0);
        let in_window: std::collections::HashSet<usize> = group.iter().copied().collect();
        let view: Vec<Message> = messages
            .iter()
            .enumerate()
            .map(|(i, m)| {
                if (i >= lo && i <= hi) || is_pinned(i, messages.len(), options.preserve_recent) {
                    m.clone()
                } else {
                    Message {
                        text: String::new(),
                        ..m.clone()
                    }
                }
            })
            .collect();
        let visible: Vec<ToolCall> = calls
            .iter()
            .enumerate()
            .filter(|(k, c)| c.pinned || in_window.contains(k))
            .map(|(_, c)| c.clone())
            .collect();
        match fit_state(&view, &visible, options) {
            Ok(state) => out.groups.push(StateGroup {
                state,
                calls: group.to_vec(),
            }),
            Err(_) if group.len() == 1 => out.floor.push(group[0]),
            Err(_) => {
                let half = group.len().div_ceil(2);
                fit(&group[..half], messages, calls, options, out);
                fit(&group[half..], messages, calls, options, out);
            }
        }
    }
    let size = candidates.len().div_ceil(2).max(1);
    for chunk in candidates.chunks(size) {
        fit(chunk, messages, calls, &window_options, &mut grouping);
    }
    grouping.stage = format!(
        "windows:{}{}",
        grouping.groups.len(),
        if grouping.floor.is_empty() {
            String::new()
        } else {
            format!(" floor:{}", grouping.floor.len())
        }
    );
    grouping
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calls::collect_tool_calls;
    use crate::model::{ToolResult, ToolUse};

    fn transcript(n: usize, text_len: usize) -> Vec<Message> {
        let mut ms = vec![Message::text(Role::User, "fix the build")];
        for i in 0..n {
            let mut input = Map::new();
            input.insert(
                "command".into(),
                Value::String(format!("cargo build -p crate{i} {}", "x".repeat(300))),
            );
            ms.push(Message {
                role: Role::Assistant,
                text: "y".repeat(text_len),
                tool_uses: vec![ToolUse {
                    tool_use_id: format!("c{i}"),
                    tool: "Bash".into(),
                    input,
                    text: None,
                    is_error: false,
                }],
                tool_results: vec![],
            });
            ms.push(Message {
                role: Role::User,
                text: String::new(),
                tool_uses: vec![],
                tool_results: vec![ToolResult {
                    tool_use_id: format!("c{i}"),
                    text: "ok".repeat(50),
                    is_error: i % 3 == 0,
                }],
            });
        }
        ms
    }

    fn opts(max: usize) -> FitOptions {
        FitOptions {
            max_state_tokens: max,
            preserve_recent: 6,
            goal: String::new(),
            egress: Egress::Full,
        }
    }

    #[test]
    fn fits_whole_when_small() {
        let ms = transcript(3, 10);
        let calls = collect_tool_calls(&ms, 6);
        let f = fit_state(&ms, &calls, &opts(25_000)).unwrap();
        assert_eq!(f.stage, "full");
        assert_eq!(f.state["goal"], "fix the build");
        let h = f.state["history"].as_array().unwrap();
        assert_eq!(h[1]["tool_calls"][0]["id"], "t1");
        assert_eq!(
            h[1]["tool_calls"][0]["result"],
            "error, 100 chars (omitted)"
        );
    }

    #[test]
    fn shrinks_through_stages_and_reports_them() {
        let ms = transcript(40, 2000);
        let calls = collect_tool_calls(&ms, 6);
        let full = fit_state(&ms, &calls, &opts(10_000_000)).unwrap().tokens;
        let f = fit_state(&ms, &calls, &opts(full / 3)).unwrap();
        assert!(f.tokens <= full / 3, "{} > {}", f.tokens, full / 3);
        assert_ne!(f.stage, "full");
        assert!(fit_state(&ms, &calls, &opts(50)).is_err());
    }

    #[test]
    fn metadata_egress_sends_no_argument_value() {
        let ms = transcript(3, 10);
        let calls = collect_tool_calls(&ms, 6);
        let f = fit_state(
            &ms,
            &calls,
            &FitOptions {
                egress: Egress::Metadata,
                ..opts(25_000)
            },
        )
        .unwrap();
        let s = f.state.to_string();
        assert!(!s.contains("cargo build"), "{s}");
        assert!(s.contains("\\\"keys\\\":[\\\"command\\\"]"));
    }

    #[test]
    fn metadata_egress_holds_through_every_shrink_stage() {
        let ms = transcript(30, 2000);
        let calls = collect_tool_calls(&ms, 6);
        let meta = |max: usize| FitOptions {
            egress: Egress::Metadata,
            ..opts(max)
        };
        // Non-vacuity: the same transcript under `Egress::Full` does egress values.
        let full_egress = fit_state(&ms, &calls, &opts(10_000_000))
            .unwrap()
            .state
            .to_string();
        assert!(full_egress.contains("cargo build"));
        let full = fit_state(&ms, &calls, &meta(10_000_000)).unwrap().tokens;
        let mut budget = full;
        let mut saw_shrunk = false;
        while budget > 128 {
            if let Ok(f) = fit_state(&ms, &calls, &meta(budget)) {
                let s = f.state.to_string();
                assert!(!s.contains("cargo"), "stage {}, budget {budget}: {s}", f.stage);
                assert!(!s.contains("xxxx"), "stage {}, budget {budget}: {s}", f.stage);
                saw_shrunk |= f.stage != "full";
            }
            budget /= 2;
        }
        assert!(saw_shrunk, "no shrink stage was reached");
    }

    #[test]
    fn windows_when_history_cannot_fit() {
        let ms = transcript(30, 3000);
        let calls = collect_tool_calls(&ms, 6);
        let candidates: Vec<usize> = calls
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.pinned)
            .map(|(k, _)| k)
            .collect();
        let g = state_groups(&ms, &calls, &candidates, &opts(700));
        assert!(g.stage.starts_with("windows:"), "{}", g.stage);
        let covered: usize = g.groups.iter().map(|x| x.calls.len()).sum::<usize>() + g.floor.len();
        assert_eq!(covered, candidates.len());
        for group in &g.groups {
            assert!(group.state.tokens <= 700);
        }
    }
}
