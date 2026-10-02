//! Training data in the Tev1 record format.
//!
//! Two sources of labels:
//!
//! * **Hindsight** (the default): a call's result is labelled "keep" when the
//!   agent later used a fact it introduced, or re-ran the call; the call itself
//!   when the agent later wrote one of its argument tokens ([`crate::facts::labels`]).
//!   These labels come from what the agent did, not from any model's answers,
//!   so they can be produced from every session, every night.
//! * **Teacher**: a judge's recorded probabilities, thresholded. Whether a
//!   vendor's outputs may train another model is a question for that vendor's
//!   terms; this source is opt-in for that reason.
//!
//! Each record is rendered through [`factrail_core::tev`], the same module the
//! Tev judge asks with, so training and inference see identical text. Splits are
//! by transcript, so no session straddles train and dev. Rendering the prompt
//! through a model's chat template (and appending its EOS token) is the
//! tokenizer's job and is left to the training toolchain (`tev1`).

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use factrail_core::{CompactOptions, Plan, tev};

use crate::corpus::{Transcript, short_hash};
use crate::facts::labels;

/// Which split a record belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Split {
    /// Training.
    Train,
    /// Validation.
    Dev,
}

impl Split {
    fn name(self) -> &'static str {
        match self {
            Self::Train => "train",
            Self::Dev => "dev",
        }
    }
}

/// Where a record came from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source {
    /// Transcript name (a hash for real sessions).
    pub transcript: String,
    /// Cut index, for hindsight labels.
    pub cut: Option<usize>,
    /// The call asked about.
    pub tool_use_id: String,
    /// `call` or `result`.
    pub kind: String,
    /// `hindsight` or `teacher`.
    pub label: String,
    /// The teacher's probability, for teacher labels.
    pub probability: Option<f64>,
}

/// One supervised decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// Stable id.
    pub id: String,
    /// The judge state, serialised.
    pub state: String,
    /// The question.
    pub question: String,
    /// The options ([`tev::options`]).
    pub options: Value,
    /// The correct letter.
    pub answer: String,
    /// The correct option's key.
    pub answer_key: String,
    /// Provenance.
    pub source: Source,
}

fn split_of(transcript: &str, dev_share: f64) -> Split {
    let byte = u8::from_str_radix(&short_hash(transcript.as_bytes())[..2], 16).unwrap_or(0);
    if f64::from(byte) < dev_share * 256.0 {
        Split::Dev
    } else {
        Split::Train
    }
}

fn record(state: &Value, instructions: &str, truth: bool, source: Source) -> Record {
    let id = short_hash(
        format!(
            "{}|{:?}|{}|{}|{}",
            source.transcript, source.cut, source.tool_use_id, source.kind, source.label
        )
        .as_bytes(),
    );
    Record {
        id,
        state: state.to_string(),
        question: tev::question(instructions),
        options: tev::options(),
        answer: if truth {
            tev::TRUE_LABEL
        } else {
            tev::FALSE_LABEL
        }
        .to_owned(),
        answer_key: if truth { "true" } else { "false" }.to_owned(),
        source,
    }
}

/// What the hindsight export runs.
#[derive(Clone, Debug)]
pub struct DatasetOptions {
    /// Cuts, as fractions of each transcript.
    pub cuts: Vec<f64>,
    /// Compaction options; keep `max_state_tokens` within the model's sequence limit.
    pub compact: CompactOptions,
    /// Share of transcripts sent to dev.
    pub dev_share: f64,
}

impl Default for DatasetOptions {
    fn default() -> Self {
        Self {
            cuts: vec![0.5, 0.75],
            compact: CompactOptions {
                max_state_tokens: 1_500,
                max_request_tokens: 1_900,
                ..CompactOptions::default()
            },
            dev_share: 0.1,
        }
    }
}

/// Hindsight-labelled records: every question a compaction at each cut would ask.
///
/// # Errors
///
/// A cut whose plan cannot be built, naming it.
pub fn hindsight_records(
    corpus: &[Transcript],
    options: &DatasetOptions,
) -> Result<Vec<(Split, Record)>, String> {
    let mut out = Vec::new();
    for (name, messages) in corpus {
        let split = split_of(name, options.dev_share);
        for &f in &options.cuts {
            let cut = ((messages.len() as f64) * f).round() as usize;
            if cut < 2 || cut >= messages.len() {
                continue;
            }
            let plan = Plan::new(
                messages[..cut].to_vec(),
                options.compact.clone(),
                &Default::default(),
            )
            .map_err(|e| format!("{name}@{cut}: {e}"))?;
            let l = labels(messages, cut);
            let ids: BTreeMap<&str, &str> = plan
                .calls()
                .iter()
                .map(|c| (c.id.as_str(), c.tool_use_id.as_str()))
                .collect();
            for request in plan.requests() {
                for (qname, q) in &request.questions {
                    let (kind, short) = qname.split_once('_').unwrap_or(("", qname));
                    let Some(&id) = ids.get(short) else { continue };
                    let Some(label) = l.get(id) else { continue };
                    let truth = if kind == "call" {
                        label.keep_call
                    } else {
                        label.keep_result
                    };
                    let source = Source {
                        transcript: name.clone(),
                        cut: Some(cut),
                        tool_use_id: id.to_owned(),
                        kind: kind.to_owned(),
                        label: "hindsight".into(),
                        probability: None,
                    };
                    out.push((
                        split,
                        record(&request.state, &q.instructions, truth, source),
                    ));
                }
            }
        }
    }
    Ok(out)
}

/// One recorded judge answer.
#[derive(Clone, Debug, PartialEq)]
pub struct TeacherExample {
    /// The session it came from (hashed).
    pub transcript: String,
    /// The state as it was sent.
    pub state: Value,
    /// The question's statement.
    pub instructions: String,
    /// The judge's probability that it holds.
    pub probability: f64,
    /// The call asked about.
    pub tool_use_id: String,
    /// `call` or `result`.
    pub kind: String,
}

