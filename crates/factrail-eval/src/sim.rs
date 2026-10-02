//! Session simulation: repeated compactions of one session.
//!
//! A single compaction can look good while a session compacted ten times
//! quietly loses everything. Here messages stream into a window sized as the
//! transcript's length divided by `length` (so `length = 1.5` is a session one
//! and a half windows long); whenever the context reaches the trigger after a
//! message carrying tool results, it is compacted under the hook's pressure rule
//! (aim for 5/6 of the trigger, evict only past the trigger), and — as the hook
//! does — only once the context has grown a quarter of the trigger past what the
//! last compaction left. After each compaction every fact the agent will still
//! use must be visible.

use serde::{Deserialize, Serialize};

use factrail_core::{CompactOptions, Message, Reduction, transcript_chars};

use crate::corpus::Transcript;
use crate::facts::{Visible, facts};
use crate::metric::{Decider, EvalOptions, compact_with};

/// What a simulation runs.
#[derive(Clone, Debug)]
pub struct SimOptions {
    /// Session lengths, in windows.
    pub lengths: Vec<f64>,
    /// Trigger as a share of the window.
    pub trigger: f64,
    /// Compaction options.
    pub compact: CompactOptions,
}

impl Default for SimOptions {
    fn default() -> Self {
        Self {
            lengths: vec![0.8, 1.0, 1.2, 1.5],
            trigger: 0.6,
            compact: CompactOptions::default(),
        }
    }
}

/// One session at one length.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SimRow {
    /// Transcript name.
    pub transcript: String,
    /// Session length in windows.
    pub length: f64,
    /// Compactions run.
    pub compactions: usize,
    /// Facts at stake, summed over compactions.
    pub needed: usize,
    /// Of those, visible after each compaction.
    pub kept: usize,
    /// Highest context fill after a compaction, as a share of the window.
    pub peak_fill: f64,
}

/// Per length, pooled over transcripts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SimTotals {
    /// Session length in windows.
    pub length: f64,
    /// Compactions run.
    pub compactions: usize,
    /// Facts at stake.
    pub needed: usize,
    /// Kept.
    pub kept: usize,
    /// `kept / needed`.
    pub rate: f64,
    /// Highest fill after any compaction.
    pub peak_fill: f64,
}

/// A whole simulation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SimReport {
    /// Per session and length.
    pub rows: Vec<SimRow>,
    /// Per length.
    pub totals: Vec<SimTotals>,
}

/// Simulates every transcript at every length.
///
/// # Errors
///
/// The first compaction that fails, naming the transcript.
pub fn simulate(
    corpus: &[Transcript],
    decider: &mut Decider<'_>,
    options: &SimOptions,
) -> Result<SimReport, String> {
    let mut rows = Vec::new();
    for (name, messages) in corpus {
        let all = facts(messages);
        for &length in &options.lengths {
            rows.push(one(name, messages, &all, decider, options, length)?);
        }
    }
    let totals = options
        .lengths
        .iter()
        .map(|&length| {
            let mine: Vec<&SimRow> = rows.iter().filter(|r| r.length == length).collect();
            let needed = mine.iter().map(|r| r.needed).sum::<usize>();
            let kept = mine.iter().map(|r| r.kept).sum::<usize>();
            SimTotals {
                length,
                compactions: mine.iter().map(|r| r.compactions).sum(),
                needed,
                kept,
                rate: if needed == 0 {
                    1.0
                } else {
                    kept as f64 / needed as f64
                },
                peak_fill: mine.iter().map(|r| r.peak_fill).fold(0.0, f64::max),
            }
        })
        .collect();
    Ok(SimReport { rows, totals })
}

fn one(
    name: &str,
    messages: &[Message],
    all: &[crate::facts::Fact],
    decider: &mut Decider<'_>,
    options: &SimOptions,
    length: f64,
) -> Result<SimRow, String> {
    let window = transcript_chars(messages) as f64 / length;
    let trigger = options.trigger * window;
    let mut ctx: Vec<Message> = Vec::new();
    let mut rearm = trigger;
    let mut row = SimRow {
        transcript: name.to_owned(),
        length,
        compactions: 0,
        needed: 0,
        kept: 0,
        peak_fill: 0.0,
    };
    for (i, m) in messages.iter().enumerate() {
        ctx.push(m.clone());
        let before = transcript_chars(&ctx) as f64;
        if m.tool_results.is_empty() || before < rearm {
            continue;
        }
        let reduction = Reduction {
            target: (1.0 - trigger * 5.0 / 6.0 / before).clamp(0.05, 0.9),
            hard: (1.0 - trigger / before).clamp(0.0, 0.9),
        };
        let eval = EvalOptions {
            cuts: vec![],
            compact: options.compact.clone(),
            reduction,
        };
        let outcome = compact_with(&ctx, messages, i + 1, decider, &eval)
            .map_err(|e| format!("{name}@{i}: {e}"))?;
        ctx = outcome.messages;
        row.compactions += 1;
        rearm = trigger.max(transcript_chars(&ctx) as f64 + trigger / 4.0);
        let seen = Visible::new(&ctx);
        let at_stake: Vec<&str> = all
            .iter()
            .filter(|f| f.needed_across(i + 1))
            .map(|f| f.token.as_str())
            .collect();
        row.needed += at_stake.len();
        row.kept += at_stake.iter().filter(|t| seen.shows(t)).count();
        row.peak_fill = row.peak_fill.max(transcript_chars(&ctx) as f64 / window);
    }
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthetic::corpus;

    #[test]
    fn longer_sessions_compact_more_and_stay_bounded() {
        let c = corpus(21, 2);
        let r = simulate(&c, &mut Decider::Rules, &SimOptions::default()).unwrap();
        let short = &r.totals[0];
        let long = &r.totals[3];
        assert!(long.compactions >= short.compactions);
        assert!(long.peak_fill <= 0.75, "{long:?}");
        assert!(long.rate > 0.5, "{long:?}");
    }
}
