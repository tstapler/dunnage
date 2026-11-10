//! Threshold logic for allocation health. Pure so it can be unit-tested exhaustively.

use serde::Serialize;

pub const GIB: u64 = 1 << 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    pub warn_unallocated: u64,
    pub crit_unallocated: u64,
    pub warn_metadata_headroom: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            warn_unallocated: 10 * GIB,
            crit_unallocated: 3 * GIB,
            warn_metadata_headroom: 2 * GIB,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Ok,
    Warn,
    Crit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub severity: Verdict,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Assessment {
    pub verdict: Verdict,
    pub findings: Vec<Finding>,
}

/// All comparisons are strict (`<`): exactly at a threshold is still healthier.
pub fn assess(unallocated: u64, metadata_headroom: u64, t: &Thresholds) -> Assessment {
    let mut findings = Vec::new();
    if unallocated < t.crit_unallocated {
        findings.push(Finding {
            severity: Verdict::Crit,
            message: format!(
                "unallocated {unallocated} B is below the critical floor {} B; no new chunks can be created",
                t.crit_unallocated
            ),
        });
    } else if unallocated < t.warn_unallocated {
        findings.push(Finding {
            severity: Verdict::Warn,
            message: format!(
                "unallocated {unallocated} B is below the warning floor {} B",
                t.warn_unallocated
            ),
        });
    }
    if metadata_headroom < t.warn_metadata_headroom {
        findings.push(Finding {
            severity: Verdict::Warn,
            message: format!(
                "metadata headroom {metadata_headroom} B is below {} B",
                t.warn_metadata_headroom
            ),
        });
    }
    let verdict = findings
        .iter()
        .map(|f| f.severity)
        .max()
        .unwrap_or(Verdict::Ok);
    Assessment { verdict, findings }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Thresholds = Thresholds {
        warn_unallocated: 10 * GIB,
        crit_unallocated: 3 * GIB,
        warn_metadata_headroom: 2 * GIB,
    };

    fn verdict(unalloc: u64, headroom: u64) -> Verdict {
        assess(unalloc, headroom, &T).verdict
    }

    #[test]
    fn healthy_is_ok() {
        let a = assess(50 * GIB, 5 * GIB, &T);
        assert_eq!(a.verdict, Verdict::Ok);
        assert!(a.findings.is_empty());
    }

    #[test]
    fn unallocated_boundaries() {
        assert_eq!(verdict(10 * GIB, 5 * GIB), Verdict::Ok);
        assert_eq!(verdict(10 * GIB - 1, 5 * GIB), Verdict::Warn);
        assert_eq!(verdict(3 * GIB, 5 * GIB), Verdict::Warn);
        assert_eq!(verdict(3 * GIB - 1, 5 * GIB), Verdict::Crit);
        assert_eq!(verdict(0, 5 * GIB), Verdict::Crit);
    }

    #[test]
    fn metadata_headroom_boundaries() {
        assert_eq!(verdict(50 * GIB, 2 * GIB), Verdict::Ok);
        assert_eq!(verdict(50 * GIB, 2 * GIB - 1), Verdict::Warn);
    }

    #[test]
    fn worst_finding_wins_and_all_are_reported() {
        let a = assess(0, 0, &T);
        assert_eq!(a.verdict, Verdict::Crit);
        assert_eq!(a.findings.len(), 2);
    }

    #[test]
    fn this_machines_froze_state_is_critical() {
        // Values from the 2026-11-01 incident fixture.
        assert_eq!(verdict(1_081_856, 546_193_408), Verdict::Crit);
    }

    #[test]
    fn verdicts_order_by_severity() {
        assert!(Verdict::Ok < Verdict::Warn && Verdict::Warn < Verdict::Crit);
    }
}
