//! The taint fence: which tool calls make a session must-not-leave, and how
//! that verdict is made sticky across compactions.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::coerce::{finite_number, js_string};
use crate::config::Config;

/// The sample shown when a sticky record carries none of its own.
const EARLIER: &str = "(earlier in this session)";

/// One tool call, reduced to what the taint rule reads.
///
/// Deserialises from `{"tool": "mcp__x__y", "skill": null}`, `skill` being
/// `input.skill ?? input.name` of a `Skill` call. Deserialisation is lenient so
/// a malformed entry can never fail a request: a missing, null or non-scalar
/// `tool` reads as `""` (which taints nothing), a number or boolean as its text.
///
/// ```
/// use factrail_policy::ToolRef;
/// let t: ToolRef = serde_json::from_str(r#"{"tool": 7}"#).unwrap();
/// assert_eq!(t, ToolRef { tool: "7".into(), skill: None });
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolRef {
    /// The tool's name, e.g. `mcp__email-gateway__ask_email` or `Skill`.
    #[serde(default, deserialize_with = "lenient_string")]
    pub tool: String,
    /// For a `Skill` call, the skill it loads.
    #[serde(default, deserialize_with = "lenient_option")]
    pub skill: Option<String>,
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(_) | Value::Number(_) | Value::Bool(_) => Some(js_string(value)),
        _ => None,
    }
}

fn lenient_string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(scalar_text(&Value::deserialize(d)?).unwrap_or_default())
}

fn lenient_option<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(scalar_text(&Value::deserialize(d)?))
}

/// What is known of a session's taint.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct TaintScan {
    /// At least one tainting call was seen (or recorded earlier, when `sticky`).
    pub tainted: bool,
    /// How many tainting calls.
    pub count: usize,
    /// Up to three distinct offending tool names, in first-seen order, for the log line.
    pub sample: Vec<String>,
    /// The verdict comes from the stored record, not from this scan.
    pub sticky: bool,
}

/// Does one tool call taint the session?
///
/// A prefix match on the tool name, anchored at the start — an MCP server's
/// every tool shares its `mcp__<server>__` prefix, and a prefix cannot be
/// mis-anchored the way a pattern can — or a `Skill` call loading a listed skill.
///
/// ```
/// use factrail_policy::{Config, ToolRef, taints_tool};
/// let config = Config::default();
/// let email = ToolRef { tool: "mcp__email-gateway__ask_email".into(), skill: None };
/// let other = ToolRef { tool: "mcp__other__mcp__email-gateway__x".into(), skill: None };
/// assert!(taints_tool(&email, &config));
/// assert!(!taints_tool(&other, &config));
/// ```
pub fn taints_tool(tool: &ToolRef, config: &Config) -> bool {
    let name = tool.tool.as_str();
    if config
        .taint_tools
        .iter()
        .any(|p| !p.is_empty() && name.starts_with(p.as_str()))
    {
        return true;
    }
    name == "Skill"
        && config
            .taint_skills
            .iter()
            .any(|s| s == tool.skill.as_deref().unwrap_or(""))
}

/// Does a skill expansion taint the session? The bare name or a
/// plugin-qualified one (`some-plugin:email-search`), because a skill typed as a
/// slash command expands without any `Skill` call in the transcript.
///
/// ```
/// use factrail_policy::{Config, skill_taints};
/// let config = Config::default();
/// assert!(skill_taints("agentbox:email-search", &config));
/// assert!(!skill_taints("email-search-extra", &config));
/// ```
pub fn skill_taints(skill: &str, config: &Config) -> bool {
    !skill.is_empty()
        && config
            .taint_skills
            .iter()
            .any(|s| !s.is_empty() && (skill == s || skill.ends_with(&format!(":{s}"))))
}

