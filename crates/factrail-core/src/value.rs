//! Fact lines by learned token value, and one fact budget pooled across a compaction.
//!
//! A token's value is P(the agent uses it after the compaction): a logistic
//! regression on 13 token features, fitted upstream (hermes-jev-compaction
//! v0.8.0) on 191 076 tokens of 85 Claude Code and Codex transcripts. Within the
//! chars the regex fact lines would take, a stub keeps the regex lines up to a
//! third, then every error piece, then pieces by lazy greedy weighted coverage
//! (most not-yet-kept value per char; coverage is monotone submodular, so the
//! lazy evaluation is exact).
//!
//! Pooling (upstream v0.9/v0.10) refills the chars every stub's greedy spent from
//! one budget over all stubs of a compaction, counting a token as covered when it
//! is visible anywhere in what the compaction keeps, including inside a longer
//! token (a short hash inside a path).

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use regex::{Regex, RegexBuilder};

use crate::model::{Message, Role, input_json};
use crate::rails::{fact_lines, units};
use crate::text::{clen, find_str};

/// Feature means of the standardised model.
const MU: [f64; 13] = [
    0.400888, 0.240747, 0.240281, 0.016752, 2.832681, 0.13081, 0.871874, 0.004401, 0.007997,
    3.024303, 0.478259, 9.281838, 0.050729,
];
/// Feature standard deviations.
const SD: [f64; 13] = [
    0.490078, 0.427537, 0.427254, 0.128343, 0.642327, 0.239112, 0.385938, 0.066197, 0.089067,
    0.954716, 0.292466, 1.062772, 0.219443,
];
const INTERCEPT: f64 = -2.884622;
const BETA: [f64; 13] = [
    -0.227483, 0.122111, -0.003107, 0.097325, -0.170036, -0.058623, 0.360508, 0.183316, 0.152196,
    -0.008517, -0.118614, -0.332467, 0.520239,
];

static DIG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9][A-Za-z0-9_.:/@#-]{5,}").expect("static regex"));
static WRD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_./-]{7,}").expect("static regex"));
static HEX: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(r"^[0-9a-f]{8,}$")
        .case_insensitive(true)
        .build()
        .expect("static regex")
});
static EXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.[A-Za-z]{1,5}$").expect("static regex"));
/// A piece that reports a failure; every one fits before the greedy picks.
pub static ERROR_PIECE: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(r"error|failed|exception|traceback|denied|not found|timed out")
        .case_insensitive(true)
        .build()
        .expect("static regex")
});

fn strip(t: &str) -> &str {
    t.trim_end_matches(['.', ':', ','])
}

/// Candidate fact tokens of `s`: digit-bearing runs, and digit-free words of at
/// least eight chars containing `/`, `_` or `.`.
///
/// ```
/// use factrail_core::value::toks;
/// let t = toks("wrote src/main_loop.rs at pid 48211, commit 9f3c2ab1.");
/// assert!(t.contains("9f3c2ab1"));
/// assert!(t.contains("src/main_loop.rs"));
/// assert!(!t.contains("48211")); // five chars: below the six-char floor
/// ```
pub fn toks(s: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for m in DIG.find_iter(s) {
        if m.as_str().bytes().any(|b| b.is_ascii_digit()) {
            out.insert(strip(m.as_str()).to_owned());
        }
    }
    for m in WRD.find_iter(s) {
        let t = m.as_str();
        let stripped = strip(t);
        if t.contains(['/', '_', '.'])
            && !t.bytes().any(|b| b.is_ascii_digit())
            && clen(stripped) >= 8
        {
            out.insert(stripped.to_owned());
        }
    }
    out
}

/// What the model knows about one result.
#[derive(Clone, Debug, Default)]
pub struct ValueContext {
    /// Arguments of the last call before the result (with parallel calls, the batch's last).
    pub input: String,
    /// The last user prompt of the transcript.
    pub user: String,
    /// Results from this one to the end of the transcript.
    pub dist: usize,
    /// Tokens the agent already reused: named in a call or a reply after a result introduced them.
    pub reused: Arc<HashSet<String>>,
}

