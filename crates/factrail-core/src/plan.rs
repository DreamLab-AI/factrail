//! A compaction in two halves: [`Plan::new`] builds the judge's requests,
//! [`Plan::finish`] applies the answers. Between them the caller asks whichever
//! judge it trusts, over whatever transport it has — this crate does no I/O.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::apply::{Action, Reduction, Selection, apply};
use crate::calls::{ToolCall, collect_tool_calls};
use crate::model::{Message, transcript_chars};
use crate::rails::{FACT_HEAD_CHARS, is_compacted};
use crate::state::{Egress, FitOptions, state_groups};
use crate::tokens::estimate_tokens;

/// Tokens the request envelope (`model`, key names) adds around state and questions.
const REQUEST_OVERHEAD_TOKENS: usize = 20;

/// How a compaction is planned and applied.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactOptions {
    /// The ongoing task; empty takes the last three user prompts.
    pub goal: String,
    /// Minimum keep probability for a call or result to stay. Default 0.5.
    pub keep_threshold: f64,
    /// Newest messages never touched (the first is always kept). Default 6.
    pub preserve_recent: usize,
    /// Estimated token ceiling for the state. Default 25 000.
    pub max_state_tokens: usize,
    /// Estimated token ceiling for state plus one batch of questions. Default 30 000.
    pub max_request_tokens: usize,
    /// Head a fact stub keeps. Default 200.
    pub head_chars: usize,
    /// What of tool arguments may be sent to the judge.
    pub egress: Egress,
    /// How fact lines are chosen.
    pub selection: Selection,
}

impl Default for CompactOptions {
    fn default() -> Self {
        Self {
            goal: String::new(),
            keep_threshold: 0.5,
            preserve_recent: 6,
            max_state_tokens: 25_000,
            max_request_tokens: 30_000,
            head_chars: FACT_HEAD_CHARS,
            egress: Egress::Full,
            selection: Selection::default(),
        }
    }
}

/// The judge's two probabilities about one call.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallAnswer {
    /// That the call itself still matters.
    #[serde(rename = "keepCall")]
    pub keep_call: f64,
    /// That its full result still needs to stay verbatim.
    #[serde(rename = "keepResult")]
    pub keep_result: f64,
}

/// Why a call got its action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionReason {
    /// In the pinned first or newest messages.
    Pinned,
    /// The judge kept its result.
    Kept,
    /// The judge kept the call but not its result.
    ResultDropped,
    /// The judge kept neither.
    CallDropped,
    /// An earlier compaction already reduced it; final.
    Reduced,
    /// No judge was asked (rules only, or no window could fit it).
    Rules,
}

/// The decision about one call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallDecision {
    /// Short id (`t1`, …).
    pub id: String,
    /// The call's session-unique id.
    pub tool_use_id: String,
    /// The tool.
    pub tool: String,
    /// The judge's answer, when it was asked (or remembered).
    pub answer: Option<CallAnswer>,
    /// What happens to it.
    pub action: Action,
    /// Why.
    pub reason: DecisionReason,
}

/// One `noul` question: a probability that a statement holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Question {
    /// Always `"noul"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The statement the probability is about.
    pub instructions: String,
}

impl Question {
    fn noul(instructions: String) -> Self {
        Self {
            kind: "noul".into(),
            instructions,
        }
    }
}

/// The two questions asked about one call: keep the call, keep its result.
pub fn questions_for(call: &ToolCall) -> [(String, Question); 2] {
    [
        (
            format!("call_{}", call.id),
            Question::noul(format!(
                "Tool call {} ({}) should stay in the history: knowing this call was made, with its input, still matters for what the assistant does next",
                call.id, call.tool
            )),
        ),
        (
            format!("result_{}", call.id),
            Question::noul(format!(
                "The full output of tool call {} ({}, {} chars) should stay in the history verbatim: the assistant still needs its contents and re-running the tool would not do",
                call.id, call.tool, call.result_chars
            )),
        ),
    ]
}

/// One request to the judge: a state and the questions asked against it.
#[derive(Clone, Debug, PartialEq)]
pub struct JudgeRequest {
    /// `{ context, goal, history }`.
    pub state: Value,
    /// Question name → question, in call order.
    pub questions: Vec<(String, Question)>,
    /// Estimated tokens of the state.
    pub state_tokens: usize,
}

