//! Fact rails: how a result the judge let go is reduced without erasing a fact.
//!
//! Nothing is erased. A *reproducible read* (a file read, a search, `git log`)
//! longer than its tier's `read_keep` shrinks to a one-line note, because a re-run
//! gives it back. Anything else is an *observation* — of the network, a process,
//! a log, a side effect — that a re-run does not give back, so it keeps its head,
//! its *fact lines* (errors, HTTP codes, paths, versions, ids, endpoints, counts,
//! receipts) and its tail. Short observations, dense dumps and error heads are
//! never cut. Every reduction names where the full output is.

use std::sync::LazyLock;

use regex::{Regex, RegexBuilder};
use serde_json::{Map, Value};

use crate::model::input_json;
use crate::text::{clen, cslice, find_char, lines, rfind_char};

fn re(pattern: &str, case_insensitive: bool) -> Regex {
    RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
        .expect("static regex")
}

/// Fact-line patterns; a line's score is how many match it.
static FACT_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        re(
            r"\b(error|errors|failed|failure|fail|exception|traceback|denied|refused|invalid|not found|timed? ?out|fatal|panic|warning)\b|invalid_\w+",
            true,
        ),
        re(r"✖|✗|\bFAIL\b|\bERR\b", false),
        re(r"\bHTTP\b[\s/\d.]*\d{3}\b|\bstatus[\s:=]+\d{3}\b", true),
        re(
            r#"[A-Za-z]:[\\/][^\s"'<>]+|(?:^|[\s"'(=])/(?:[\w.@-]+/)+[\w.@-]+"#,
            false,
        ),
        re(r"\b\d+\.\d+\.\d+\b", false),
        re(r"\b[0-9a-f]{7,40}\b", false),
        re(
            r"\b(?:\d{1,3}\.){3}\d{1,3}(?::\d+)?\b|\blocalhost:\d+\b",
            false,
        ),
        re(
            r"\b(pid|port|exit|rc|code|size|bytes|pass(?:ed)?|fail(?:ed)?|tests?|total|count)\b[\s:=]*\d",
            true,
        ),
        re(
            r"\b\d[\d,.]*\s?(ms|kb|mb|gb|bytes|%|tokens|lines?|files?)\b",
            true,
        ),
        re(r"\b\d{4,}\b", false),
        re(
            r"\b[\w.-]+\.(?:[cm]?[jt]sx?|py|json|ya?ml|md|toml|txt|log|rs|go|sh|ps1|cmd|lock|sql)\b",
            true,
        ),
        // Receipts of non-idempotent calls: a re-run would send or create again.
        re(
            r"\b(?:message_id|ticket|confirm(?:ed)?|sent|delivered|created|updated|deleted|order|transaction|commit)\b[\s:=#]+[\w-]+",
            true,
        ),
    ]
});

/// A line longer than this is split into pieces rather than cut.
pub const FACT_LINE_CHARS: usize = 200;
/// Head kept by a fact stub.
pub const FACT_HEAD_CHARS: usize = 200;
/// Tail kept by a fact stub.
pub const FACT_TAIL_CHARS: usize = 120;
/// Minimum fact-line budget of a stub.
pub const FACT_BUDGET_CHARS: usize = 360;
/// Head an error result keeps.
pub const ERROR_KEEP_CHARS: usize = 2000;
/// String arguments of a reduced call are cut to this.
pub const INPUT_BRIEF_CHARS: usize = 200;
/// String arguments of a reproducible read keep this much: enough to re-run it.
pub const RERUN_INPUT_CHARS: usize = 160;

/// How many fact patterns match `line`.
pub fn fact_score(line: &str) -> usize {
    if line.is_empty() {
        return 0;
    }
    FACT_PATTERNS.iter().filter(|p| p.is_match(line)).count()
}

