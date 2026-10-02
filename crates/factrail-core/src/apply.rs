//! Applying decisions with as few rails given up as the window needs.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::{Regex, RegexBuilder};

use crate::calls::ToolCall;
use crate::model::{Message, transcript_chars};
use crate::rails::{
    INPUT_BRIEF_CHARS, LAST_RESORT_TIER, Layout, RAIL_TIERS, RERUN_INPUT_CHARS, brief_map,
    fact_lines, full_output_note, is_compacted, read_is_observation, render_stub, reproducible,
    rerun_note, stub_layout,
};
use crate::text::{clen, cslice};
use crate::value::{
    ValueContext, ValueParts, message_contexts, pool_lines, token_values, toks, value_parts,
};

/// What the judge decided for one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// The call and its result stay verbatim.
    Keep,
    /// The call stays; its result is reduced by the rails.
    DropResult,
    /// The call's arguments are cut short; its result is reduced by the rails.
    DropCall,
}

/// How fact lines are chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    /// Pick lines by learned token value; `false` keeps the regex fact lines.
    pub value: bool,
    /// Pool one fact budget across every stub of the compaction (needs `value`).
    pub pool: bool,
    /// Count a token as covered when it is visible inside a longer kept one.
    pub contain: bool,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            value: true,
            pool: true,
            contain: true,
        }
    }
}

/// How far a compaction must go.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reduction {
    /// The char reduction the rails escalate towards.
    pub target: f64,
    /// When even the strictest tier misses this reduction, the oldest results
    /// give way until it is met; 0 never evicts.
    pub hard: f64,
}

/// The rebuilt transcript and what it cost.
#[derive(Clone, Debug)]
pub struct Applied {
    /// The transcript, one message per input message, in order.
    pub messages: Vec<Message>,
    /// For each message, whether anything in it changed.
    pub touched: Vec<bool>,
    /// The strictest rail tier used; [`LAST_RESORT_TIER`] when results were evicted.
    pub tier: usize,
}

/// Fact-like tokens a result loses by going to a stricter tier: paths, hex ids, numbers.
static FACT_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(
        r#"[A-Za-z]:[\\/][^\s"'<>]+|/(?:[\w.@-]+/)+[\w.@-]+|\b[0-9a-f]{7,40}\b|\b\d[\d.,:]*\d\b"#,
    )
    .case_insensitive(true)
    .build()
    .expect("static regex")
});

fn fact_tokens(s: &str) -> HashSet<&str> {
    FACT_TOKEN.find_iter(s).map(|m| m.as_str()).collect()
}

type PickKey = (usize, usize, usize);