/// Each token of `text` valued as P(the agent uses it after the compaction).
pub fn token_values(text: &str, ctx: &ValueContext) -> HashMap<String, f64> {
    let length = clen(text);
    let mut values = HashMap::new();
    for t in toks(text) {
        let tl = clen(&t) as f64;
        let digits = t.chars().filter(char::is_ascii_digit).count() as f64;
        let x = [
            f64::from(u8::from(t.bytes().any(|b| b.is_ascii_digit()))),
            f64::from(u8::from(t.contains(['/', '\\']))),
            f64::from(u8::from(EXT.is_match(&t))),
            f64::from(u8::from(HEX.is_match(&t))),
            tl.ln(),
            digits / tl,
            (text.matches(t.as_str()).count() as f64).ln_1p(),
            f64::from(u8::from(ctx.input.contains(t.as_str()))),
            f64::from(u8::from(ctx.user.contains(t.as_str()))),
            (ctx.dist as f64).ln_1p(),
            find_str(text, &t).unwrap_or(0) as f64 / length.max(1) as f64,
            (length.max(1) as f64).ln(),
            f64::from(u8::from(ctx.reused.contains(&t))),
        ];
        let z = x
            .iter()
            .enumerate()
            .fold(INTERCEPT, |acc, (k, v)| acc + BETA[k] * (v - MU[k]) / SD[k]);
        values.insert(t, 1.0 / (1.0 + (-z).exp()));
    }
    values
}

enum Event<'a> {
    User(&'a str),
    Asst(&'a str),
    In(String),
    Out(&'a str, &'a str),
}

/// The [`ValueContext`] of every tool result, by `tool_use_id`.
pub fn message_contexts(messages: &[Message]) -> HashMap<String, ValueContext> {
    let mut events = Vec::new();
    for m in messages {
        match m.role {
            Role::User => {
                if m.tool_results.is_empty() {
                    events.push(Event::User(&m.text));
                }
                for r in &m.tool_results {
                    events.push(Event::Out(&r.text, &r.tool_use_id));
                }
            }
            Role::Assistant => {
                events.push(Event::Asst(&m.text));
                for t in &m.tool_uses {
                    events.push(Event::In(input_json(&t.input)));
                }
            }
        }
    }
    let user = events
        .iter()
        .rev()
        .find_map(|e| {
            if let Event::User(t) = e {
                Some((*t).to_owned())
            } else {
                None
            }
        })
        .unwrap_or_default();
    let mut intro: HashMap<String, usize> = HashMap::new();
    let mut reused = HashSet::new();
    for (i, e) in events.iter().enumerate() {
        match e {
            Event::In(t) => reused.extend(
                toks(t)
                    .into_iter()
                    .filter(|x| intro.get(x).is_some_and(|&k| k < i)),
            ),
            Event::Asst(t) => reused.extend(
                toks(t)
                    .into_iter()
                    .filter(|x| intro.get(x).is_some_and(|&k| k < i)),
            ),
            Event::Out(t, _) => {
                for x in toks(t) {
                    intro.entry(x).or_insert(i);
                }
            }
            Event::User(_) => {}
        }
    }
    let reused = Arc::new(reused);
    let mut outs: Vec<(&str, String)> = Vec::new();
    let mut last_in = String::new();
    for e in &events {
        match e {
            Event::In(t) => last_in.clone_from(t),
            Event::Out(_, id) => outs.push((id, last_in.clone())),
            _ => {}
        }
    }
    let total = outs.len();
    outs.into_iter()
        .enumerate()
        .map(|(n, (id, input))| {
            (
                id.to_owned(),
                ValueContext {
                    input,
                    user: user.clone(),
                    dist: total - n,
                    reused: Arc::clone(&reused),
                },
            )
        })
        .collect()
}

/// A stub's pieces and which of them it keeps.
#[derive(Clone, Debug, PartialEq)]
pub struct ValueParts {
    /// The pieces of the cut-out middle.
    pub units: Vec<String>,
    /// Kept before the greedy: regex lines up to a third, then error pieces.
    pub pre: Vec<usize>,
    /// Kept by the greedy coverage, in pick order.
    pub greedy: Vec<usize>,
}

impl ValueParts {
    /// The kept pieces, in text order.
    pub fn lines(&self, greedy: &[usize]) -> Vec<String> {
        let mut idx: Vec<usize> = self.pre.iter().chain(greedy).copied().collect();
        idx.sort_unstable();
        idx.into_iter().map(|i| self.units[i].clone()).collect()
    }
}

/// Within the chars the regex fact lines of `text` use under `budget`: regex
/// lines up to a third, error pieces, then lazy greedy coverage of token value.
pub fn value_parts(text: &str, budget: usize, values: &HashMap<String, f64>) -> ValueParts {
    let cap = crate::rails::lines_chars(&fact_lines(text, budget));
    let units = units(text);
    let index: HashMap<&str, usize> = units
        .iter()
        .enumerate()
        .map(|(i, u)| (u.as_str(), i))
        .collect();
    let mut pre = Vec::new();
    let mut covered = HashSet::new();
    let mut used = 0usize;
    for line in fact_lines(text, cap / 3) {
        if let Some(&i) = index.get(line.as_str()) {
            let cost = clen(&line) + 1;
            if !pre.contains(&i) && used + cost <= cap {
                pre.push(i);
                used += cost;
                covered.extend(toks(&line));
            }
        }
    }
    for (i, u) in units.iter().enumerate() {
        let cost = clen(u) + 1;
        if !pre.contains(&i) && ERROR_PIECE.is_match(u) && used + cost <= cap {
            pre.push(i);
            used += cost;
            covered.extend(toks(u));
        }
    }
    let taken: HashSet<usize> = pre.iter().copied().collect();
    let cands: Vec<Candidate<usize>> = (0..units.len())
        .filter(|i| !taken.contains(i))
        .map(|i| Candidate {
            key: i,
            chars: clen(&units[i]),
            toks: toks(&units[i]),
            values,
        })
        .collect();
    let greedy = lazy_cover(&cands, &mut covered, cap.saturating_sub(used));
    ValueParts { units, pre, greedy }
}

/// One piece a greedy may pick.
pub struct Candidate<'v, K> {
    /// What to report when picked.
    pub key: K,
    /// The piece's length.
    pub chars: usize,
    /// Its tokens.
    pub toks: HashSet<String>,
    /// Token values of the result it belongs to.
    pub values: &'v HashMap<String, f64>,
}