/// Splits a line on literal `\n` escapes (JSON with escaped newlines), then
/// splits any piece longer than [`FACT_LINE_CHARS`] at a separator in its second
/// half, so a fact deep in a long line is still a candidate.
///
/// ```
/// use factrail_core::rails::pieces;
/// assert_eq!(pieces(r"a\nb"), vec!["a", "b"]);
/// let long = format!("{} id=42", "w ".repeat(150));
/// assert!(pieces(&long).iter().all(|p| p.chars().count() <= 200));
/// ```
pub fn pieces(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in line.split("\\n") {
        let mut rest = part.trim().to_owned();
        while clen(&rest) > FACT_LINE_CHARS {
            let window = cslice(&rest, 0, FACT_LINE_CHARS);
            let cut = [", ", "; ", " | ", " "]
                .iter()
                .filter_map(|sep| window.rfind(sep).map(|b| crate::text::char_at(window, b)))
                .max();
            let end = match cut {
                Some(c) if c > FACT_LINE_CHARS / 2 => c + 1,
                _ => FACT_LINE_CHARS,
            };
            out.push(cslice(&rest, 0, end).trim().to_owned());
            rest = cslice(&rest, end, usize::MAX).trim().to_owned();
        }
        if !rest.is_empty() {
            out.push(rest);
        }
    }
    out
}

/// Every piece of `text`, in order (the units fact lines are chosen from).
pub fn units(text: &str) -> Vec<String> {
    lines(text).flat_map(pieces).collect()
}

/// Lines of `text` that carry facts, most fact-dense first until `budget` chars
/// (each line costs its length plus one), returned in text order.
///
/// ```
/// use factrail_core::rails::fact_lines;
/// let out = "compiling\nerror: build failed at src/main.rs:12\nok\n";
/// assert_eq!(fact_lines(out, 1000), vec!["error: build failed at src/main.rs:12"]);
/// ```
pub fn fact_lines(text: &str, budget: usize) -> Vec<String> {
    let mut scored: Vec<(usize, usize, String)> = units(text)
        .into_iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let score = fact_score(&line);
            (score > 0).then_some((score, index, line))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut picked: Vec<(usize, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut used = 0usize;
    for (_, index, line) in scored {
        let cost = clen(&line) + 1;
        if seen.contains(&line) || used + cost > budget {
            continue;
        }
        used += cost;
        seen.insert(line.clone());
        picked.push((index, line));
    }
    picked.sort_by_key(|p| p.0);
    picked.into_iter().map(|p| p.1).collect()
}

/// Chars `lines` occupy when joined, one newline each.
pub fn lines_chars(lines: &[String]) -> usize {
    lines.iter().map(|l| clen(l) + 1).sum()
}

/// One rail tier.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rails {
    /// An observation up to this many chars is never cut.
    pub small: usize,
    /// Fact-line budget as a share of the result.
    pub share: f64,
    /// A read up to this many chars is kept, not turned into a re-run note.
    pub read_keep: usize,
    /// A dense dump up to this many chars is kept.
    pub dense_keep: usize,
    /// "Dense": fact lines are at least this share of the chars.
    pub dense_share: f64,
}

/// The tiers, loosest first: reads give way first (a re-run gives them back),
/// then observations lose rails step by step.
pub const RAIL_TIERS: [Rails; 4] = [
    Rails {
        small: 6000,
        share: 0.3,
        read_keep: 3000,
        dense_keep: 32_000,
        dense_share: 0.5,
    },
    Rails {
        small: 6000,
        share: 0.3,
        read_keep: 0,
        dense_keep: 32_000,
        dense_share: 0.5,
    },
    Rails {
        small: 3000,
        share: 0.2,
        read_keep: 0,
        dense_keep: 0,
        dense_share: 1.0,
    },
    Rails {
        small: 0,
        share: 0.1,
        read_keep: 0,
        dense_keep: 0,
        dense_share: 1.0,
    },
];

/// Reported as the rail tier when the oldest results had to give way.
pub const LAST_RESORT_TIER: usize = RAIL_TIERS.len();

/// The marker every reduced result carries. A result carrying any of these is
/// final: reducing it again would cut the facts it kept. The two older spellings
/// are the engines this one replaces, so a session compacted by them is
/// recognised too.
static COMPACTED_MARK: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"\[(?:factrail|fast-jev-compaction|jev-compaction) (?:omitted|truncated) ",
        false,
    )
});