impl JudgeRequest {
    /// The questions as the JSON object the System One wire format carries.
    pub fn questions_json(&self) -> Map<String, Value> {
        self.questions
            .iter()
            .map(|(name, q)| {
                (
                    name.clone(),
                    serde_json::to_value(q).expect("question serialises"),
                )
            })
            .collect()
    }
}

/// Planning failed before any request.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// The state leaves no room for even one call's questions.
    #[error("state leaves no room for questions (~{state_tokens} of {max_request_tokens} tokens)")]
    NoRoom {
        /// Estimated state tokens.
        state_tokens: usize,
        /// The request ceiling.
        max_request_tokens: usize,
    },
}

/// An answer set did not answer what was asked.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid or missing answer for {0}")]
pub struct AnswerError(pub String);

/// What a compaction did, for the log line and the decision record.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    /// Messages in (and out: messages are never removed).
    pub messages: usize,
    /// Chars before.
    pub chars_before: usize,
    /// Chars after.
    pub chars_after: usize,
    /// Tool calls with a result.
    pub calls: usize,
    /// Kept verbatim by the judge.
    pub kept: usize,
    /// Result reduced, call kept.
    pub results_dropped: usize,
    /// Result reduced, arguments cut.
    pub calls_dropped: usize,
    /// Pinned.
    pub pinned: usize,
    /// Reduced without a judge.
    pub rules: usize,
    /// Already reduced by an earlier compaction.
    pub reduced: usize,
    /// Largest state sent, in estimated tokens.
    pub state_tokens: usize,
    /// How the state was fitted.
    pub state_stage: String,
    /// Requests made.
    pub requests: usize,
    /// Strictest rail tier used.
    pub rail_tier: usize,
    /// Credential-shaped values masked from the judge requests before they were
    /// sent. Core never sends anything, so it leaves this 0; the caller that asked
    /// the judge fills it in. Older records without it read as 0.
    #[serde(default)]
    pub redacted: usize,
}

impl Stats {
    /// `(before − after) / before`, 0 for an empty transcript; negative when the
    /// compaction made the transcript longer, so no gate ever mistakes growth for gain.
    pub fn reduction(&self) -> f64 {
        if self.chars_before == 0 {
            0.0
        } else {
            (self.chars_before as f64 - self.chars_after as f64) / self.chars_before as f64
        }
    }
}

/// The compacted transcript and how it came about.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// One message per input message, in order.
    pub messages: Vec<Message>,
    /// For each message, whether it changed.
    pub touched: Vec<bool>,
    /// One decision per call.
    pub decisions: Vec<CallDecision>,
    /// Counts.
    pub stats: Stats,
    /// Answers newly received, by `tool_use_id`, to remember for the next compaction.
    pub new_answers: HashMap<String, CallAnswer>,
}

/// A planned compaction.
#[derive(Clone, Debug)]
pub struct Plan {
    messages: Vec<Message>,
    options: CompactOptions,
    calls: Vec<ToolCall>,
    reduced: HashSet<String>,
    known: HashMap<String, CallAnswer>,
    requests: Vec<JudgeRequest>,
    floor: Vec<usize>,
    stage: String,
}

