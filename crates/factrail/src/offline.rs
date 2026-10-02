//! Commands that work on transcripts at rest: compact a file, replay a corpus,
//! simulate sessions, export a dataset, expire saved outputs.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use factrail_backend::{Answered, DecisionLog, OutputStore, Paths, ask_all};
use factrail_core::formats::{from_claude_jsonl, from_openai_chat};
use factrail_core::{
    CallAnswer, CompactOptions, Egress, JudgeRequest, Message, Plan, Reduction, Selection,
};
use factrail_eval::corpus::{CorpusOptions, Transcript, claude_projects, json_dir};
use factrail_eval::dataset::{
    DatasetOptions, TeacherExample, hindsight_records, teacher_records, write,
};
use factrail_eval::gate::{Baseline, check};
use factrail_eval::metric::{Decider, EvalOptions, evaluate};
use factrail_eval::sim::{SimOptions, simulate};
use factrail_eval::synthetic;
use factrail_policy::{Backend, Config};

use crate::cli::{CompactArgs, DatasetArgs, EngineArgs, EvalArgs, JudgeArgs, JudgeKind, Source};
use crate::judges;

fn compact_options(engine: &EngineArgs) -> CompactOptions {
    CompactOptions {
        keep_threshold: engine.keep_threshold,
        preserve_recent: engine.preserve_recent,
        max_state_tokens: engine.max_state_tokens,
        max_request_tokens: engine.max_request_tokens.max(engine.max_state_tokens + 500),
        egress: if engine.metadata_egress {
            Egress::Metadata
        } else {
            Egress::Full
        },
        selection: Selection {
            value: !engine.regex_lines,
            pool: !engine.regex_lines && !engine.no_pool,
            contain: true,
        },
        ..CompactOptions::default()
    }
}

/// Reads a transcript in any supported format: a JSON array of hook messages,
/// `{ "messages": [...] }`, an OpenAI chat array, or a Claude Code `.jsonl`.
pub fn read_transcript(path: &Path) -> Result<Vec<Message>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if path.extension().is_some_and(|e| e == "jsonl") {
        return Ok(from_claude_jsonl(&text));
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let list = value.get("messages").cloned().unwrap_or(value);
    let items = list.as_array().ok_or("expected a JSON array of messages")?;
    let openai = items.iter().any(|m| {
        m.get("tool_calls").is_some()
            || m.get("role").and_then(Value::as_str) == Some("tool")
            || m.get("content").is_some()
    });
    if openai {
        return Ok(from_openai_chat(items).0);
    }
    serde_json::from_value(list).map_err(|e| format!("messages: {e}"))
}

fn judge_config(args: &JudgeArgs) -> Config {
    let mut options = serde_json::Map::new();
    let backend = match args.judge {
        JudgeKind::Tev => "tev",
        JudgeKind::Systemone => "systemone",
        _ => "rules",
    };
    options.insert("backend".into(), json!(backend));
    if let Some(u) = &args.base_url {
        options.insert("baseUrl".into(), json!(u));
    }
    if let Some(m) = &args.model {
        options.insert("model".into(), json!(m));
    }
    options.insert("backendLocal".into(), json!(args.local));
    Config::from_options(&options)
}

/// One answer map (question name → probability) per request.
type Answers = Vec<HashMap<String, f64>>;

/// A blocking adapter from a live judge: the full answers, redaction counts included.
fn live_answered(
    args: &JudgeArgs,
) -> Result<impl FnMut(&[JudgeRequest]) -> Result<Vec<Answered>, String>, String> {
    let config = judge_config(args);
    if config.backend == Backend::SystemOne && !judges::has_credentials(&config) {
        return Err("systemone judge: set TYPESAFE_API_KEY (or FACTRAIL_API_KEY), or --base-url with --local for a keyless façade".into());
    }
    let judge = judges::build(&config).ok_or("tev judge: --base-url is required")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let deadline = Duration::from_secs(args.deadline_secs);
    Ok(move |requests: &[JudgeRequest]| {
        runtime
            .block_on(ask_all(&judge, requests, deadline))
            .map_err(|e| e.to_string())
    })
}

/// [`live_answered`] reduced to the evaluator's callback: one answer map per request.
fn live(
    args: &JudgeArgs,
) -> Result<impl FnMut(&[JudgeRequest]) -> Result<Answers, String>, String> {
    let mut ask = live_answered(args)?;
    Ok(move |requests: &[JudgeRequest]| {
        ask(requests).map(|answered| answered.into_iter().map(|a| a.answers).collect())
    })
}