/// True when `text` was already reduced by a compaction.
pub fn is_compacted(text: &str) -> bool {
    COMPACTED_MARK.is_match(text)
}

/// Where a reduced result's full output is: the phrase every stub carries, which
/// the caller swaps for a saved file's path once it has written one. The id is
/// backquoted so no call's note is a prefix of another's (`u1` vs `u12`).
///
/// ```
/// use factrail_core::rails::full_output_note;
/// assert!(!full_output_note("toolu_12").contains(&full_output_note("toolu_1")));
/// ```
pub fn full_output_note(id: &str) -> String {
    format!("the full output stays in this session's transcript under `{id}`")
}

/// The phrase that replaces [`full_output_note`] once the output is saved at `path`.
pub fn saved_output_note(path: &str) -> String {
    format!("the full output is saved at {path}; Read it for anything not kept here")
}

/// Where a stub cuts a result, or that it keeps it whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Layout {
    /// Kept whole: short, or a dense dump.
    Whole,
    /// Head `[0, head_end)`, fact lines chosen from `[head_end, tail_start)` within
    /// `budget` chars, tail `[tail_start, len)`. All in `char`s.
    Stub {
        /// End of the kept head.
        head_end: usize,
        /// Start of the kept tail.
        tail_start: usize,
        /// Fact-line budget.
        budget: usize,
    },
}

/// Where an observation is cut under `rails`, before its fact lines are chosen.
/// `head_chars` is the head a stub keeps ([`FACT_HEAD_CHARS`] by default); an
/// error keeps at least [`ERROR_KEEP_CHARS`].
pub fn stub_layout(text: &str, is_error: bool, rails: &Rails, head_chars: usize) -> Layout {
    let len = clen(text);
    let head_keep = if is_error {
        head_chars.max(ERROR_KEEP_CHARS)
    } else {
        head_chars
    };
    if len <= rails.small.max(head_keep + FACT_TAIL_CHARS + 120) {
        return Layout::Whole;
    }
    if len <= rails.dense_keep
        && (lines_chars(&fact_lines(text, usize::MAX)) as f64) >= len as f64 * rails.dense_share
    {
        return Layout::Whole;
    }
    let head_end = match rfind_char(text, '\n', head_keep) {
        Some(nl) if nl * 2 > head_keep => nl,
        _ => head_keep,
    };
    let tail_start = match find_char(text, '\n', len - FACT_TAIL_CHARS) {
        Some(nl) if nl + 1 < len => nl + 1,
        _ => len - FACT_TAIL_CHARS,
    };
    let budget = FACT_BUDGET_CHARS.max((len as f64 * rails.share) as usize);
    Layout::Stub {
        head_end,
        tail_start,
        budget,
    }
}

/// The stub text for a [`Layout::Stub`] with `facts` chosen.
pub fn render_stub(
    text: &str,
    head_end: usize,
    tail_start: usize,
    facts: &[String],
    is_error: bool,
    id: &str,
) -> String {
    let mut out = String::with_capacity(head_end + 200 + lines_chars(facts) + FACT_TAIL_CHARS);
    out.push_str(cslice(text, 0, head_end));
    out.push_str(&format!(
        "\n[factrail omitted {} chars of this tool result{}{}; {}]\n",
        tail_start - head_end,
        if is_error { " (error)" } else { "" },
        if facts.is_empty() {
            String::new()
        } else {
            format!("; kept its {} fact line(s)", facts.len())
        },
        full_output_note(id),
    ));
    if !facts.is_empty() {
        out.push_str(&facts.join("\n"));
        out.push_str("\n…\n");
    }
    out.push_str(cslice(text, tail_start, usize::MAX));
    out
}

/// One line standing in for a reproducible read.
pub fn rerun_note(text: &str, id: &str) -> String {
    let len = clen(text);
    if len <= 160 {
        return text.to_owned();
    }
    format!(
        "[factrail omitted {len} chars: a reproducible read, re-run the tool to see it; {}]",
        full_output_note(id)
    )
}

