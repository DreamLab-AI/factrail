//! The replay metric: compact a transcript at a cut, then count the facts the
//! agent goes on to use that the compacted context still shows.
//!
//! Every cut is also compacted by the upstream rule this project replaces
//! (a dropped call erased with its result, a dropped result cut to a 300-char
//! head) under the *same* decisions, so the difference isolates what the rails
//! buy rather than what the judge decided.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use factrail_core::{
    Action, CallAnswer, CallDecision, CompactOptions, JudgeRequest, Message, Plan, Reduction,
    transcript_chars,
};

use crate::corpus::Transcript;
use crate::facts::{Visible, facts, labels};

/// A live judge: one answer map (question name → probability) per request, in order.
pub type AskFn<'a> = dyn FnMut(&[JudgeRequest]) -> Result<Vec<HashMap<String, f64>>, String> + 'a;

/// Who decides what to keep.
pub enum Decider<'a> {
    /// No judge: every unpinned result reduced by the rails.
    Rules,
    /// Hindsight labels as the judge's answers: an upper bound on any judge.
    Oracle,
    /// Remembered answers by `tool_use_id`; calls without one are kept.
    Known(&'a HashMap<String, CallAnswer>),
    /// A live judge behind a callback: one answer map per request, in order.
    Ask(&'a mut AskFn<'a>),
}

/// What a replay runs.
#[derive(Clone, Debug)]
pub struct EvalOptions {
    /// Where to cut, as fractions of the transcript's messages.
    pub cuts: Vec<f64>,
    /// Compaction options.
    pub compact: CompactOptions,
    /// How far each compaction goes.
    pub reduction: Reduction,
}

impl Default for EvalOptions {
    fn default() -> Self {
        Self {
            cuts: vec![0.5, 0.75],
            compact: CompactOptions::default(),
            reduction: Reduction {
                target: 0.3,
                hard: 0.3,
            },
        }
    }
}

/// One transcript at one cut.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CutReport {
    /// Transcript name.
    pub transcript: String,
    /// Cut index (messages before it are compacted).
    pub cut: usize,
    /// Facts learnt before the cut and used after it.
    pub facts: usize,
    /// Of those, still visible after factrail's compaction.
    pub kept: usize,
    /// Of those, still visible after the upstream erase rule with the same decisions.
    pub kept_erase: usize,
    /// Facts factrail did not keep in context that came from a reproducible read
    /// (a re-run gives them back); the rest are in the saved output of their result.
    pub lost_rereadable: usize,
    /// Chars before.
    pub chars_before: usize,
    /// Chars after factrail.
    pub chars_after: usize,
    /// Chars after the erase rule.
    pub chars_after_erase: usize,
    /// Judge requests made.
    pub requests: usize,
    /// Strictest rail tier used.
    pub tier: usize,
}

/// Pooled results.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    /// Cuts evaluated.
    pub cuts: usize,
    /// Facts at stake.
    pub facts: usize,
    /// Kept by factrail.
    pub kept: usize,
    /// Kept by the erase rule.
    pub kept_erase: usize,
    /// Lost from context but from a reproducible read.
    pub lost_rereadable: usize,
    /// `kept / facts`.
    pub rate: f64,
    /// `kept_erase / facts`.
    pub rate_erase: f64,
    /// Pooled char reduction by factrail.
    pub reduction: f64,
    /// Smallest per-cut reduction by factrail.
    pub reduction_min: f64,
    /// Pooled char reduction by the erase rule.
    pub reduction_erase: f64,
    /// Judge requests made in all.
    pub requests: usize,
}

/// A whole replay.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// Per cut.
    pub rows: Vec<CutReport>,
    /// Pooled.
    pub totals: Totals,
}