/// Remembered answers from the decision log, by `tool_use_id`.
fn recorded_answers() -> HashMap<String, CallAnswer> {
    let mut out = HashMap::new();
    let Some(paths) = Paths::from_env() else {
        return out;
    };
    for record in DecisionLog::new(&paths).records() {
        for r in record.requests {
            for (short, id) in &r.calls {
                let (Some(c), Some(res)) = (
                    r.answers.get(&format!("call_{short}")),
                    r.answers.get(&format!("result_{short}")),
                ) else {
                    continue;
                };
                out.insert(
                    id.clone(),
                    CallAnswer {
                        keep_call: *c,
                        keep_result: *res,
                    },
                );
            }
        }
    }
    out
}

fn load(source: &Source, taint: &Config) -> Result<Vec<Transcript>, String> {
    if let Some(n) = source.synthetic {
        return Ok(synthetic::corpus(source.seed, n));
    }
    if let Some(dir) = &source.json_dir {
        return json_dir(dir).map_err(|e| e.to_string());
    }
    let root = source
        .claude_projects
        .clone()
        .unwrap_or_else(default_projects);
    let options = CorpusOptions {
        min_calls: source.min_calls,
        max: source.max,
        taint_tools: taint.taint_tools.clone(),
        taint_skills: taint.taint_skills.clone(),
        seed: source.seed,
    };
    claude_projects(&root, &options).map_err(|e| format!("{}: {e}", root.display()))
}

fn default_projects() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".claude/projects")
}

/// `factrail compact`.
pub fn compact(args: &CompactArgs) -> Result<(), String> {
    let messages = read_transcript(&args.input)?;
    let plan = Plan::new(messages, compact_options(&args.engine), &HashMap::new())
        .map_err(|e| e.to_string())?;
    let reduction = Reduction {
        target: args.target,
        hard: args.hard,
    };
    let outcome = match args.judge.judge {
        JudgeKind::Rules => plan.finish_without_judge(reduction),
        JudgeKind::Systemone | JudgeKind::Tev => {
            let mut ask = live_answered(&args.judge)?;
            let answered = ask(plan.requests())?;
            let maps: Answers = answered.iter().map(|a| a.answers.clone()).collect();
            let mut out = plan.finish(&maps, reduction).map_err(|e| e.to_string())?;
            out.stats.redacted = answered.iter().map(|a| a.redacted).sum();
            out
        }
        JudgeKind::Oracle | JudgeKind::Recorded => {
            return Err("compact supports --judge rules, systemone or tev".into());
        }
    };
    let mut outcome = outcome;
    if args.save_outputs {
        let paths = Paths::from_env().ok_or("no home directory for saved outputs")?;
        let original = read_transcript(&args.input)?;
        OutputStore::new(&paths).offload(
            &args.session,
            &original,
            &mut outcome.messages,
            &mut outcome.touched,
        );
    }
    let body = json!({ "messages": outcome.messages, "stats": outcome.stats, "decisions": outcome.decisions });
    let text = serde_json::to_string_pretty(&body).map_err(|e| e.to_string())?;
    match &args.out {
        Some(path) => fs::write(path, text).map_err(|e| e.to_string())?,
        None => println!("{text}"),
    }
    eprintln!(
        "factrail: {:.1}% reduction, tier {}, {} request(s)",
        outcome.stats.reduction() * 100.0,
        outcome.stats.rail_tier,
        outcome.stats.requests
    );
    Ok(())
}

