//! The decision log: every judged request, its answers and what the compaction
//! did with them, appended as JSON lines for evaluation and training.
//!
//! One file per UTC day, `decisions/<YYYY-MM-DD>.jsonl` under the data
//! directory, 0600 in a 0700 folder on unix. The caller never writes a record
//! for a tainted session. A record holds the state *as sent* — already
//! redacted, and carrying only tool-result sizes, never their contents.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use factrail_core::{CallDecision, JudgeRequest, Stats, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::judge::{Judge, JudgeInfo};
use crate::store::{Paths, private_append};

/// One request as it went to the judge, and what came back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedRequest {
    /// The state as sent (redacted).
    pub state: Value,
    /// The questions as sent (redacted), name → `{type, instructions}`.
    pub questions: Map<String, Value>,
    /// Question name → probability the judge gave.
    pub answers: HashMap<String, f64>,
    /// `(short id, tool_use_id)` of every call the questions ask about, in question order.
    pub calls: Vec<(String, String)>,
}

impl RecordedRequest {
    /// The record of `request` as `judge` sent it, with its `answers`; `calls`
    /// (the plan's [`calls`](factrail_core::Plan::calls)) maps the short ids in
    /// question names (`call_<id>`, `result_<id>`) to `tool_use_id`s.
    pub fn new(
        judge: &Judge,
        request: &JudgeRequest,
        answers: &HashMap<String, f64>,
        calls: &[ToolCall],
    ) -> Self {
        let sent = judge.outgoing(request);
        let mut ids: Vec<(String, String)> = Vec::new();
        for (name, _) in &request.questions {
            let short = name
                .strip_prefix("call_")
                .or_else(|| name.strip_prefix("result_"))
                .unwrap_or(name);
            if ids.iter().any(|(s, _)| s == short) {
                continue;
            }
            if let Some(call) = calls.iter().find(|c| c.id == short) {
                ids.push((call.id.clone(), call.tool_use_id.clone()));
            }
        }
        Self {
            state: sent.state,
            questions: sent.questions,
            answers: answers.clone(),
            calls: ids,
        }
    }
}

/// One compaction's line in the decision log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    /// When it happened, ms since the Unix epoch.
    pub ts_ms: u64,
    /// The session id.
    pub session: String,
    /// The judge asked.
    pub judge: JudgeInfo,
    /// What of tool arguments was sent (`full` or `metadata`).
    pub egress: String,
    /// The goal line, when one was set.
    pub goal: Option<String>,
    /// The requests sent and their answers.
    pub requests: Vec<RecordedRequest>,
    /// One decision per call.
    pub decisions: Vec<CallDecision>,
    /// What the compaction did.
    pub stats: Stats,
    /// The outcome's log reason (`ok`, `deadline`, …).
    pub outcome: String,
}

/// The decision log under `paths.data/decisions`.
#[derive(Clone, Debug)]
pub struct DecisionLog {
    dir: PathBuf,
}

impl DecisionLog {
    /// The log under `paths.data/decisions`. Nothing is created until an append.
    pub fn new(paths: &Paths) -> Self {
        Self {
            dir: paths.data.join("decisions"),
        }
    }

    /// The `decisions` folder.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The day file `now` falls in: `<YYYY-MM-DD>.jsonl`, UTC (a time before
    /// the epoch counts as 1970-01-01).
    ///
    /// ```
    /// use std::time::{Duration, UNIX_EPOCH};
    /// use factrail_backend::DecisionLog;
    /// assert_eq!(DecisionLog::file_name(UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29.jsonl");
    /// ```
    pub fn file_name(now: SystemTime) -> String {
        let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX);
        let (y, m, d) = civil_date(days);
        format!("{y:04}-{m:02}-{d:02}.jsonl")
    }

    /// Appends `record` as one line to the day file of `now`; returns its path.
    ///
    /// # Errors
    ///
    /// Any I/O error creating the folder or appending to the file.
    pub fn append(&self, record: &DecisionRecord, now: SystemTime) -> io::Result<PathBuf> {
        let mut line = serde_json::to_string(record).map_err(io::Error::other)?;
        line.push('\n');
        private_append(&self.dir, &Self::file_name(now), line.as_bytes())
    }

    /// Every record of this log; see [`DecisionLog::read_all`].
    pub fn records(&self) -> impl Iterator<Item = DecisionRecord> {
        Self::read_all(&self.dir)
    }

    /// Every record in the `*.jsonl` files of `dir`, files in name (date)
    /// order, lines in file order. Unreadable files and lines that are not a
    /// record are skipped; a missing folder yields nothing.
    pub fn read_all(dir: &Path) -> impl Iterator<Item = DecisionRecord> {
        let mut files: Vec<PathBuf> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl") && p.is_file())
            .collect();
        files.sort();
        files
            .into_iter()
            .filter_map(|p| fs::File::open(p).ok())
            .flat_map(|f| {
                BufReader::new(f)
                    .lines()
                    .map_while(Result::ok)
                    .filter_map(|line| serde_json::from_str(&line).ok())
            })
    }
}