/// Upstream's rule: a dropped call vanishes with its result; a dropped result
/// keeps a `head`-char head and a truncation note.
pub fn erase(messages: &[Message], decisions: &[CallDecision], head: usize) -> Vec<Message> {
    let action: HashMap<&str, Action> = decisions
        .iter()
        .map(|d| (d.tool_use_id.as_str(), d.action))
        .collect();
    let cut = |text: &str| {
        let len = factrail_core::text::clen(text);
        if len <= head + 120 {
            return text.to_owned();
        }
        format!(
            "{}\n[fast-jev-compaction truncated {} chars of this tool result; re-run the tool if needed]",
            factrail_core::text::head(text, head),
            len - head
        )
    };
    messages
        .iter()
        .filter_map(|m| {
            let mut m = m.clone();
            m.tool_uses
                .retain(|t| action.get(t.tool_use_id.as_str()) != Some(&Action::DropCall));
            m.tool_results
                .retain(|r| action.get(r.tool_use_id.as_str()) != Some(&Action::DropCall));
            for r in &mut m.tool_results {
                if action.get(r.tool_use_id.as_str()) == Some(&Action::DropResult) {
                    r.text = cut(&r.text);
                }
            }
            (!(m.text.trim().is_empty() && m.tool_uses.is_empty() && m.tool_results.is_empty()))
                .then_some(m)
        })
        .collect()
}

/// Answers for every question of `requests`, from per-call probabilities.
pub fn answers_from(
    plan: &Plan,
    by_call: &dyn Fn(&str) -> CallAnswer,
) -> Vec<HashMap<String, f64>> {
    let ids: HashMap<&str, &str> = plan
        .calls()
        .iter()
        .map(|c| (c.id.as_str(), c.tool_use_id.as_str()))
        .collect();
    plan.requests()
        .iter()
        .map(|r| {
            r.questions
                .iter()
                .map(|(name, _)| {
                    let (kind, short) = name.split_once('_').unwrap_or(("", name));
                    let a = by_call(ids.get(short).copied().unwrap_or(""));
                    (
                        name.clone(),
                        if kind == "call" {
                            a.keep_call
                        } else {
                            a.keep_result
                        },
                    )
                })
                .collect()
        })
        .collect()
}

/// Compacts `prefix` with `decider`. `full` and `cut` give the oracle its hindsight.
///
/// # Errors
///
/// A plan that cannot be built or a judge that fails, as text.
pub fn compact_with(
    prefix: &[Message],
    full: &[Message],
    cut: usize,
    decider: &mut Decider<'_>,
    options: &EvalOptions,
) -> Result<factrail_core::Outcome, String> {
    let empty = HashMap::new();
    let known = match decider {
        Decider::Known(k) => *k,
        _ => &empty,
    };
    let plan =
        Plan::new(prefix.to_vec(), options.compact.clone(), known).map_err(|e| e.to_string())?;
    let answers = match decider {
        Decider::Rules => return Ok(plan.finish_without_judge(options.reduction)),
        Decider::Oracle => {
            let l = labels(full, cut);
            answers_from(&plan, &|id| {
                let x = l.get(id).copied().unwrap_or(crate::facts::Label {
                    keep_result: true,
                    keep_call: true,
                });
                CallAnswer {
                    keep_call: f64::from(u8::from(x.keep_call)),
                    keep_result: f64::from(u8::from(x.keep_result)),
                }
            })
        }
        Decider::Known(_) => answers_from(&plan, &|_| CallAnswer {
            keep_call: 1.0,
            keep_result: 1.0,
        }),
        Decider::Ask(ask) => ask(plan.requests())?,
    };
    plan.finish(&answers, options.reduction)
        .map_err(|e| e.to_string())
}