/// `factrail eval`; returns whether the gate (if any) passed.
pub fn eval(args: &EvalArgs) -> Result<bool, String> {
    let corpus = load(&args.source, &Config::default())?;
    if corpus.is_empty() {
        return Err("the corpus is empty".into());
    }
    let options = EvalOptions {
        cuts: args.cuts.clone(),
        compact: compact_options(&args.engine),
        reduction: Reduction {
            target: args.target,
            hard: args.hard,
        },
    };
    let known;
    let mut live_ask;
    let mut decider = match args.judge.judge {
        JudgeKind::Rules => Decider::Rules,
        JudgeKind::Oracle => Decider::Oracle,
        JudgeKind::Recorded => {
            known = recorded_answers();
            Decider::Known(&known)
        }
        JudgeKind::Systemone | JudgeKind::Tev => {
            live_ask = live(&args.judge)?;
            Decider::Ask(&mut live_ask)
        }
    };
    let report = evaluate(&corpus, &mut decider, &options)?;
    let t = &report.totals;
    println!(
        "factrail eval: {} transcript(s), {} cut(s), judge {:?}\n  facts kept {}/{} = {:.4}  (erase rule, same decisions: {}/{} = {:.4})\n  of {} lost from context: {} from reproducible reads (a re-run gives them back), the rest in saved outputs\n  reduction {:.4} pooled, {:.4} worst cut  (erase rule: {:.4})\n  judge requests {}",
        corpus.len(),
        t.cuts,
        args.judge.judge,
        t.kept,
        t.facts,
        t.rate,
        t.kept_erase,
        t.facts,
        t.rate_erase,
        t.facts - t.kept,
        t.lost_rereadable,
        t.reduction,
        t.reduction_min,
        t.reduction_erase,
        t.requests
    );
    let mut sim_report = None;
    if args.sim {
        let sim = simulate(
            &corpus,
            &mut decider,
            &SimOptions {
                compact: compact_options(&args.engine),
                ..SimOptions::default()
            },
        )?;
        for s in &sim.totals {
            println!(
                "  session {:.1} windows: {} compaction(s), facts kept {}/{} = {:.4}, peak fill {:.3}",
                s.length, s.compactions, s.kept, s.needed, s.rate, s.peak_fill
            );
        }
        sim_report = Some(sim);
    }
    if let Some(path) = &args.json {
        let body = json!({ "report": report, "sim": sim_report });
        fs::write(
            path,
            serde_json::to_string_pretty(&body).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    }
    if let Some(path) = &args.write_baseline {
        let baseline = Baseline {
            rate: t.rate,
            reduction: t.reduction,
            tolerance: args.tolerance,
        };
        fs::write(
            path,
            serde_json::to_string_pretty(&baseline).map_err(|e| e.to_string())? + "\n",
        )
        .map_err(|e| e.to_string())?;
        println!("  baseline written to {}", path.display());
    }
    if let Some(path) = &args.gate {
        let baseline: Baseline = serde_json::from_str(
            &fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?,
        )
        .map_err(|e| e.to_string())?;
        let verdict = check(t, &baseline);
        for line in &verdict.lines {
            println!("  {line}");
        }
        println!(
            "{}",
            if verdict.pass {
                "GATE PASS"
            } else {
                "GATE FAIL"
            }
        );
        return Ok(verdict.pass);
    }
    Ok(true)
}

/// `factrail dataset`.
pub fn dataset(args: &DatasetArgs) -> Result<(), String> {
    let corpus = load(&args.source, &Config::default())?;
    let options = DatasetOptions {
        dev_share: args.dev_share,
        compact: CompactOptions {
            max_state_tokens: args.max_state_tokens,
            max_request_tokens: args.max_state_tokens + 400,
            ..CompactOptions::default()
        },
        ..DatasetOptions::default()
    };
    let mut records = hindsight_records(&corpus, &options)?;
    if args.teacher {
        let paths = Paths::from_env().ok_or("no home directory for the decision log")?;
        let mut examples = Vec::new();
        for record in DecisionLog::new(&paths).records() {
            let transcript = factrail_eval::corpus::short_hash(record.session.as_bytes());
            for r in record.requests {
                let ids: HashMap<&str, &str> = r
                    .calls
                    .iter()
                    .map(|(s, id)| (s.as_str(), id.as_str()))
                    .collect();
                for (name, q) in &r.questions {
                    let (kind, short) = name.split_once('_').unwrap_or(("", name));
                    let (Some(p), Some(instructions), Some(id)) = (
                        r.answers.get(name),
                        q.get("instructions").and_then(Value::as_str),
                        ids.get(short),
                    ) else {
                        continue;
                    };
                    examples.push(TeacherExample {
                        transcript: transcript.clone(),
                        state: r.state.clone(),
                        instructions: instructions.to_owned(),
                        probability: *p,
                        tool_use_id: (*id).to_owned(),
                        kind: kind.to_owned(),
                    });
                }
            }
        }
        records.extend(teacher_records(&examples, 0.5, args.dev_share));
    }
    let manifest = write(&args.out, &records).map_err(|e| e.to_string())?;
    println!(
        "factrail dataset: {} record(s) from {} transcript(s) → {}",
        records.len(),
        corpus.len(),
        args.out.display()
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?
    );
    Ok(())
}

/// `factrail outputs expire`.
pub fn expire(days: u64) -> Result<(), String> {
    let paths = Paths::from_env().ok_or("no home directory")?;
    let removed = OutputStore::new(&paths)
        .expire(SystemTime::now(), Duration::from_secs(days * 24 * 3600))
        .map_err(|e| e.to_string())?;
    println!("factrail: removed {removed} saved output(s) older than {days} day(s)");
    Ok(())
}
