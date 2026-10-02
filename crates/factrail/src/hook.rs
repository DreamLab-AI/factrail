//! The Claude Code hook protocol (docs/protocol.md): one JSON request on stdin,
//! one JSON response on stdout.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use factrail_backend::{
    AnswerStore, DecisionLog, DecisionRecord, JudgeError, OutputStore, Paths, RecordedRequest,
    ask_all,
};
use factrail_core::{
    CompactOptions, Egress, Message, Outcome, Plan, Pressure, RAIL_FLOOR, Selection, pressure,
    transcript_tokens,
};
use factrail_policy::{
    Backend, CacheWarm, Config, Fallback, Judge, Scope, SwitchCommand, ToolRef, TriggerReason,
    cache_ttl_seconds, compact_scope, decide, merge_taint, nudge_delay_ms, rearm_gap,
    resolve_enabled, scan_taint, should_arm_nudge, should_compact, taint_write, trigger_tokens,
    turn_may_trigger,
};

use crate::judges;

/// The protocol version this binary speaks.
pub const PROTOCOL: u64 = 1;

/// Saved outputs older than this are deleted.
const OUTPUT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

/// Runs one hook request and returns the response. Never fails: an error becomes `ok: false`.
pub fn run(event: &str, input: &str) -> Value {
    let result = serde_json::from_str::<Value>(input)
        .map_err(|e| format!("request is not JSON: {e}"))
        .and_then(|request| {
            match request.get("protocol").and_then(Value::as_u64) {
                Some(PROTOCOL) => {}
                other => {
                    return Err(format!(
                        "protocol mismatch: this binary speaks {PROTOCOL}, the request {other:?}"
                    ));
                }
            }
            match event {
                "compact" => compact(&request),
                "turn" => Ok(turn(&request)),
                "taint" => Ok(taint(&request)),
                "status" => Ok(status(&request)),
                other => Err(format!("unknown hook event {other:?}")),
            }
        });
    match result {
        Ok(mut body) => {
            body["protocol"] = json!(PROTOCOL);
            body["ok"] = json!(true);
            body
        }
        Err(error) => json!({ "protocol": PROTOCOL, "ok": false, "error": error }),
    }
}

fn config_of(request: &Value) -> Config {
    Config::from_options(
        request
            .get("config")
            .and_then(Value::as_object)
            .unwrap_or(&Map::new()),
    )
}

fn store<'a>(request: &'a Value, key: &str) -> Option<&'a Value> {
    request
        .get("store")
        .and_then(|s| s.get(key))
        .filter(|v| !v.is_null())
}

fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|x| x.is_finite())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn tool_refs(messages: &[Message]) -> Vec<ToolRef> {
    messages
        .iter()
        .flat_map(|m| &m.tool_uses)
        .map(|t| ToolRef {
            tool: t.tool.clone(),
            skill: (t.tool == "Skill")
                .then(|| {
                    t.input
                        .get("skill")
                        .or_else(|| t.input.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .flatten(),
        })
        .collect()
}

fn request_tools(request: &Value) -> Vec<ToolRef> {
    request
        .get("tools")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

fn compact_options(config: &Config) -> CompactOptions {
    CompactOptions {
        goal: String::new(),
        keep_threshold: config.keep_threshold,
        preserve_recent: config.preserve_recent_messages,
        max_state_tokens: config.max_state_tokens,
        max_request_tokens: config.max_request_tokens,
        head_chars: config.truncate_head_chars,
        egress: match config.egress {
            factrail_policy::Egress::Full => Egress::Full,
            factrail_policy::Egress::Metadata => Egress::Metadata,
        },
        selection: Selection::default(),
    }
}

fn pct(x: f64) -> String {
    format!("{}%", (x * 100.0).round())
}

fn summary_line(o: &Outcome) -> String {
    let s = &o.stats;
    let mut parts = Vec::new();
    for (n, what) in [
        (s.kept, "kept"),
        (s.results_dropped, "results reduced"),
        (s.calls_dropped, "calls reduced"),
        (s.rules, "reduced by rules"),
        (s.pinned, "pinned"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {what}"));
        }
    }
    format!(
        "{} reduction; {}; tier {}; {} request(s){}",
        pct(s.reduction()),
        if parts.is_empty() {
            "no tool calls".to_owned()
        } else {
            parts.join(", ")
        },
        s.rail_tier,
        s.requests,
        if s.state_stage.is_empty() {
            String::new()
        } else {
            format!(", state ~{} tok ({})", s.state_tokens, s.state_stage)
        }
    )
}

/// What a compaction request resolved to, before the response is written.
struct Resolution {
    action: &'static str,
    reason: String,
    outcome: Option<Outcome>,
    log: String,
    instructions: Option<String>,
}

/// `session.compact`.
fn compact(request: &Value) -> Result<Value, String> {
    let config = config_of(request);
    let session = request.get("session").cloned().unwrap_or(Value::Null);
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let trigger = session.get("trigger").and_then(Value::as_str);
    let agent_id = session.get("agentId").and_then(Value::as_str);
    let messages: Vec<Message> =
        serde_json::from_value(request.get("messages").cloned().unwrap_or(json!([])))
            .map_err(|e| format!("messages: {e}"))?;
    let sticky = store(request, "taint");
    let scan = merge_taint(sticky, scan_taint(&tool_refs(&messages), &config));
    let taint_out = taint_write(sticky, &scan, now_ms());

    let scope = compact_scope(trigger, agent_id);
    if scope != Scope::Main {
        let (action, reason) = if scope == Scope::Precompute {
            ("skip", "precompute")
        } else {
            ("builtin", "subagent")
        };
        return Ok(
            json!({ "action": action, "reason": reason, "log": format!("factrail: {reason} compaction left to the engine"),
            "toast": false, "store": { "taint": taint_out, "last": Value::Null } }),
        );
    }

    let enabled = resolve_enabled(
        store(request, "enabled"),
        config.enabled_by_default.as_ref(),
    );
    let verdict = if config.backend == Backend::Rules && enabled {
        None
    } else {
        Some(decide(
            enabled,
            judges::has_credentials(&config),
            &scan,
            &config,
        ))
    };
    let paths = Paths::from_env();
    let resolution = match verdict.as_ref().map(|v| v.judge) {
        Some(Judge::Builtin) => {
            let v = verdict.as_ref().expect("matched");
            let detail = v
                .detail
                .as_ref()
                .map(|d| format!(": {d}"))
                .unwrap_or_default();
            let instructions = (v.reason != factrail_policy::Reason::SwitchedOff
                && config.save_full_outputs)
                .then(|| {
                    paths.as_ref().and_then(|p| {
                        save_for_summary(&OutputStore::new(p), &session_id, &messages)
                    })
                })
                .flatten();
            Resolution {
                action: "builtin",
                reason: v.reason.as_str().to_owned(),
                outcome: None,
                log: format!("factrail: built-in summary ({}{detail})", v.reason),
                instructions,
            }
        }
        judge => run_compaction(
            &config,
            &session_id,
            trigger,
            &messages,
            request,
            judge,
            verdict.as_ref(),
            &scan,
            paths.as_ref(),
        )?,
    };

    let mut body = json!({
        "action": resolution.action,
        "reason": resolution.reason,
        "log": resolution.log,
        "toast": resolution.action == "install" || resolution.reason == "tainted",
        "store": { "taint": taint_out, "last": resolution.log },
    });
    if let Some(i) = resolution.instructions {
        body["instructions"] = json!(i);
    }
    if let Some(o) = resolution.outcome {
        let list: Vec<Value> = o
            .messages
            .iter()
            .zip(&o.touched)
            .enumerate()
            .map(|(i, (m, touched))| {
                if *touched {
                    serde_json::to_value(m).expect("message serialises")
                } else {
                    json!({ "keep": i })
                }
            })
            .collect();
        body["messages"] = Value::Array(list);
    }
    Ok(body)
}

#[allow(clippy::too_many_arguments)]
fn run_compaction(
    config: &Config,
    session_id: &str,
    trigger: Option<&str>,
    messages: &[Message],
    request: &Value,
    judge_kind: Option<Judge>,
    verdict: Option<&factrail_policy::Verdict>,
    scan: &factrail_policy::TaintScan,
    paths: Option<&Paths>,
) -> Result<Resolution, String> {
    let usage = request.get("usage");
    let window = num(usage.and_then(|u| u.get("window")));
    let tokens = num(usage.and_then(|u| u.get("tokens")));
    let mut p: Pressure = pressure(
        tokens,
        trigger_tokens(window, config),
        transcript_tokens(messages),
    );
    if trigger == Some("manual") {
        p.reduction.target = p.reduction.target.max(RAIL_FLOOR);
    }
    let gate = config.min_reduction_ratio.unwrap_or(p.gate);
    let known = paths
        .map(|p| AnswerStore::new(p).load(session_id))
        .unwrap_or_default();
    let plan =
        Plan::new(messages.to_vec(), compact_options(config), &known).map_err(|e| e.to_string())?;
    let rules_plan = plan.clone();

    let use_model = judge_kind == Some(Judge::Model);
    let base_reason = match verdict {
        Some(v) if use_model => v.reason.as_str().to_owned(),
        Some(v) => format!("rules:{}", v.reason.as_str()),
        None => "rules".to_owned(),
    };
    let model_judge = if use_model {
        judges::build(config)
    } else {
        None
    };

    let (mut outcome, mut reason, judge_note) = match model_judge {
        Some(judge) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            let deadline = Duration::from_millis(config.compaction_timeout_ms);
            let asked = if plan.requests().is_empty() {
                Ok(Vec::new())
            } else {
                runtime.block_on(ask_all(&judge, plan.requests(), deadline))
            };
            match asked {
                Ok(answered) => {
                    let maps: Vec<HashMap<String, f64>> =
                        answered.iter().map(|a| a.answers.clone()).collect();
                    let redacted: usize = answered.iter().map(|a| a.redacted).sum();
                    // Recorded as sent (redacted), and never for a tainted session: these
                    // records are training data, and training may leave the machine.
                    let recorded: Vec<RecordedRequest> = if config.record_decisions && !scan.tainted
                    {
                        plan.requests()
                            .iter()
                            .zip(&maps)
                            .map(|(r, a)| RecordedRequest::new(&judge, r, a, plan.calls()))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let mut out = plan.finish(&maps, p.reduction).map_err(|e| e.to_string())?;
                    out.stats.redacted = redacted;
                    if let (Some(p), false) = (paths, recorded.is_empty()) {
                        let record = DecisionRecord {
                            ts_ms: now_ms() as u64,
                            session: session_id.to_owned(),
                            judge: judge.info(),
                            egress: config.egress.as_str().to_owned(),
                            goal: None,
                            requests: recorded,
                            decisions: out.decisions.clone(),
                            stats: out.stats.clone(),
                            outcome: base_reason.clone(),
                        };
                        let _ = DecisionLog::new(p).append(&record, SystemTime::now());
                    }
                    let note = if redacted > 0 {
                        format!("; redacted {redacted} credential-shaped value(s) from the request")
                    } else {
                        String::new()
                    };
                    (
                        out,
                        base_reason.clone(),
                        format!("{}{note}", judge.info().kind),
                    )
                }
                Err(error) if config.fallback == Fallback::Summary => {
                    return Ok(Resolution {
                        action: "builtin",
                        reason: if matches!(error, JudgeError::Deadline(_)) {
                            "deadline".into()
                        } else {
                            "error".into()
                        },
                        outcome: None,
                        log: format!("factrail: built-in summary (judge failed: {error})"),
                        instructions: None,
                    });
                }
                Err(error) => (
                    rules_plan.clone().finish_without_judge(p.reduction),
                    "ok-rules".to_owned(),
                    format!("rules after judge failure: {error}"),
                ),
            }
        }
        None => (
            plan.finish_without_judge(p.reduction),
            if use_model {
                "ok-rules".to_owned()
            } else {
                base_reason.clone()
            },
            "rules".to_owned(),
        ),
    };

    if outcome.stats.reduction() < gate && outcome.stats.requests > 0 {
        let rules = rules_plan.finish_without_judge(p.reduction);
        if rules.stats.reduction() > outcome.stats.reduction() {
            outcome = rules;
            reason = "ok-rules".to_owned();
        }
    }
    let reduction = outcome.stats.reduction();
    if reduction <= 0.0 || (reduction < gate && config.fallback == Fallback::Summary) {
        let why = if reduction <= 0.0 {
            "nothing to reduce"
        } else {
            "below the gate"
        };
        return Ok(Resolution {
            action: "builtin",
            reason: "below-gate".into(),
            outcome: None,
            log: format!(
                "factrail: built-in summary ({why}: {} vs gate {})",
                summary_line(&outcome),
                pct(gate)
            ),
            instructions: None,
        });
    }
    if let Some(p) = paths {
        if !outcome.new_answers.is_empty() {
            let _ = AnswerStore::new(p).save(session_id, &outcome.new_answers);
        }
        if config.save_full_outputs {
            let store = OutputStore::new(p);
            let _ = store.expire_if_due(SystemTime::now(), OUTPUT_MAX_AGE);
            store.offload(
                session_id,
                messages,
                &mut outcome.messages,
                &mut outcome.touched,
            );
        }
    }
    let below = if reduction < gate {
        format!(" (below the {} this window wanted)", pct(gate))
    } else {
        String::new()
    };
    let log = format!(
        "factrail: kept {}/{} messages verbatim, no summary — {} [{judge_note}]{below}",
        outcome.touched.iter().filter(|t| !**t).count(),
        outcome.messages.len(),
        summary_line(&outcome)
    );
    Ok(Resolution {
        action: "install",
        reason,
        outcome: Some(outcome),
        log,
        instructions: None,
    })
}

/// Before the built-in summary: every tool output of 200 chars or more is saved,
/// and the summariser is told where, so exact outputs stay one read away.
fn save_for_summary(store: &OutputStore, session: &str, messages: &[Message]) -> Option<String> {
    let mut saved = 0;
    for r in messages.iter().flat_map(|m| &m.tool_results) {
        if factrail_core::text::clen(&r.text) >= 200
            && store.save(session, &r.tool_use_id, &r.text).is_ok()
        {
            saved += 1;
        }
    }
    (saved > 0).then(|| {
        let dir = store.path(session, "x");
        let dir = dir.parent().map_or_else(|| store.dir().display().to_string(), |d| d.display().to_string());
        format!(
            "The full outputs of this session's tool calls are saved as files under {dir}, one per tool_use_id (<id>.txt). Keep this path in the summary so exact outputs can be read back."
        )
    })
}

/// `turn.complete`: should the plugin compact now, arm the cache-warm nudge, or wait?
fn turn(request: &Value) -> Value {
    let config = config_of(request);
    let now = num(request.get("now")).map_or_else(now_ms, |n| n as i64);
    let sticky = store(request, "taint");
    let scan = merge_taint(sticky, scan_taint(&request_tools(request), &config));
    let taint_out = taint_write(sticky, &scan, now);
    let event = request.get("event");
    let agent_id = event.and_then(|e| e.get("agentId")).and_then(Value::as_str);
    let reason = event.and_then(|e| e.get("reason")).and_then(Value::as_str);
    let quiet = |why: &str| {
        json!({ "compact": false, "reason": why, "threshold": Value::Null, "need": Value::Null,
        "baseline": Value::Null, "taint": taint_out, "nudge": Value::Null, "log": Value::Null })
    };
    if !turn_may_trigger(agent_id, reason) {
        return quiet("not-an-answer");
    }
    if !resolve_enabled(
        store(request, "enabled"),
        config.enabled_by_default.as_ref(),
    ) {
        return quiet("switched-off");
    }
    let usage = request.get("usage");
    let tokens = num(usage.and_then(|u| u.get("tokens")));
    let percent = num(usage.and_then(|u| u.get("percent")));
    let window = num(usage.and_then(|u| u.get("window")));
    let stored = request.get("baseline").filter(|b| b.is_object());
    let mut baseline = stored.and_then(|b| num(b.get("tokens")));
    let mut baseline_out = Value::Null;
    if stored
        .and_then(|b| b.get("pending"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        if let Some(t) = tokens {
            baseline = Some(t);
            baseline_out = json!({ "tokens": t, "at": now });
        }
    }
    let verdict = should_compact(tokens, percent, window, baseline, &config);
    let mut log = Value::Null;
    if verdict.reason == TriggerReason::Hysteresis {
        log = json!(format!(
            "factrail: at {} tokens, holding until {} (re-arm after the last compaction)",
            tokens.unwrap_or(0.0),
            verdict.need.unwrap_or(0.0)
        ));
    }
    let mut nudge = Value::Null;
    if !verdict.run && config.cache_warm != CacheWarm::Off {
        let gap = rearm_gap(trigger_tokens(window, &config), config.rearm_tokens);
        if should_arm_nudge(tokens, config.cache_warm_floor_tokens, baseline, gap) {
            let rate_limits = request
                .get("rateLimits")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            let has_key = request
                .get("hasApiKey")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let ttl = cache_ttl_seconds(rate_limits, has_key, &config);
            nudge = json!({
                "delayMs": nudge_delay_ms(ttl, config.cache_ttl_margin_seconds),
                "mode": if config.cache_warm == CacheWarm::Notify { "notify" } else { "compact" },
                "tokens": tokens.unwrap_or(0.0),
                "ttlSeconds": ttl,
            });
        }
    }
    json!({ "compact": verdict.run, "reason": verdict.reason.as_str(), "threshold": verdict.threshold, "need": verdict.need,
        "baseline": baseline_out, "taint": taint_out, "nudge": nudge, "log": log })
}

/// `tool.call` / `skill.prompt`: does this one call taint the session?
fn taint(request: &Value) -> Value {
    let config = config_of(request);
    let now = num(request.get("now")).map_or_else(now_ms, |n| n as i64);
    let tool = request
        .get("tool")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let skill = request
        .get("skill")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let refs = match (tool, skill) {
        (Some(tool), skill) => vec![ToolRef { tool, skill }],
        (None, Some(skill)) => vec![ToolRef {
            tool: "Skill".into(),
            skill: Some(skill),
        }],
        (None, None) => vec![],
    };
    let scan = scan_taint(&refs, &config);
    json!({ "taint": taint_write(store(request, "taint"), &scan, now) })
}

/// `/factrail on|off|status`.
fn status(request: &Value) -> Value {
    let config = config_of(request);
    let args = request.get("args").and_then(Value::as_str).unwrap_or("");
    let command = SwitchCommand::parse(args);
    let enabled_write = match command {
        SwitchCommand::On => Some(true),
        SwitchCommand::Off => Some(false),
        _ => None,
    };
    let stored_enabled = enabled_write
        .map(Value::Bool)
        .or_else(|| store(request, "enabled").cloned());
    let enabled = resolve_enabled(stored_enabled.as_ref(), config.enabled_by_default.as_ref());
    let sticky = store(request, "taint");
    let scan = merge_taint(sticky, scan_taint(&request_tools(request), &config));
    let credentials = judges::has_credentials(&config);
    let verdict = decide(enabled, credentials, &scan, &config);
    let usage = request.get("usage");
    let window = num(usage.and_then(|u| u.get("window")));
    let tokens = num(usage.and_then(|u| u.get("tokens")));
    let threshold = trigger_tokens(window, &config);
    let k = |x: f64| format!("{}k", (x / 1000.0).round());
    let judge_line = match (config.backend, verdict.judge) {
        (_, Judge::Builtin) => format!(
            "this session would use the built-in summary ({})",
            verdict.reason
        ),
        (Backend::Rules, _) => "this session would be compacted by the rules (no model)".to_owned(),
        (_, Judge::Rules) => format!(
            "this session would be compacted by the rules, no model ({}{})",
            verdict.reason,
            verdict
                .detail
                .as_ref()
                .map(|d| format!(": {d}"))
                .unwrap_or_default()
        ),
        (_, Judge::Model) => format!(
            "this session would be judged by {} ({})",
            config.backend.as_str(),
            verdict.reason
        ),
    };
    let endpoint = match config.backend {
        Backend::Rules => "none (rules)".to_owned(),
        _ => config
            .base_url
            .clone()
            .unwrap_or_else(|| "vendor cloud (default)".to_owned()),
    };
    let paths = Paths::from_env();
    let mut lines = vec![
        format!(
            "factrail {}: {}{} · backend {} · keep ≥ {} · fallback {}",
            env!("CARGO_PKG_VERSION"),
            if enabled { "ON" } else { "OFF" },
            if enabled_write.is_some() {
                " (saved)"
            } else {
                ""
            },
            config.backend.as_str(),
            config.keep_threshold,
            config.fallback.as_str()
        ),
        format!(
            "trigger at {} tokens (min of {}% of the window and {}) · context now {}",
            threshold.map_or("?".to_owned(), k),
            config.compact_at_percent,
            k(config.compact_at_tokens),
            tokens.map_or("?".to_owned(), k)
        ),
        judge_line,
        format!(
            "endpoint: {endpoint} · declared {}",
            if config.backend_local {
                "LOCAL: tainted sessions may be judged"
            } else {
                "NON-LOCAL: tainted sessions get the rules only"
            }
        ),
        format!(
            "egress: {} · taint rule: tools {} · skills {} · sticky per session",
            config.egress.as_str(),
            config.taint_tools.join(", "),
            config.taint_skills.join(", ")
        ),
        match &paths {
            Some(p) => format!(
                "saved outputs: {} · decision log: {}",
                if config.save_full_outputs {
                    p.cache.join("outputs").display().to_string()
                } else {
                    "off".into()
                },
                if config.record_decisions {
                    p.data.join("decisions").display().to_string()
                } else {
                    "off".into()
                }
            ),
            None => "saved outputs and decision log: no home directory found".to_owned(),
        },
    ];
    if scan.tainted {
        lines.push(format!(
            "session tainted: {} call(s): {}",
            scan.count,
            scan.sample.join(", ")
        ));
    }
    if let Some(last) = store(request, "last").and_then(Value::as_str) {
        lines.push(format!("last outcome: {last}"));
    }
    if command == SwitchCommand::Help {
        lines.push("usage: /factrail on | off | status".to_owned());
    }
    json!({ "text": lines.join("\n"), "store": { "enabled": enabled_write } })
}