/// The proleptic Gregorian `(year, month, day)` of `days` since 1970-01-01
/// (Howard Hinnant's `civil_from_days`).
///
/// ```
/// use factrail_backend::civil_date;
/// assert_eq!(civil_date(0), (1970, 1, 1));
/// assert_eq!(civil_date(19_782), (2024, 2, 29));
/// assert_eq!(civil_date(-1), (1969, 12, 31));
/// ```
pub fn civil_date(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March first
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::judge::SystemOneJudge;
    use factrail_core::Question;

    /// Days since the epoch the slow way, to check [`civil_date`] against.
    fn naive_days(year: i64, month: u32, day: u32) -> i64 {
        let leap = |y: i64| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let mut days = 0;
        for y in 1970..year {
            days += if leap(y) { 366 } else { 365 };
        }
        let lengths = [
            31,
            if leap(year) { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        days + lengths[..month as usize - 1].iter().sum::<i64>() + i64::from(day) - 1
    }

    #[test]
    fn civil_date_matches_a_naive_count() {
        // Every day from 1970 to 2134, against a year-by-year count.
        for d in 0..60_000 {
            let (y, m, dd) = civil_date(d);
            assert!((1..=12).contains(&m) && (1..=31).contains(&dd));
            assert_eq!(naive_days(y, m, dd), d, "{y}-{m}-{dd}");
        }
        assert_eq!(civil_date(2_932_896), (9999, 12, 31));
        assert_eq!(civil_date(11_016), (2000, 2, 29));
        assert_eq!(civil_date(-719_468), (0, 3, 1));
    }

    fn record(ts_ms: u64) -> DecisionRecord {
        DecisionRecord {
            ts_ms,
            session: "s".into(),
            judge: JudgeInfo {
                kind: "tev".into(),
                endpoint: "http://x/v1/chat/completions".into(),
                model: "tev1".into(),
            },
            egress: "full".into(),
            goal: None,
            requests: vec![],
            decisions: vec![],
            stats: Stats::default(),
            outcome: "ok".into(),
        }
    }

    #[test]
    fn append_names_files_by_utc_day_and_reads_back() {
        let root =
            std::env::temp_dir().join(format!("factrail-backend-log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let log = DecisionLog::new(&Paths::under(&root));
        assert_eq!(log.records().count(), 0);
        let late = UNIX_EPOCH + Duration::from_secs(1_709_251_199); // 2024-02-29 23:59:59
        let next = late + Duration::from_secs(1);
        let p1 = log.append(&record(1), late).unwrap();
        log.append(&record(2), late).unwrap();
        let p2 = log.append(&record(3), next).unwrap();
        assert_eq!(p1.file_name().unwrap(), "2024-02-29.jsonl");
        assert_eq!(p2.file_name().unwrap(), "2024-03-01.jsonl");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&p1).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(log.dir()).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        fs::write(
            log.dir().join("2024-03-02.jsonl"),
            "garbage\n{\"ts_ms\": 1}\n",
        )
        .unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&p2)
            .and_then(|mut f| io::Write::write_all(&mut f, b"not json\n"))
            .unwrap();
        let ts: Vec<u64> = DecisionLog::read_all(log.dir()).map(|r| r.ts_ms).collect();
        assert_eq!(ts, [1, 2, 3]);
        assert_eq!(
            DecisionLog::file_name(UNIX_EPOCH - Duration::from_secs(5)),
            "1970-01-01.jsonl"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recorded_request_is_redacted_and_maps_calls() {
        let judge = Judge::SystemOne(SystemOneJudge::new(
            None,
            Some("key-0123456789".into()),
            None,
            Duration::from_secs(1),
        ));
        let q = |s: &str| Question {
            kind: "noul".into(),
            instructions: s.into(),
        };
        let request = JudgeRequest {
            state: serde_json::json!({ "goal": "use key-0123456789 and ghp_abcdefghijklmnopqrstuvwxyz0123" }),
            questions: vec![("call_t2".into(), q("a")), ("result_t2".into(), q("b"))],
            state_tokens: 10,
        };
        let calls = vec![ToolCall {
            id: "t2".into(),
            tool_use_id: "toolu_9".into(),
            tool: "Bash".into(),
            input: Map::new(),
            call_index: 1,
            result_index: 2,
            result_chars: 5,
            is_error: false,
            pinned: false,
        }];
        let answers = HashMap::from([("call_t2".to_owned(), 0.8), ("result_t2".to_owned(), 0.1)]);
        let rec = RecordedRequest::new(&judge, &request, &answers, &calls);
        assert_eq!(
            rec.state["goal"],
            "use [REDACTED:known] and [REDACTED:gh-token]"
        );
        assert_eq!(rec.calls, [("t2".to_owned(), "toolu_9".to_owned())]);
        assert_eq!(rec.questions["result_t2"]["instructions"], "b");
        let line = serde_json::to_string(&rec).unwrap();
        assert_eq!(serde_json::from_str::<RecordedRequest>(&line).unwrap(), rec);
    }
}