const READ_TOOLS: &[&str] = &[
    // Claude Code
    "Read",
    "Grep",
    "Glob",
    "LS",
    "NotebookRead",
    "ToolSearch",
    // Hermes, Codex and other OpenAI-chat agents
    "read_file",
    "search_files",
    "list_files",
    "list_directory",
    "grep",
    "glob",
];
const SHELL_TOOLS: &[&str] = &[
    "Bash",
    "PowerShell",
    "terminal",
    "shell",
    "bash",
    "exec_command",
];
const READ_VERBS: &[&str] = &[
    "cd",
    "echo",
    "printf",
    "true",
    "ls",
    "dir",
    "cat",
    "type",
    "head",
    "tail",
    "wc",
    "find",
    "fd",
    "rg",
    "grep",
    "egrep",
    "sha256sum",
    "sha1sum",
    "md5sum",
    "stat",
    "file",
    "tree",
    "cut",
    "tr",
    "sort",
    "uniq",
    "sed",
    "awk",
    "jq",
    "basename",
    "dirname",
    "realpath",
    "es",
    "es.exe",
    "get-content",
    "get-childitem",
    "select-string",
    "select-object",
    "measure-object",
    "get-filehash",
    "test-path",
    "resolve-path",
    "format-table",
    "out-string",
];
/// File metadata is a measurement taken at one moment: a read reporting it is an observation.
const METADATA_VERBS: &[&str] = &[
    "wc",
    "stat",
    "du",
    "df",
    "dir",
    "get-childitem",
    "gci",
    "measure-object",
];

static GIT_READS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^git\s+(log|show|diff|status|blame|ls-files|rev-parse|branch|remote|describe)\b",
        false,
    )
});
/// A log, a JSONL ledger or a followed stream changes under you.
static MUTABLE_SOURCE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"\.(?:log|jsonl|out|err)\b|[\\/]logs?[\\/]|\bjournalctl\b|\b(?:docker|kubectl)\s+logs\b|-Tail\b|-Wait\b|\btail\s+-[a-zA-Z]*[fF]",
        true,
    )
});
static REDIRECT_NOISE: LazyLock<Regex> =
    LazyLock::new(|| re(r"2>&1|2>/dev/null|2>\$null|>\s*/dev/null|>\s*\$null", false));
static TEE_OR_PIPE_TO_SHELL: LazyLock<Regex> =
    LazyLock::new(|| re(r"\btee\b|\|\s*(sh|bash|pwsh|iex)\b", false));
static SEGMENTS: LazyLock<Regex> = LazyLock::new(|| re(r"\r?\n|;|&&|\|\||\|", false));
static ENV_PREFIX: LazyLock<Regex> = LazyLock::new(|| re(r"^(?:\w+=\S*\s+)+", false));
static TIMEOUT_PREFIX: LazyLock<Regex> = LazyLock::new(|| re(r"^timeout\s+\S+\s+", false));
static COMMAND_PREFIX: LazyLock<Regex> = LazyLock::new(|| re(r"^command\s+", false));
static SED_IN_PLACE: LazyLock<Regex> = LazyLock::new(|| re(r"^sed\s+(-\w*i|--in-place)", false));
static LONG_LISTING: LazyLock<Regex> = LazyLock::new(|| re(r"^ls\s+(?:\S+\s+)*-[a-zA-Z]*l", false));
/// Markers in a read's output that make it an observation (a failure, a timeout, a background job).
static READ_OBSERVATION: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"\b(?:errno|os error|timed? ?out|timeout|permission denied|access is denied|no such file|cannot find|not found|running in background|background with id|exit code [1-9]|killed)\b",
        true,
    )
});

/// A `>` or `>>` that writes a file: preceded by the start or by anything but
/// `>`, `2` or `&`, and not followed by `&` (the lookaround of the original
/// pattern `(^|[^>2&])>{1,2}(?!&)`, written out).
fn writes_file(command: &str) -> bool {
    let bytes = command.as_bytes();
    bytes.iter().enumerate().any(|(i, &b)| {
        b == b'>'
            && (i == 0 || !matches!(bytes[i - 1], b'>' | b'2' | b'&'))
            && bytes.get(i + 1) != Some(&b'&')
    })
}