/// Scan a session's tool calls. Every call counts, pinned or not: a pinned
/// email call is still an email call in a transcript that would otherwise leave.
pub fn scan_taint(tools: &[ToolRef], config: &Config) -> TaintScan {
    let hits: Vec<&str> = tools
        .iter()
        .filter(|t| taints_tool(t, config))
        .map(|t| t.tool.as_str())
        .collect();
    let mut sample: Vec<String> = Vec::new();
    for hit in &hits {
        if sample.len() == 3 {
            break;
        }
        if !sample.iter().any(|s| s == hit) {
            sample.push((*hit).to_owned());
        }
    }
    TaintScan {
        tainted: !hits.is_empty(),
        count: hits.len(),
        sample,
        sticky: false,
    }
}

/// Is a stored record a taint? Only an object whose `tainted` is JSON `true`.
pub fn sticky_tainted(sticky: Option<&Value>) -> bool {
    matches!(
        sticky.and_then(|v| v.get("tainted")),
        Some(Value::Bool(true))
    )
}

/// Merge the stored record with a fresh scan. Once tainted, always tainted: a
/// built-in summary can absorb email content and leave no tool call behind, so
/// a later clean scan is not evidence of cleanliness.
///
/// A fresh tainted scan wins as it stands. Otherwise a stored taint yields a
/// sticky verdict whose `count` is the record's (a positive number, floored,
/// else 1) and whose `sample` is the record's first three entries, or a
/// placeholder when it has none. A malformed record is not a taint.
///
/// ```
/// use factrail_policy::{TaintScan, merge_taint};
/// use serde_json::json;
/// let record = json!({ "tainted": true, "count": 2, "sample": ["mcp__email-gateway__x"], "at": 1 });
/// let merged = merge_taint(Some(&record), TaintScan::default());
/// assert!(merged.tainted && merged.sticky);
/// assert!(!merge_taint(Some(&json!("garbage")), TaintScan::default()).tainted);
/// ```
pub fn merge_taint(sticky: Option<&Value>, scan: TaintScan) -> TaintScan {
    if scan.tainted || !sticky_tainted(sticky) {
        return scan;
    }
    let record = sticky.unwrap_or(&Value::Null);
    let count = match finite_number(record.get("count")) {
        Some(n) if n >= 1.0 => n.floor() as usize,
        _ => 1,
    };
    let mut sample: Vec<String> = match record.get("sample") {
        Some(Value::Array(items)) => items.iter().take(3).map(js_string).collect(),
        _ => Vec::new(),
    };
    if sample.is_empty() {
        sample.push(EARLIER.to_owned());
    }
    TaintScan {
        tainted: true,
        count,
        sample,
        sticky: true,
    }
}

/// The record stored under [`taint_key`] when a session is first seen tainted.
///
/// ```
/// use factrail_policy::{TaintScan, taint_record};
/// use serde_json::json;
/// let scan = TaintScan { tainted: true, count: 1, sample: vec!["mcp__email-gateway__x".into()], sticky: false };
/// assert_eq!(taint_record(&scan, 5), json!({ "tainted": true, "count": 1, "sample": ["mcp__email-gateway__x"], "at": 5 }));
/// ```
pub fn taint_record(scan: &TaintScan, now_ms: i64) -> Value {
    let sample: Vec<&String> = scan.sample.iter().take(3).collect();
    json!({ "tainted": true, "count": scan.count, "sample": sample, "at": now_ms })
}

/// The store write a scan calls for: the [`taint_record`] when the scan is
/// tainted and the store does not already hold a taint, else `None`. The first
/// record is kept, so the timestamp says when the session was first tainted.
///
/// For a single tool call (`tool.call`) or skill expansion (`skill.prompt`),
/// pass a scan of `count: 1` naming it.
pub fn taint_write(sticky: Option<&Value>, scan: &TaintScan, now_ms: i64) -> Option<Value> {
    (scan.tainted && !sticky_tainted(sticky)).then(|| taint_record(scan, now_ms))
}

/// The plugin-store key that makes one session's taint sticky: `taint:<session>`.
pub fn taint_key(session: &str) -> String {
    format!("taint:{session}")
}

/// The plugin-store key holding one session's post-compaction size: `baseline:<session>`.
pub fn baseline_key(session: &str) -> String {
    format!("baseline:{session}")
}