/// Replays every transcript at every cut.
///
/// # Errors
///
/// The first compaction that fails, as text naming the transcript.
pub fn evaluate(
    corpus: &[Transcript],
    decider: &mut Decider<'_>,
    options: &EvalOptions,
) -> Result<Report, String> {
    let mut rows = Vec::new();
    for (name, messages) in corpus {
        let all = facts(messages);
        for &f in &options.cuts {
            let cut = ((messages.len() as f64) * f).round() as usize;
            if cut < 2 || cut >= messages.len() {
                continue;
            }
            let prefix = &messages[..cut];
            let at_stake: Vec<&crate::facts::Fact> =
                all.iter().filter(|x| x.needed_across(cut)).collect();
            let rereadable: std::collections::HashSet<&str> = prefix
                .iter()
                .flat_map(|m| &m.tool_uses)
                .filter(|t| factrail_core::rails::reproducible(&t.tool, &t.input))
                .map(|t| t.tool_use_id.as_str())
                .collect();
            let outcome = compact_with(prefix, messages, cut, decider, options)
                .map_err(|e| format!("{name}@{cut}: {e}"))?;
            let erased = erase(prefix, &outcome.decisions, 300);
            let seen = Visible::new(&outcome.messages);
            let seen_erase = Visible::new(&erased);
            rows.push(CutReport {
                transcript: name.clone(),
                cut,
                facts: at_stake.len(),
                kept: at_stake.iter().filter(|f| seen.shows(&f.token)).count(),
                kept_erase: at_stake
                    .iter()
                    .filter(|f| seen_erase.shows(&f.token))
                    .count(),
                lost_rereadable: at_stake
                    .iter()
                    .filter(|f| !seen.shows(&f.token) && rereadable.contains(f.source.as_str()))
                    .count(),
                chars_before: transcript_chars(prefix),
                chars_after: transcript_chars(&outcome.messages),
                chars_after_erase: transcript_chars(&erased),
                requests: outcome.stats.requests,
                tier: outcome.stats.rail_tier,
            });
        }
    }
    Ok(Report {
        totals: totals(&rows),
        rows,
    })
}

fn ratio(a: usize, b: usize) -> f64 {
    if b == 0 { 1.0 } else { a as f64 / b as f64 }
}

/// Pools per-cut rows.
pub fn totals(rows: &[CutReport]) -> Totals {
    let sum = |f: fn(&CutReport) -> usize| rows.iter().map(f).sum::<usize>();
    let before = sum(|r| r.chars_before);
    let reduction = |after: usize| {
        if before == 0 {
            0.0
        } else {
            1.0 - after as f64 / before as f64
        }
    };
    Totals {
        cuts: rows.len(),
        facts: sum(|r| r.facts),
        kept: sum(|r| r.kept),
        kept_erase: sum(|r| r.kept_erase),
        lost_rereadable: sum(|r| r.lost_rereadable),
        rate: ratio(sum(|r| r.kept), sum(|r| r.facts)),
        rate_erase: ratio(sum(|r| r.kept_erase), sum(|r| r.facts)),
        reduction: reduction(sum(|r| r.chars_after)),
        reduction_min: rows
            .iter()
            .map(|r| {
                if r.chars_before == 0 {
                    0.0
                } else {
                    1.0 - r.chars_after as f64 / r.chars_before as f64
                }
            })
            .fold(f64::INFINITY, f64::min)
            .min(1.0),
        reduction_erase: reduction(sum(|r| r.chars_after_erase)),
        requests: sum(|r| r.requests),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthetic::corpus;

    #[test]
    fn rails_keep_more_than_erase_under_the_same_decisions() {
        let c = corpus(11, 4);
        let report = evaluate(&c, &mut Decider::Rules, &EvalOptions::default()).unwrap();
        let t = &report.totals;
        assert!(t.facts > 0);
        assert!(t.rate > t.rate_erase, "{t:?}");
        assert!(t.reduction >= 0.25, "{t:?}");
    }

    #[test]
    fn oracle_keeps_at_least_as_much_as_rules() {
        let c = corpus(3, 3);
        let rules = evaluate(&c, &mut Decider::Rules, &EvalOptions::default())
            .unwrap()
            .totals;
        let oracle = evaluate(&c, &mut Decider::Oracle, &EvalOptions::default())
            .unwrap()
            .totals;
        assert!(oracle.rate >= rules.rate, "{oracle:?} vs {rules:?}");
    }

    #[test]
    fn ask_callback_is_used() {
        let c = corpus(5, 1);
        let mut calls = 0;
        let mut ask = |reqs: &[JudgeRequest]| {
            calls += reqs.len();
            Ok(reqs
                .iter()
                .map(|r| r.questions.iter().map(|(n, _)| (n.clone(), 0.0)).collect())
                .collect())
        };
        let report = evaluate(&c, &mut Decider::Ask(&mut ask), &EvalOptions::default()).unwrap();
        assert!(report.totals.requests > 0);
        assert!(calls > 0);
    }
}