/// True when re-running the call gives its output back: a read of files, not of the world.
///
/// ```
/// use factrail_core::rails::reproducible;
/// let cmd = |c: &str| serde_json::json!({"command": c}).as_object().unwrap().clone();
/// assert!(reproducible("Bash", &cmd("cat src/lib.rs | head -50")));
/// assert!(!reproducible("Bash", &cmd("cat out > copy.txt")));
/// assert!(!reproducible("Bash", &cmd("wc -l src/lib.rs")));       // metadata is an observation
/// assert!(!reproducible("Bash", &cmd("tail -n 20 server.log")));  // logs change
/// assert!(reproducible("Read", &serde_json::Map::new()));
/// ```
pub fn reproducible(tool: &str, input: &Map<String, Value>) -> bool {
    if MUTABLE_SOURCE.is_match(&input_json(input)) {
        return false;
    }
    if READ_TOOLS.contains(&tool) {
        return true;
    }
    if !SHELL_TOOLS.contains(&tool) {
        return false;
    }
    let command = match input.get("command").or_else(|| input.get("cmd")) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => return false,
    };
    if command.is_empty() {
        return false;
    }
    let quiet = REDIRECT_NOISE.replace_all(&command, "");
    if writes_file(&quiet) || TEE_OR_PIPE_TO_SHELL.is_match(&quiet) {
        return false;
    }
    SEGMENTS.split(&command).all(|segment| {
        let words = ENV_PREFIX.replace(segment.trim(), "");
        let words = TIMEOUT_PREFIX.replace(&words, "");
        let words = COMMAND_PREFIX.replace(&words, "").into_owned();
        if words.is_empty() || GIT_READS.is_match(&words) {
            return true;
        }
        if SED_IN_PLACE.is_match(&words) {
            return false;
        }
        let verb = words
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches(|c| c == '"' || c == '\'')
            .to_lowercase();
        if METADATA_VERBS.contains(&verb.as_str()) || LONG_LISTING.is_match(&words) {
            return false;
        }
        READ_VERBS.contains(&verb.as_str())
    })
}

/// True when a read's output reports a failure, a timeout or a background job,
/// which makes it an observation however it was produced.
pub fn read_is_observation(text: &str) -> bool {
    READ_OBSERVATION.is_match(text)
}

