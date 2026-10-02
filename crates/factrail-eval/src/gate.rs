//! The regression gate a nightly job runs.

use serde::{Deserialize, Serialize};

use crate::metric::Totals;

/// The bar a change must clear, committed next to the code it guards.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    /// Fact rate the last accepted version reached.
    pub rate: f64,
    /// Pooled reduction it reached.
    pub reduction: f64,
    /// Slack allowed on both before a change fails.
    pub tolerance: f64,
}

/// The gate's answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    /// Whether every check held.
    pub pass: bool,
    /// One line per check, `PASS …` or `FAIL …`.
    pub lines: Vec<String>,
}

/// Checks a replay against `baseline`: the fact rate and the reduction may not
/// fall more than the tolerance, and the rails must keep at least as many facts
/// as the erase rule under the same decisions.
///
/// ```
/// use factrail_eval::gate::{Baseline, check};
/// use factrail_eval::metric::Totals;
/// let t = Totals { rate: 0.95, rate_erase: 0.4, reduction: 0.5, ..Totals::default() };
/// let b = Baseline { rate: 0.96, reduction: 0.45, tolerance: 0.02 };
/// assert!(check(&t, &b).pass);
/// assert!(!check(&Totals { rate: 0.9, ..t }, &b).pass);
/// ```
pub fn check(totals: &Totals, baseline: &Baseline) -> Verdict {
    let mut lines = Vec::new();
    let mut pass = true;
    let mut test = |ok: bool, line: String| {
        pass &= ok;
        lines.push(format!("{} {line}", if ok { "PASS" } else { "FAIL" }));
    };
    test(
        totals.rate >= baseline.rate - baseline.tolerance,
        format!(
            "fact rate {:.4} vs baseline {:.4} (tolerance {:.3})",
            totals.rate, baseline.rate, baseline.tolerance
        ),
    );
    test(
        totals.reduction >= baseline.reduction - baseline.tolerance,
        format!(
            "reduction {:.4} vs baseline {:.4} (tolerance {:.3})",
            totals.reduction, baseline.reduction, baseline.tolerance
        ),
    );
    test(
        totals.rate >= totals.rate_erase,
        format!(
            "rails {:.4} ≥ erase rule {:.4} under the same decisions",
            totals.rate, totals.rate_erase
        ),
    );
    Verdict { pass, lines }
}