/// Teacher-labelled records: `probability >= threshold` is "true".
pub fn teacher_records(
    examples: &[TeacherExample],
    threshold: f64,
    dev_share: f64,
) -> Vec<(Split, Record)> {
    examples
        .iter()
        .map(|e| {
            let source = Source {
                transcript: e.transcript.clone(),
                cut: None,
                tool_use_id: e.tool_use_id.clone(),
                kind: e.kind.clone(),
                label: "teacher".into(),
                probability: Some(e.probability),
            };
            (
                split_of(&e.transcript, dev_share),
                record(
                    &e.state,
                    &e.instructions,
                    e.probability >= threshold,
                    source,
                ),
            )
        })
        .collect()
}

/// The chat-message form of a record (system, user, assistant).
pub fn sft(record: &Record) -> Value {
    let state: Value =
        serde_json::from_str(&record.state).unwrap_or(Value::String(record.state.clone()));
    let instructions = record
        .question
        .strip_suffix(" Is this statement true?")
        .unwrap_or(&record.question);
    serde_json::json!({ "messages": [
        { "role": "system", "content": tev::SYSTEM },
        { "role": "user", "content": tev::decision(&state, instructions) },
        { "role": "assistant", "content": record.answer },
    ]})
}

/// Counts and digests of a written dataset.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Records per split and answer key (`train/true`, …).
    pub counts: BTreeMap<String, usize>,
    /// SHA-256 (first six bytes, hex) of every file written.
    pub files: BTreeMap<String, String>,
    /// Distinct transcripts per split.
    pub transcripts: BTreeMap<String, usize>,
}

fn private_file(path: &Path) -> io::Result<fs::File> {
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// Writes `records/{train,dev}.jsonl`, `sft/{train,dev}.jsonl` and
/// `manifest.json` under `dir`, which must not already exist (as tev1's builders,
/// a dataset is never silently overwritten). Files are created 0600: they hold
/// session text.
///
/// # Errors
///
/// `dir` exists, or a write fails.
pub fn write(dir: &Path, records: &[(Split, Record)]) -> io::Result<Manifest> {
    if dir.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists; refusing to overwrite a dataset", dir.display()),
        ));
    }
    fs::create_dir_all(dir.join("records"))?;
    fs::create_dir_all(dir.join("sft"))?;
    let mut manifest = Manifest::default();
    for split in [Split::Train, Split::Dev] {
        let mine: Vec<&Record> = records
            .iter()
            .filter(|(s, _)| *s == split)
            .map(|(_, r)| r)
            .collect();
        let mut rec = String::new();
        let mut chat = String::new();
        for r in &mine {
            rec.push_str(&serde_json::to_string(r).map_err(io::Error::other)?);
            rec.push('\n');
            chat.push_str(&sft(r).to_string());
            chat.push('\n');
            *manifest
                .counts
                .entry(format!("{}/{}", split.name(), r.answer_key))
                .or_default() += 1;
        }
        let transcripts: std::collections::BTreeSet<&str> =
            mine.iter().map(|r| r.source.transcript.as_str()).collect();
        manifest
            .transcripts
            .insert(split.name().into(), transcripts.len());
        for (sub, body) in [("records", rec), ("sft", chat)] {
            let rel = format!("{sub}/{}.jsonl", split.name());
            private_file(&dir.join(&rel))?.write_all(body.as_bytes())?;
            manifest.files.insert(rel, short_hash(body.as_bytes()));
        }
    }
    let json = serde_json::to_string_pretty(&manifest).map_err(io::Error::other)?;
    private_file(&dir.join("manifest.json"))?.write_all(json.as_bytes())?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthetic::corpus;

    #[test]
    fn hindsight_export_is_split_by_transcript_and_renders_like_the_judge() {
        let c = corpus(40, 12);
        let records = hindsight_records(
            &c,
            &DatasetOptions {
                dev_share: 0.3,
                ..DatasetOptions::default()
            },
        )
        .unwrap();
        assert!(!records.is_empty());
        assert!(records.iter().any(|(_, r)| r.answer == "A"));
        assert!(records.iter().any(|(_, r)| r.answer == "B"));
        for t in c.iter().map(|x| &x.0) {
            let splits: std::collections::HashSet<Split> = records
                .iter()
                .filter(|(_, r)| &r.source.transcript == t)
                .map(|(s, _)| *s)
                .collect();
            assert!(splits.len() <= 1, "{t} straddles splits");
        }
        let (_, r) = &records[0];
        let chat = sft(r);
        let state: Value = serde_json::from_str(&r.state).unwrap();
        let instr = r.question.strip_suffix(" Is this statement true?").unwrap();
        assert_eq!(
            chat["messages"][1]["content"],
            Value::String(tev::decision(&state, instr))
        );
        assert!(
            factrail_core::estimate_tokens(&chat["messages"][1]["content"].to_string()) < 2_300
        );
    }

    #[test]
    fn write_refuses_existing_and_records_counts() {
        let dir = std::env::temp_dir().join(format!("factrail-ds-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let ex = TeacherExample {
            transcript: "s".into(),
            state: serde_json::json!({"goal": "g"}),
            instructions: "Tool call t1 (Bash) should stay".into(),
            probability: 0.8,
            tool_use_id: "u".into(),
            kind: "call".into(),
        };
        let recs = teacher_records(&[ex], 0.5, 0.0);
        let m = write(&dir, &recs).unwrap();
        assert_eq!(m.counts["train/true"], 1);
        assert!(write(&dir, &recs).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join("records/train.jsonl"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