/// String fields of an argument value cut to `max` chars, recursively: a reduced
/// call stays readable, not verbatim.
///
/// ```
/// use factrail_core::rails::brief_input;
/// let v = brief_input(&serde_json::json!({"a": "abcdefghijklmnopqrst", "n": 3}), 2);
/// assert_eq!(v, serde_json::json!({"a": "ab…[18 chars]", "n": 3}));
/// // A string only a little over the cap stays whole: the cut form would be longer.
/// let v = brief_input(&serde_json::json!("abcdef"), 2);
/// assert_eq!(v, serde_json::json!("abcdef"));
/// ```
pub fn brief_input(value: &Value, max: usize) -> Value {
    match value {
        Value::String(s) => {
            let len = clen(s);
            if len <= max {
                return value.clone();
            }
            // The cut string carries a suffix; cut only when the result is shorter.
            let cut = format!("{}…[{} chars]", cslice(s, 0, max), len - max);
            if clen(&cut) < len {
                Value::String(cut)
            } else {
                value.clone()
            }
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| brief_input(v, max)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), brief_input(v, max)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// [`brief_input`] over a whole argument map.
pub fn brief_map(input: &Map<String, Value>, max: usize) -> Map<String, Value> {
    input
        .iter()
        .map(|(k, v)| (k.clone(), brief_input(v, max)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(c: &str) -> Map<String, Value> {
        serde_json::json!({ "command": c })
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn reproducible_reads_and_observations() {
        assert!(reproducible(
            "Bash",
            &cmd("git log --oneline -5 && git status")
        ));
        assert!(reproducible(
            "Bash",
            &cmd("FOO=1 timeout 5 rg foo src 2>/dev/null")
        ));
        assert!(!reproducible("Bash", &cmd("sed -i s/a/b/ f")));
        assert!(!reproducible("Bash", &cmd("ls -la dir")));
        assert!(!reproducible("Bash", &cmd("cat card; ls -la dir")));
        assert!(!reproducible("Bash", &cmd("curl https://x")));
        assert!(!reproducible("Bash", &cmd("echo hi | bash")));
        assert!(!reproducible("Bash", &cmd("cat a | tee b")));
        assert!(reproducible("Bash", &cmd("cat a 2>&1")));
        assert!(!reproducible(
            "Read",
            &serde_json::json!({"file_path": "/var/log/x.log"})
                .as_object()
                .unwrap()
                .clone()
        ));
        assert!(!reproducible("WebFetch", &Map::new()));
        assert!(reproducible("read_file", &Map::new()));
    }

    #[test]
    fn write_detection_matches_the_lookaround() {
        assert!(writes_file("a > b"));
        assert!(writes_file(">b"));
        assert!(writes_file("a >> b"));
        assert!(!writes_file("a 2>b"));
        assert!(!writes_file("a >&2"));
        assert!(writes_file("a >>&2")); // `>` then `>`: the first `>` is not followed by `&`
    }

    #[test]
    fn fact_lines_rank_dedupe_and_keep_order() {
        let text =
            "noise\nGET /api 200 HTTP/1.1 200\nerror: x failed\nnoise\nerror: x failed\npid 4242\n";
        let lines = fact_lines(text, 1000);
        assert_eq!(
            lines,
            vec!["GET /api 200 HTTP/1.1 200", "error: x failed", "pid 4242"]
        );
        // "pid 4242" matches two patterns (a counter word, four digits), the error line one.
        let tight = fact_lines(text, 17);
        assert_eq!(tight, vec!["pid 4242"]);
    }

    #[test]
    fn layout_keeps_small_and_dense_and_cuts_on_lines() {
        assert_eq!(
            stub_layout("short", false, &RAIL_TIERS[0], FACT_HEAD_CHARS),
            Layout::Whole
        );
        let dense: String = (0..1500)
            .map(|i| format!("pid {i:05} port 80{i:02}\n"))
            .collect();
        assert_eq!(
            stub_layout(&dense, false, &RAIL_TIERS[0], FACT_HEAD_CHARS),
            Layout::Whole
        );
        assert_ne!(
            stub_layout(&dense, false, &RAIL_TIERS[2], FACT_HEAD_CHARS),
            Layout::Whole
        );
        let filler: String = (0..400)
            .map(|i| format!("line number word filler text {}\n", i % 7))
            .collect();
        match stub_layout(&filler, false, &RAIL_TIERS[0], FACT_HEAD_CHARS) {
            Layout::Stub {
                head_end,
                tail_start,
                budget,
            } => {
                assert_eq!(cslice(&filler, head_end, head_end + 1), "\n");
                assert_eq!(cslice(&filler, tail_start - 1, tail_start), "\n");
                assert!(budget >= FACT_BUDGET_CHARS);
            }
            Layout::Whole => panic!("filler should be cut"),
        }
    }

    #[test]
    fn stub_and_rerun_notes_carry_the_mark() {
        let text = "a".repeat(10_000);
        let s = render_stub(&text, 200, 9880, &["error 1".into()], true, "toolu_1");
        assert!(is_compacted(&s));
        assert!(s.contains("(error); kept its 1 fact line(s); the full output stays in this session's transcript under `toolu_1`]"));
        assert!(is_compacted(&rerun_note(&text, "toolu_1")));
        assert_eq!(rerun_note("tiny", "x"), "tiny");
        assert!(is_compacted("[fast-jev-compaction omitted 5 chars"));
        assert!(is_compacted("[jev-compaction truncated 5"));
    }
}