impl Plan {
    /// Plans a compaction of `messages`. Calls whose result an earlier compaction
    /// reduced, and calls `known` already has an answer for, are not asked again
    /// (re-asking gave the same action on 711 of 711 calls upstream).
    ///
    /// # Errors
    ///
    /// [`PlanError::NoRoom`] when the fitted state leaves no room for questions.
    pub fn new(
        messages: Vec<Message>,
        options: CompactOptions,
        known: &HashMap<String, CallAnswer>,
    ) -> Result<Self, PlanError> {
        let calls = collect_tool_calls(&messages, options.preserve_recent);
        let reduced: HashSet<String> = messages
            .iter()
            .flat_map(|m| {
                m.tool_results
                    .iter()
                    .filter(|r| is_compacted(&r.text))
                    .map(|r| r.tool_use_id.clone())
                    .chain(
                        m.tool_uses
                            .iter()
                            .filter(|t| t.text.as_deref().is_some_and(is_compacted))
                            .map(|t| t.tool_use_id.clone()),
                    )
                    .collect::<Vec<_>>()
            })
            .collect();
        let known: HashMap<String, CallAnswer> = calls
            .iter()
            .filter(|c| !c.pinned)
            .filter_map(|c| {
                known
                    .get(&c.tool_use_id)
                    .map(|a| (c.tool_use_id.clone(), *a))
            })
            .collect();
        let candidates: Vec<usize> = calls
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                !c.pinned
                    && !reduced.contains(&c.tool_use_id)
                    && !known.contains_key(&c.tool_use_id)
            })
            .map(|(k, _)| k)
            .collect();
        let mut plan = Self {
            messages,
            options,
            calls,
            reduced,
            known,
            requests: Vec::new(),
            floor: Vec::new(),
            stage: String::new(),
        };
        if candidates.is_empty() {
            return Ok(plan);
        }
        let fit = FitOptions {
            max_state_tokens: plan.options.max_state_tokens,
            preserve_recent: plan.options.preserve_recent,
            goal: plan.options.goal.clone(),
            egress: plan.options.egress,
        };
        let grouping = state_groups(&plan.messages, &plan.calls, &candidates, &fit);
        for group in grouping.groups {
            let budget = plan
                .options
                .max_request_tokens
                .saturating_sub(group.state.tokens + REQUEST_OVERHEAD_TOKENS);
            let mut batch: Vec<(String, Question)> = Vec::new();
            let mut used = 0;
            for &k in &group.calls {
                let qs = questions_for(&plan.calls[k]);
                let json: Map<String, Value> = qs
                    .iter()
                    .map(|(n, q)| {
                        (
                            n.clone(),
                            serde_json::to_value(q).expect("question serialises"),
                        )
                    })
                    .collect();
                let tokens = estimate_tokens(&Value::Object(json).to_string());
                if !batch.is_empty() && used + tokens > budget {
                    plan.requests.push(JudgeRequest {
                        state: group.state.state.clone(),
                        questions: std::mem::take(&mut batch),
                        state_tokens: group.state.tokens,
                    });
                    used = 0;
                }
                if batch.is_empty() && tokens > budget {
                    return Err(PlanError::NoRoom {
                        state_tokens: group.state.tokens,
                        max_request_tokens: plan.options.max_request_tokens,
                    });
                }
                batch.extend(qs);
                used += tokens;
            }
            if !batch.is_empty() {
                plan.requests.push(JudgeRequest {
                    state: group.state.state,
                    questions: batch,
                    state_tokens: group.state.tokens,
                });
            }
        }
        plan.floor = grouping.floor;
        plan.stage = grouping.stage;
        Ok(plan)
    }

    /// The requests to send, in order. Empty when nothing needs asking.
    pub fn requests(&self) -> &[JudgeRequest] {
        &self.requests
    }

    /// The tool calls of the transcript.
    pub fn calls(&self) -> &[ToolCall] {
        &self.calls
    }

    /// The transcript being compacted.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Applies the judge's answers: one map of question name → `noul` probability
    /// per request, in [`Plan::requests`] order.
    ///
    /// # Errors
    ///
    /// [`AnswerError`] naming the first question with no finite answer.
    pub fn finish(
        self,
        answers: &[HashMap<String, f64>],
        reduction: Reduction,
    ) -> Result<Outcome, AnswerError> {
        let mut by_name: HashMap<&str, f64> = HashMap::new();
        for map in answers {
            for (k, v) in map {
                by_name.insert(k, *v);
            }
        }
        let mut fresh = HashMap::new();
        for request in &self.requests {
            for pair in request.questions.chunks(2) {
                let [(call_q, _), (result_q, _)] = pair else {
                    continue;
                };
                let get = |n: &str| {
                    by_name
                        .get(n)
                        .copied()
                        .filter(|v| v.is_finite())
                        .ok_or_else(|| AnswerError(n.to_owned()))
                };
                let answer = CallAnswer {
                    keep_call: get(call_q)?,
                    keep_result: get(result_q)?,
                };
                let short = call_q.trim_start_matches("call_");
                if let Some(call) = self.calls.iter().find(|c| c.id == short) {
                    fresh.insert(call.tool_use_id.clone(), answer);
                }
            }
        }
        Ok(self.conclude(&fresh, false, reduction))
    }

    /// Applies the rails with no judge: every unpinned call's result is reduced
    /// and its call kept (the deterministic path for a session no model may see,
    /// or when the judge is unreachable). Nothing is erased and nothing is
    /// summarised, so no fact is paraphrased away.
    pub fn finish_without_judge(self, reduction: Reduction) -> Outcome {
        self.conclude(&HashMap::new(), true, reduction)
    }

    fn conclude(
        self,
        fresh: &HashMap<String, CallAnswer>,
        rules: bool,
        reduction: Reduction,
    ) -> Outcome {
        let floor: HashSet<&str> = self
            .floor
            .iter()
            .map(|&k| self.calls[k].tool_use_id.as_str())
            .collect();
        let threshold = self.options.keep_threshold;
        let decisions: Vec<CallDecision> = self
            .calls
            .iter()
            .map(|c| {
                let answer = fresh
                    .get(&c.tool_use_id)
                    .or_else(|| self.known.get(&c.tool_use_id))
                    .copied();
                let (action, reason) = if c.pinned {
                    (Action::Keep, DecisionReason::Pinned)
                } else if self.reduced.contains(&c.tool_use_id) {
                    (Action::DropResult, DecisionReason::Reduced)
                } else if let (false, Some(a)) = (rules, answer) {
                    if a.keep_result >= threshold {
                        (Action::Keep, DecisionReason::Kept)
                    } else if a.keep_call >= threshold {
                        (Action::DropResult, DecisionReason::ResultDropped)
                    } else {
                        (Action::DropCall, DecisionReason::CallDropped)
                    }
                } else if rules || floor.contains(c.tool_use_id.as_str()) {
                    (Action::DropResult, DecisionReason::Rules)
                } else {
                    (Action::Keep, DecisionReason::Kept)
                };
                CallDecision {
                    id: c.id.clone(),
                    tool_use_id: c.tool_use_id.clone(),
                    tool: c.tool.clone(),
                    answer,
                    action,
                    reason,
                }
            })
            .collect();
        let actions: HashMap<&str, Action> = decisions
            .iter()
            .map(|d| (d.tool_use_id.as_str(), d.action))
            .collect();
        let chars_before = transcript_chars(&self.messages);
        let applied = apply(
            &self.messages,
            &self.calls,
            &actions,
            self.options.head_chars,
            self.options.selection,
            reduction,
        );
        let count = |r: DecisionReason| decisions.iter().filter(|d| d.reason == r).count();
        let stats = Stats {
            redacted: 0,
            messages: self.messages.len(),
            chars_before,
            chars_after: transcript_chars(&applied.messages),
            calls: self.calls.len(),
            kept: count(DecisionReason::Kept),
            results_dropped: count(DecisionReason::ResultDropped),
            calls_dropped: count(DecisionReason::CallDropped),
            pinned: count(DecisionReason::Pinned),
            rules: count(DecisionReason::Rules),
            reduced: count(DecisionReason::Reduced),
            state_tokens: self
                .requests
                .iter()
                .map(|r| r.state_tokens)
                .max()
                .unwrap_or(0),
            state_stage: self.stage.clone(),
            requests: if rules { 0 } else { self.requests.len() },
            rail_tier: applied.tier,
        };
        Outcome {
            messages: applied.messages,
            touched: applied.touched,
            decisions,
            stats,
            new_answers: fresh.clone(),
        }
    }
}