struct Entry {
    gain: f64,
    n: usize,
}
impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Entry {
    /// Highest gain first; ties by lower index, as Python's heapq orders `(-gain, n)`.
    fn cmp(&self, other: &Self) -> Ordering {
        self.gain.total_cmp(&other.gain).then(other.n.cmp(&self.n))
    }
}

/// Lazy greedy weighted coverage: value of not-yet-covered tokens per char,
/// within `room` chars. Returns the keys picked, in pick order, and adds their
/// tokens to `covered`.
pub fn lazy_cover<K: Clone>(
    cands: &[Candidate<'_, K>],
    covered: &mut HashSet<String>,
    room: usize,
) -> Vec<K> {
    let worth = |c: &Candidate<'_, K>, covered: &HashSet<String>| {
        c.toks
            .iter()
            .filter(|t| !covered.contains(*t))
            .map(|t| c.values.get(t).copied().unwrap_or(0.0))
            .sum::<f64>()
            / (c.chars + 1) as f64
    };
    let mut heap: BinaryHeap<Entry> = cands
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.toks.is_empty())
        .map(|(n, c)| Entry {
            gain: c
                .toks
                .iter()
                .map(|t| c.values.get(t).copied().unwrap_or(0.0))
                .sum::<f64>()
                / (c.chars + 1) as f64,
            n,
        })
        .collect();
    let mut picked = Vec::new();
    let mut used = 0usize;
    while let Some(Entry { n, .. }) = heap.pop() {
        let c = &cands[n];
        let gain = worth(c, covered);
        if gain <= 0.0 {
            continue;
        }
        if let Some(top) = heap.peek() {
            if gain < top.gain - 1e-12 {
                heap.push(Entry { gain, n });
                continue;
            }
        }
        if used + c.chars + 1 > room {
            continue;
        }
        picked.push(c.key.clone());
        used += c.chars + 1;
        covered.extend(c.toks.iter().cloned());
    }
    picked
}

/// Byte offsets where `t` occurs in `s` with no letter or digit touching it.
fn aligned_at(s: &str, t: &str) -> Vec<usize> {
    if t.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(off) = s[from..].find(t) {
        let p = from + off;
        let e = p + t.len();
        let before = s[..p].chars().next_back();
        let after = s[e..].chars().next();
        if !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric) {
            out.push(p);
        }
        from = p + s[p..].chars().next().map_or(1, char::len_utf8);
    }
    out
}