/// Reduces results, caching what it computed.
struct Reducer<'a> {
    texts: HashMap<&'a str, (&'a str, bool)>,
    rerunnable: HashSet<&'a str>,
    head_chars: usize,
    selection: Selection,
    ctxs: HashMap<String, ValueContext>,
    values: HashMap<&'a str, HashMap<String, f64>>,
    parts: HashMap<(&'a str, PickKey), ValueParts>,
    pooled: HashMap<&'a str, Vec<String>>,
    cache: HashMap<(&'a str, usize), String>,
    tokens: HashMap<(&'a str, usize), HashSet<String>>,
}

impl<'a> Reducer<'a> {
    /// `id`'s result under tier `tier`, and the stub key when the value selector chose its lines.
    fn reduce_uncached(&mut self, id: &'a str, tier: usize) -> (String, Option<PickKey>) {
        let (text, is_error) = self.texts[id];
        if is_compacted(text) {
            return (text.to_owned(), None);
        }
        let rails = &RAIL_TIERS[tier];
        if self.rerunnable.contains(id)
            && !is_error
            && clen(text) > rails.read_keep
            && !read_is_observation(text)
        {
            return (rerun_note(text, id), None);
        }
        match stub_layout(text, is_error, rails, self.head_chars) {
            Layout::Whole => (text.to_owned(), None),
            Layout::Stub {
                head_end,
                tail_start,
                budget,
            } => {
                let middle = cslice(text, head_end, tail_start);
                let key = (head_end, tail_start, budget);
                let facts = if self.selection.value && self.ctxs.contains_key(id) {
                    if let Some(lines) = self.pooled.get(id) {
                        lines.clone()
                    } else {
                        if !self.values.contains_key(id) {
                            let v = token_values(text, &self.ctxs[id]);
                            self.values.insert(id, v);
                        }
                        let values = &self.values[id];
                        let parts = self
                            .parts
                            .entry((id, key))
                            .or_insert_with(|| value_parts(middle, budget, values));
                        parts.lines(&parts.greedy.clone())
                    }
                } else {
                    fact_lines(middle, budget)
                };
                let with_key = (self.selection.value && self.ctxs.contains_key(id)).then_some(key);
                (
                    render_stub(text, head_end, tail_start, &facts, is_error, id),
                    with_key,
                )
            }
        }
    }

    fn reduce(&mut self, id: &'a str, tier: usize) -> &str {
        if !self.cache.contains_key(&(id, tier)) {
            let (text, _) = self.reduce_uncached(id, tier);
            self.cache.insert((id, tier), text);
        }
        &self.cache[&(id, tier)]
    }

    fn len(&mut self, id: &'a str, tier: usize) -> usize {
        clen(self.reduce(id, tier))
    }

    /// Fact-like tokens of `id`'s result under `tier`.
    fn fact_tokens(&mut self, id: &'a str, tier: usize) -> &HashSet<String> {
        if !self.tokens.contains_key(&(id, tier)) {
            let set = fact_tokens(self.reduce(id, tier))
                .into_iter()
                .map(str::to_owned)
                .collect();
            self.tokens.insert((id, tier), set);
        }
        &self.tokens[&(id, tier)]
    }

    /// Fact-like tokens lost going from tier `from` to tier `to`.
    fn lost(&mut self, id: &'a str, from: usize, to: usize) -> usize {
        let after = self.fact_tokens(id, to).clone();
        self.fact_tokens(id, from)
            .iter()
            .filter(|t| !after.contains(*t))
            .count()
    }
}

/// Rebuilds `messages` with each reduced call's arguments cut and its result
/// replaced by `reduced[id]`.
fn rebuild(
    messages: &[Message],
    actions: &HashMap<&str, Action>,
    rerunnable: &HashSet<&str>,
    reduced: &HashMap<&str, String>,
) -> Applied {
    let mut out = Vec::with_capacity(messages.len());
    let mut touched = Vec::with_capacity(messages.len());
    for m in messages {
        let mut changed = false;
        let mut copy = m.clone();
        for tool in &mut copy.tool_uses {
            let Some(&action) = actions.get(tool.tool_use_id.as_str()) else {
                continue;
            };
            if action == Action::Keep {
                continue;
            }
            let rerun = rerunnable.contains(tool.tool_use_id.as_str());
            if action == Action::DropCall || rerun {
                let brief = brief_map(
                    &tool.input,
                    if rerun {
                        RERUN_INPUT_CHARS
                    } else {
                        INPUT_BRIEF_CHARS
                    },
                );
                if brief != tool.input {
                    tool.input = brief;
                    changed = true;
                }
            }
            if let (Some(mirror), Some(new)) = (&tool.text, reduced.get(tool.tool_use_id.as_str()))
            {
                if mirror != new {
                    tool.text = Some(new.clone());
                    changed = true;
                }
            }
        }
        for result in &mut copy.tool_results {
            if let Some(new) = reduced.get(result.tool_use_id.as_str()) {
                if &result.text != new {
                    result.text.clone_from(new);
                    changed = true;
                }
            }
        }
        touched.push(changed);
        out.push(if changed { copy } else { m.clone() });
    }
    Applied {
        messages: out,
        touched,
        tier: 0,
    }
}

/// Applies `actions` (by `tool_use_id`) to `messages` with as few rails given up as
/// `reduction.target` needs: starting from the loosest tier, the result whose next
/// tier frees the most chars per fact-like token lost is escalated first. With
/// value selection on, the stubs' fact lines then share one pooled budget. When
/// even the strictest tier misses `reduction.hard`, the oldest reduced results
/// give way: first only their fact lines stay, then they become one-line notes.
/// Pinned calls and kept calls are never touched.
pub fn apply(
    messages: &[Message],
    calls: &[ToolCall],
    actions: &HashMap<&str, Action>,
    head_chars: usize,
    selection: Selection,
    reduction: Reduction,
) -> Applied {
    let before = transcript_chars(messages);
    let rerunnable: HashSet<&str> = calls
        .iter()
        .filter(|c| reproducible(&c.tool, &c.input))
        .map(|c| c.tool_use_id.as_str())
        .collect();
    let mut order: Vec<&str> = Vec::new();
    let mut texts = HashMap::new();
    for m in messages {
        for r in &m.tool_results {
            if actions
                .get(r.tool_use_id.as_str())
                .is_some_and(|a| *a != Action::Keep)
                && !texts.contains_key(r.tool_use_id.as_str())
            {
                texts.insert(r.tool_use_id.as_str(), (r.text.as_str(), r.is_error));
                order.push(r.tool_use_id.as_str());
            }
        }
    }
    let ctxs = if selection.value {
        message_contexts(messages)
    } else {
        HashMap::new()
    };
    let mut reducer = Reducer {
        texts,
        rerunnable: rerunnable.clone(),
        head_chars,
        selection,
        ctxs,
        values: HashMap::new(),
        parts: HashMap::new(),
        pooled: HashMap::new(),
        cache: HashMap::new(),
        tokens: HashMap::new(),
    };

    let mut level: HashMap<&str, usize> = HashMap::new();
    let loosest: HashMap<&str, String> = order
        .iter()
        .map(|&id| (id, reducer.reduce(id, 0).to_owned()))
        .collect();
    let first = rebuild(messages, actions, &rerunnable, &loosest);
    let mut after = transcript_chars(&first.messages);
    let need = before as f64 * (1.0 - reduction.target);
    let mut top = 0;
    while (after as f64) > need && before > 0 {
        let mut best: Option<(&str, usize, usize, f64)> = None;
        for &id in &order {
            let cur = level.get(id).copied().unwrap_or(0);
            for tier in cur + 1..RAIL_TIERS.len() {
                let gain = reducer.len(id, cur).saturating_sub(reducer.len(id, tier));
                if gain == 0 {
                    continue;
                }
                let score = gain as f64 / (1 + reducer.lost(id, cur, tier)) as f64;
                if best.is_none_or(|b| score > b.3) {
                    best = Some((id, tier, gain, score));
                }
                break;
            }
        }
        let Some((id, tier, gain, _)) = best else {
            break;
        };
        level.insert(id, tier);
        top = top.max(tier);
        after -= gain;
    }

    if selection.value && selection.pool {
        pool(
            &mut reducer,
            &order,
            &level,
            messages,
            actions,
            &rerunnable,
            selection.contain,
        );
    }
    let finals: HashMap<&str, String> = order
        .iter()
        .map(|&id| {
            let tier = level.get(id).copied().unwrap_or(0);
            (id, reducer.reduce_uncached(id, tier).0)
        })
        .collect();
    let mut applied = rebuild(messages, actions, &rerunnable, &finals);
    applied.tier = top;
    let hard_need = before as f64 * (1.0 - reduction.hard);
    if before > 0 && reduction.hard > 0.0 && transcript_chars(&applied.messages) as f64 > hard_need
    {
        let pinned: HashSet<&str> = calls
            .iter()
            .filter(|c| c.pinned)
            .map(|c| c.tool_use_id.as_str())
            .collect();
        let evict: Vec<&str> = order
            .iter()
            .copied()
            .filter(|id| !pinned.contains(id))
            .collect();
        if last_resort(&mut applied, &evict, hard_need) {
            applied.tier = LAST_RESORT_TIER;
        }
    }
    applied
}

/// Refills every value-selected stub's greedy lines from one budget.
fn pool<'a>(
    reducer: &mut Reducer<'a>,
    order: &[&'a str],
    level: &HashMap<&str, usize>,
    messages: &[Message],
    actions: &HashMap<&str, Action>,
    rerunnable: &HashSet<&str>,
    contain: bool,
) {
    let mut keys: HashMap<&str, PickKey> = HashMap::new();
    let mut base: HashMap<&str, String> = HashMap::new();
    for &id in order {
        let (text, key) = reducer.reduce_uncached(id, level.get(id).copied().unwrap_or(0));
        if let Some(key) = key {
            keys.insert(id, key);
        }
        base.insert(id, text);
    }
    let applied = rebuild(messages, actions, rerunnable, &base);
    let mut side = Vec::new();
    let mut members: Vec<&'a str> = Vec::new();
    for m in &applied.messages {
        for r in &m.tool_results {
            match keys
                .get_key_value(r.tool_use_id.as_str())
                .map(|(&o, &key)| (o, key))
            {
                Some((o, key)) => {
                    let parts = &reducer.parts[&(o, key)];
                    let kept: HashSet<&String> =
                        parts.greedy.iter().map(|&i| &parts.units[i]).collect();
                    side.push(
                        r.text
                            .split('\n')
                            .filter(|l| !kept.iter().any(|k| k.as_str() == *l))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                    members.push(o);
                }
                None => side.push(r.text.clone()),
            }
        }
    }
    if members.is_empty() {
        return;
    }
    let mut covered: HashSet<String> = side.iter().flat_map(|s| toks(s)).collect();
    let joined = side.join("\n");
    let parts: Vec<(&str, &ValueParts, &HashMap<String, f64>)> = members
        .iter()
        .map(|&o| (o, &reducer.parts[&(o, keys[o])], &reducer.values[o]))
        .collect();
    let lines = pool_lines(&parts, &mut covered, contain.then_some(joined.as_str()));
    reducer.pooled.extend(lines);
}

/// The oldest reduced results give way until the transcript is at most `need`
/// chars. Returns whether anything changed.
fn last_resort(applied: &mut Applied, order: &[&str], need: f64) -> bool {
    let mut total = transcript_chars(&applied.messages) as f64;
    let mut place: HashMap<String, (usize, usize)> = HashMap::new();
    for (mi, m) in applied.messages.iter().enumerate() {
        for (ri, r) in m.tool_results.iter().enumerate() {
            place.insert(r.tool_use_id.clone(), (mi, ri));
        }
    }
    let mut changed = false;
    let mut put = |applied: &mut Applied, id: &str, next: String, total: &mut f64| {
        let (mi, ri) = place[id];
        let old = clen(&applied.messages[mi].tool_results[ri].text);
        *total -= old as f64 - clen(&next) as f64;
        applied.messages[mi].tool_results[ri].text = next;
        applied.touched[mi] = true;
        changed = true;
    };
    for &id in order {
        if total <= need {
            break;
        }
        let Some(&(mi, ri)) = place.get(id) else {
            continue;
        };
        let text = applied.messages[mi].tool_results[ri].text.clone();
        let mut notes: Vec<String> = text
            .split('\n')
            .filter(|l| is_compacted(l))
            .map(str::to_owned)
            .collect();
        if notes.is_empty() {
            notes.push(format!(
                "[factrail omitted {} chars: only fact lines kept under context pressure; {}]",
                clen(&text),
                full_output_note(id)
            ));
        }
        let mut lines = fact_lines(&text, clen(&text) / 4);
        lines.extend(notes);
        let next = lines.join("\n");
        if clen(&next) < clen(&text) {
            put(applied, id, next, &mut total);
        }
    }
    for &id in order {
        if total <= need {
            break;
        }
        let Some(&(mi, ri)) = place.get(id) else {
            continue;
        };
        let text = &applied.messages[mi].tool_results[ri].text;
        let next = format!(
            "[factrail omitted {} chars: evicted under context pressure, oldest first; {}]",
            clen(text),
            full_output_note(id)
        );
        if clen(&next) < clen(text) {
            put(applied, id, next, &mut total);
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calls::collect_tool_calls;
    use crate::model::{Role, ToolResult, ToolUse};
    use serde_json::{Map, Value};

    fn session(outputs: &[(&str, &str, String)]) -> Vec<Message> {
        let mut ms = vec![Message::text(Role::User, "investigate the outage")];
        for (id, command, out) in outputs {
            let mut input = Map::new();
            input.insert("command".into(), Value::String((*command).to_owned()));
            ms.push(Message {
                role: Role::Assistant,
                text: String::new(),
                tool_uses: vec![ToolUse {
                    tool_use_id: (*id).into(),
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
                    tool_use_id: (*id).into(),
                    text: out.clone(),
                    is_error: false,
                }],
            });
        }
        for i in 0..6 {
            ms.push(Message::text(
                if i % 2 == 0 {
                    Role::Assistant
                } else {
                    Role::User
                },
                format!("turn {i}"),
            ));
        }
        ms
    }

    fn noisy(fact: &str, n: usize) -> String {
        let mut s = String::new();
        for i in 0..n {
            s.push_str(&format!("plain filler text line number {}\n", i % 9));
            if i == n / 2 {
                s.push_str(fact);
                s.push('\n');
            }
        }
        s
    }

    fn all(ms: &[Message], action: Action) -> (Vec<ToolCall>, HashMap<String, Action>) {
        let calls = collect_tool_calls(ms, 6);
        let actions = calls
            .iter()
            .filter(|c| !c.pinned)
            .map(|c| (c.tool_use_id.clone(), action))
            .collect();
        (calls, actions)
    }

    fn borrow(a: &HashMap<String, Action>) -> HashMap<&str, Action> {
        a.iter().map(|(k, v)| (k.as_str(), *v)).collect()
    }

    #[test]
    fn observations_keep_their_facts_and_reads_shrink() {
        let ms = session(&[
            (
                "c1",
                "curl -s http://svc:8080/health",
                noisy("HTTP/1.1 503 upstream connect error pid 77812", 600),
            ),
            ("c2", "cat src/lib.rs", "x".repeat(9000)),
        ]);
        let (calls, actions) = all(&ms, Action::DropResult);
        let out = apply(
            &ms,
            &calls,
            &borrow(&actions),
            200,
            Selection::default(),
            Reduction {
                target: 0.3,
                hard: 0.0,
            },
        );
        let curl = &out.messages[2].tool_results[0].text;
        assert!(
            curl.contains("503 upstream connect error pid 77812"),
            "{curl}"
        );
        assert!(curl.contains("[factrail omitted"));
        let read = &out.messages[4].tool_results[0].text;
        assert!(read.starts_with("[factrail omitted 9000 chars: a reproducible read"));
        assert!(out.touched[2] && out.touched[4] && !out.touched[0]);
        assert!(transcript_chars(&out.messages) < transcript_chars(&ms));
    }

    #[test]
    fn keep_changes_nothing() {
        let ms = session(&[("c1", "curl x", noisy("error 1", 600))]);
        let (calls, actions) = all(&ms, Action::Keep);
        let out = apply(
            &ms,
            &calls,
            &borrow(&actions),
            200,
            Selection::default(),
            Reduction {
                target: 0.9,
                hard: 0.9,
            },
        );
        assert_eq!(out.messages, ms);
        assert!(out.touched.iter().all(|t| !t));
    }

    #[test]
    fn escalates_tiers_then_evicts_under_a_hard_line() {
        let outputs: Vec<(&str, &str, String)> = vec![
            ("c1", "curl a", noisy("status 500 id 0a1b2c3d4e", 300)),
            ("c2", "curl b", noisy("status 404 id 9f8e7d6c5b", 300)),
            ("c3", "curl c", noisy("status 502 id 1234567abc", 300)),
        ];
        let ms = session(&outputs);
        let (calls, actions) = all(&ms, Action::DropResult);
        let loose = apply(
            &ms,
            &calls,
            &borrow(&actions),
            200,
            Selection::default(),
            Reduction {
                target: 0.05,
                hard: 0.0,
            },
        );
        let strict = apply(
            &ms,
            &calls,
            &borrow(&actions),
            200,
            Selection::default(),
            Reduction {
                target: 0.95,
                hard: 0.0,
            },
        );
        assert!(strict.tier >= loose.tier);
        assert!(transcript_chars(&strict.messages) <= transcript_chars(&loose.messages));
        let evicted = apply(
            &ms,
            &calls,
            &borrow(&actions),
            200,
            Selection::default(),
            Reduction {
                target: 0.95,
                hard: 0.97,
            },
        );
        assert_eq!(evicted.tier, LAST_RESORT_TIER);
        assert!(transcript_chars(&evicted.messages) < transcript_chars(&strict.messages));
        // The oldest result gives way first: it is now shorter than under the strictest tier.
        let oldest = |a: &Applied| clen(&a.messages[2].tool_results[0].text);
        assert!(oldest(&evicted) < oldest(&strict));
    }

    #[test]
    fn idempotent_on_reduced_results() {
        let ms = session(&[("c1", "curl x", noisy("error 1", 600))]);
        let (calls, actions) = all(&ms, Action::DropResult);
        let once = apply(
            &ms,
            &calls,
            &borrow(&actions),
            200,
            Selection::default(),
            Reduction {
                target: 0.3,
                hard: 0.0,
            },
        );
        let twice = apply(
            &once.messages,
            &calls,
            &borrow(&actions),
            200,
            Selection::default(),
            Reduction {
                target: 0.9,
                hard: 0.0,
            },
        );
        assert_eq!(
            once.messages[2].tool_results[0].text,
            twice.messages[2].tool_results[0].text
        );
    }

    #[test]
    fn drop_call_briefs_the_arguments() {
        let long = format!("curl {}", "q".repeat(500));
        let ms = session(&[("c1", long.as_str(), noisy("error 1", 600))]);
        let (calls, actions) = all(&ms, Action::DropCall);
        let out = apply(
            &ms,
            &calls,
            &borrow(&actions),
            200,
            Selection::default(),
            Reduction {
                target: 0.3,
                hard: 0.0,
            },
        );
        let cmd = out.messages[1].tool_uses[0].input["command"]
            .as_str()
            .unwrap();
        assert!(cmd.ends_with("…[305 chars]"), "{cmd}");
    }

    #[test]
    fn regex_selection_without_value_model() {
        let ms = session(&[("c1", "curl x", noisy("fatal: refused port 5432", 600))]);
        let (calls, actions) = all(&ms, Action::DropResult);
        let sel = Selection {
            value: false,
            pool: false,
            contain: false,
        };
        let out = apply(
            &ms,
            &calls,
            &borrow(&actions),
            200,
            sel,
            Reduction {
                target: 0.3,
                hard: 0.0,
            },
        );
        assert!(
            out.messages[2].tool_results[0]
                .text
                .contains("fatal: refused port 5432")
        );
    }
}