/// Estimated tokens of a transcript's texts, tool inputs and tool results.
pub fn transcript_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| {
            estimate_tokens(&m.text)
                + m.tool_uses
                    .iter()
                    .map(|t| estimate_tokens(&crate::model::input_json(&t.input)))
                    .sum::<usize>()
                + m.tool_results
                    .iter()
                    .map(|r| estimate_tokens(&r.text))
                    .sum::<usize>()
        })
        .sum()
}

/// The default reduction when the window's fill is unknown.
pub const RAIL_FLOOR: f64 = 0.3;

/// How far a compaction must go, and when it is good enough.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pressure {
    /// What the rails aim for and where eviction starts.
    pub reduction: Reduction,
    /// The reduction below which the result is not worth installing.
    pub gate: f64,
}

/// What this compaction must free. With figures: enough to bring the context
/// back to 5/6 of the trigger (the rails' target), and the oldest results give
/// way only to get under the trigger itself (the hard line, also the gate). The
/// part of the context outside the transcript (system prompt, tools) does not
/// shrink: it is the context's token count less the transcript's own estimate.
/// Without figures: [`RAIL_FLOOR`], evicting to it, gated at 0.25.
///
/// ```
/// use factrail_core::pressure;
/// let p = pressure(Some(200_000.0), Some(180_000.0), 190_000);
/// assert!((p.reduction.target - (1.0 - (150_000.0 - 10_000.0) / 190_000.0)).abs() < 1e-9);
/// assert!(p.gate < p.reduction.target);
/// ```
pub fn pressure(
    context_tokens: Option<f64>,
    trigger_tokens: Option<f64>,
    transcript_tokens: usize,
) -> Pressure {
    match (context_tokens, trigger_tokens) {
        (Some(tokens), Some(trigger)) if tokens > 0.0 && trigger > 0.0 && transcript_tokens > 0 => {
            let est = transcript_tokens as f64;
            let overhead = (tokens - est).max(0.0);
            let need = |line: f64| 1.0 - (line - overhead) / est;
            let hard = need(trigger).clamp(0.0, 0.9);
            Pressure {
                reduction: Reduction {
                    target: need(trigger * 5.0 / 6.0).clamp(0.05, 0.9),
                    hard,
                },
                gate: hard,
            }
        }
        _ => Pressure {
            reduction: Reduction {
                target: RAIL_FLOOR,
                hard: RAIL_FLOOR,
            },
            gate: 0.25,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Role, ToolResult, ToolUse};

    fn session(n: usize) -> Vec<Message> {
        let mut ms = vec![Message::text(Role::User, "ship the release")];
        for i in 0..n {
            let mut input = Map::new();
            input.insert(
                "command".into(),
                Value::String(format!("curl -s http://svc/{i}")),
            );
            ms.push(Message {
                role: Role::Assistant,
                text: format!("step {i}"),
                tool_uses: vec![ToolUse {
                    tool_use_id: format!("u{i}"),
                    tool: "Bash".into(),
                    input,
                    text: None,
                    is_error: false,
                }],
                tool_results: vec![],
            });
            let body: String = (0..400)
                .map(|k| format!("line {k} filler words here\n"))
                .collect();
            ms.push(Message {
                role: Role::User,
                text: String::new(),
                tool_uses: vec![],
                tool_results: vec![ToolResult {
                    tool_use_id: format!("u{i}"),
                    text: format!("{body}error: id {i:08}\n{body}"),
                    is_error: false,
                }],
            });
        }
        ms
    }

    fn answer_all(plan: &Plan, call: f64, result: f64) -> Vec<HashMap<String, f64>> {
        plan.requests()
            .iter()
            .map(|r| {
                r.questions
                    .iter()
                    .map(|(n, _)| {
                        (
                            n.clone(),
                            if n.starts_with("call_") { call } else { result },
                        )
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn plans_two_questions_per_unpinned_call() {
        let plan = Plan::new(session(5), CompactOptions::default(), &HashMap::new()).unwrap();
        let asked: usize = plan.requests().iter().map(|r| r.questions.len()).sum();
        let unpinned = plan.calls().iter().filter(|c| !c.pinned).count();
        assert_eq!(asked, 2 * unpinned);
        assert_eq!(plan.requests()[0].questions[0].0, "call_t1");
        assert_eq!(
            plan.requests()[0].questions_json()["result_t1"]["type"],
            "noul"
        );
    }

    #[test]
    fn small_request_budget_splits_batches() {
        let opts = CompactOptions {
            max_request_tokens: 2_000,
            ..CompactOptions::default()
        };
        let plan = Plan::new(session(12), opts, &HashMap::new());
        match plan {
            Ok(p) => assert!(p.requests().len() > 1),
            Err(PlanError::NoRoom { .. }) => {}
        }
    }

    #[test]
    fn keep_answers_keep_and_drop_answers_reduce() {
        let ms = session(5);
        let before = transcript_chars(&ms);
        let plan = Plan::new(ms.clone(), CompactOptions::default(), &HashMap::new()).unwrap();
        let keep = plan
            .clone()
            .finish(
                &answer_all(&plan, 0.9, 0.9),
                Reduction {
                    target: 0.3,
                    hard: 0.0,
                },
            )
            .unwrap();
        assert_eq!(keep.messages, ms);
        let drop = plan
            .clone()
            .finish(
                &answer_all(&plan, 0.9, 0.1),
                Reduction {
                    target: 0.3,
                    hard: 0.0,
                },
            )
            .unwrap();
        assert!(drop.stats.chars_after < before);
        assert!(drop.stats.results_dropped > 0);
        assert_eq!(
            drop.new_answers.len(),
            plan.calls().iter().filter(|c| !c.pinned).count()
        );
        assert!(
            drop.messages[2].tool_results[0]
                .text
                .contains("error: id 00000000")
        );
    }

    #[test]
    fn dropping_a_call_never_lengthens_the_transcript() {
        // Regression: a 204-char argument just over the 200-char brief cap used to
        // grow when cut (suffix longer than the cut), so `reduction` underflowed.
        let mut input = Map::new();
        input.insert(
            "command".into(),
            Value::String(format!("curl -s http://svc/{}", "a".repeat(185))),
        );
        let ms = vec![
            Message::text(Role::User, "go"),
            Message {
                role: Role::Assistant,
                text: String::new(),
                tool_uses: vec![ToolUse {
                    tool_use_id: "u1".into(),
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
                    tool_use_id: "u1".into(),
                    text: "ok".into(),
                    is_error: false,
                }],
            },
            Message::text(Role::Assistant, "done"),
        ];
        let opts = CompactOptions {
            preserve_recent: 1,
            ..CompactOptions::default()
        };
        let plan = Plan::new(ms, opts, &HashMap::new()).unwrap();
        let out = plan
            .clone()
            .finish(
                &answer_all(&plan, 0.0, 0.0),
                Reduction {
                    target: 0.3,
                    hard: 0.0,
                },
            )
            .unwrap();
        assert!(
            out.stats.chars_after <= out.stats.chars_before,
            "{:?}",
            out.stats
        );
        assert!(out.stats.reduction() >= 0.0);
    }

    #[test]
    fn reduction_is_negative_not_a_panic_when_output_grows() {
        let stats = Stats {
            chars_before: 100,
            chars_after: 110,
            ..Stats::default()
        };
        assert!((stats.reduction() + 0.1).abs() < 1e-9);
    }

    #[test]
    fn missing_answer_is_an_error() {
        let plan = Plan::new(session(6), CompactOptions::default(), &HashMap::new()).unwrap();
        assert!(!plan.requests().is_empty());
        assert!(
            plan.finish(
                &[],
                Reduction {
                    target: 0.3,
                    hard: 0.0
                }
            )
            .is_err()
        );
    }

    #[test]
    fn rules_reduce_without_requests_and_known_answers_are_not_reasked() {
        let ms = session(5);
        let plan = Plan::new(ms.clone(), CompactOptions::default(), &HashMap::new()).unwrap();
        let rules = plan.clone().finish_without_judge(Reduction {
            target: 0.3,
            hard: 0.0,
        });
        assert_eq!(rules.stats.requests, 0);
        assert!(rules.stats.rules > 0);
        assert!(rules.stats.reduction() > 0.3);
        let out = plan
            .clone()
            .finish(
                &answer_all(&plan, 0.9, 0.1),
                Reduction {
                    target: 0.3,
                    hard: 0.0,
                },
            )
            .unwrap();
        let again = Plan::new(ms, CompactOptions::default(), &out.new_answers).unwrap();
        assert!(again.requests().is_empty());
        let reuse = again
            .finish(
                &[],
                Reduction {
                    target: 0.3,
                    hard: 0.0,
                },
            )
            .unwrap();
        assert_eq!(reuse.messages, out.messages);
    }

    #[test]
    fn recompaction_skips_reduced_calls() {
        let ms = session(5);
        let plan = Plan::new(ms, CompactOptions::default(), &HashMap::new()).unwrap();
        let out = plan.finish_without_judge(Reduction {
            target: 0.3,
            hard: 0.0,
        });
        let again = Plan::new(out.messages, CompactOptions::default(), &HashMap::new()).unwrap();
        assert!(again.requests().is_empty());
    }

    #[test]
    fn pressure_without_figures_uses_the_floor() {
        let p = pressure(None, Some(1.0), 10);
        assert_eq!(p.reduction.target, RAIL_FLOOR);
        assert_eq!(p.gate, 0.25);
    }
}