fn aligned(s: &str, t: &str) -> bool {
    !aligned_at(s, t).is_empty()
}

/// Each piece's tokens plus every key visible inside it as a word: a reader sees
/// those too, so a piece that shows one covers it.
pub fn contained(units: &[String], keys: &[&String]) -> Vec<HashSet<String>> {
    let mut res: Vec<HashSet<String>> = units.iter().map(|u| toks(u)).collect();
    let joined = units.join("\n");
    let mut starts = Vec::with_capacity(units.len());
    let mut o = 0;
    for u in units {
        starts.push(o);
        o += u.len() + 1;
    }
    for t in keys {
        for p in aligned_at(&joined, t) {
            let i = starts.partition_point(|&s| s <= p) - 1;
            if p + t.len() <= starts[i] + units[i].len() {
                res[i].insert((*t).clone());
            }
        }
    }
    res
}

/// One budget for a whole compaction: the chars every stub's greedy spent are
/// refilled by one lazy greedy over the pieces of all stubs; each keeps its
/// preamble. `covered` is every token kept outside the greedy lines anywhere in
/// the compaction; with `side` (the compaction's text outside the greedy lines)
/// coverage counts tokens visible inside longer ones. Returns each stub's lines.
pub fn pool_lines<K: Clone + Eq + std::hash::Hash>(
    parts: &[(K, &ValueParts, &HashMap<String, f64>)],
    covered: &mut HashSet<String>,
    side: Option<&str>,
) -> HashMap<K, Vec<String>> {
    let room: usize = parts
        .iter()
        .map(|(_, p, _)| {
            p.greedy
                .iter()
                .map(|&i| clen(&p.units[i]) + 1)
                .sum::<usize>()
        })
        .sum();
    let sets: Vec<Vec<HashSet<String>>> = parts
        .iter()
        .map(|(_, p, values)| match side {
            Some(_) => contained(&p.units, &values.keys().collect::<Vec<_>>()),
            None => p.units.iter().map(|u| toks(u)).collect(),
        })
        .collect();
    if let Some(side) = side {
        for (_, _, values) in parts {
            covered.extend(values.keys().filter(|t| aligned(side, t)).cloned());
        }
    }
    let mut cands = Vec::new();
    for (k, ((_, p, values), unit_toks)) in parts.iter().zip(&sets).enumerate() {
        for (i, (unit, ts)) in p.units.iter().zip(unit_toks).enumerate() {
            if !p.pre.contains(&i) {
                cands.push(Candidate {
                    key: (k, i),
                    chars: clen(unit),
                    toks: ts.clone(),
                    values,
                });
            }
        }
    }
    let mut picks: Vec<Vec<usize>> = vec![Vec::new(); parts.len()];
    for (k, i) in lazy_cover(&cands, covered, room) {
        picks[k].push(i);
    }
    parts
        .iter()
        .enumerate()
        .map(|(k, (key, p, _))| (key.clone(), p.lines(&picks[k])))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_probabilities_and_reuse_raises_them() {
        let text = "build 7f3a9c21e done in 412ms\nartifact at /srv/out/app-1.2.3.tar";
        let base = ValueContext::default();
        let v = token_values(text, &base);
        assert!(v.values().all(|p| *p > 0.0 && *p < 1.0));
        let mut reused = HashSet::new();
        reused.insert("7f3a9c21e".to_owned());
        let ctx = ValueContext {
            reused: Arc::new(reused),
            ..base
        };
        let w = token_values(text, &ctx);
        assert!(w["7f3a9c21e"] > v["7f3a9c21e"]);
    }

    #[test]
    fn naming_a_token_in_input_or_user_raises_its_value() {
        let text = "build 7f3a9c21e done in 412ms\nartifact at /srv/out/app-1.2.3.tar";
        let base = ValueContext::default();
        let v = token_values(text, &base);
        let key = "7f3a9c21e";
        let with_input = ValueContext {
            input: key.to_owned(),
            ..base.clone()
        };
        let with_user = ValueContext {
            user: key.to_owned(),
            ..base
        };
        assert!(token_values(text, &with_input)[key] > v[key]);
        assert!(token_values(text, &with_user)[key] > v[key]);
    }

    #[test]
    fn contexts_track_reuse_and_distance() {
        use crate::model::{ToolResult, ToolUse};
        let mut input = serde_json::Map::new();
        input.insert("command".into(), "curl http://h/x".into());
        let mut input2 = serde_json::Map::new();
        input2.insert("command".into(), "kill 993311".into());
        let ms = vec![
            Message::text(Role::User, "deploy it"),
            Message {
                role: Role::Assistant,
                text: String::new(),
                tool_uses: vec![ToolUse {
                    tool_use_id: "a".into(),
                    tool: "Bash".into(),
                    input,
                    text: None,
                    is_error: false,
                }],
                tool_results: vec![],
            },
            Message {
                role: Role::User,
                text: String::new(),
                tool_uses: vec![],
                tool_results: vec![ToolResult {
                    tool_use_id: "a".into(),
                    text: "pid 993311 started".into(),
                    is_error: false,
                }],
            },
            Message {
                role: Role::Assistant,
                text: String::new(),
                tool_uses: vec![ToolUse {
                    tool_use_id: "b".into(),
                    tool: "Bash".into(),
                    input: input2,
                    text: None,
                    is_error: false,
                }],
                tool_results: vec![],
            },
            Message {
                role: Role::User,
                text: String::new(),
                tool_uses: vec![],
                tool_results: vec![ToolResult {
                    tool_use_id: "b".into(),
                    text: "ok".into(),
                    is_error: false,
                }],
            },
        ];
        let ctxs = message_contexts(&ms);
        assert_eq!(ctxs["a"].dist, 2);
        assert_eq!(ctxs["b"].dist, 1);
        assert!(ctxs["a"].reused.contains("993311"));
        assert_eq!(ctxs["a"].user, "deploy it");
        assert!(ctxs["a"].input.contains("curl"));
    }

    #[test]
    fn value_parts_stay_within_the_regex_budget() {
        let text: String = (0..200)
            .map(|i| format!("row {i} id=ab{i:06}cd status ok\n"))
            .collect();
        let values = token_values(&text, &ValueContext::default());
        let budget = 600;
        let parts = value_parts(&text, budget, &values);
        let cap = crate::rails::lines_chars(&fact_lines(&text, budget));
        let kept = parts.lines(&parts.greedy);
        assert!(crate::rails::lines_chars(&kept) <= cap);
        assert!(!kept.is_empty());
    }

    #[test]
    fn contained_sees_a_hash_inside_a_path() {
        let units = vec!["see /tmp/build-1a2b3c4d/out.txt".to_owned()];
        let key = "1a2b3c4d".to_owned();
        let sets = contained(&units, &[&key]);
        assert!(sets[0].contains("1a2b3c4d"));
        assert!(aligned("x 1a2b3c4d y", "1a2b3c4d"));
        assert!(!aligned("x1a2b3c4d", "1a2b3c4d"));
    }

    #[test]
    fn pooling_spends_no_more_than_the_stubs_spent() {
        let a: String = (0..120)
            .map(|i| format!("alpha {i} token_{i:04}x9 value\n"))
            .collect();
        let b: String = (0..120)
            .map(|i| format!("beta {i} ref-{i:05}z queue\n"))
            .collect();
        let va = token_values(&a, &ValueContext::default());
        let vb = token_values(&b, &ValueContext::default());
        let pa = value_parts(&a, 400, &va);
        let pb = value_parts(&b, 400, &vb);
        let spent = |p: &ValueParts| {
            p.greedy
                .iter()
                .map(|&i| clen(&p.units[i]) + 1)
                .sum::<usize>()
        };
        let before = spent(&pa) + spent(&pb);
        let parts = vec![("a", &pa, &va), ("b", &pb, &vb)];
        let pooled = pool_lines(&parts, &mut HashSet::new(), Some(""));
        let pre = |p: &ValueParts| p.pre.iter().map(|&i| clen(&p.units[i]) + 1).sum::<usize>();
        let after = crate::rails::lines_chars(&pooled["a"])
            + crate::rails::lines_chars(&pooled["b"])
            - pre(&pa)
            - pre(&pb);
        assert!(after <= before, "{after} > {before}");
    }
}
