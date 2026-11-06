//! Cadence, seek and warm-up comparison helpers (spec 14.3).
//!
//! Two jobs, both narrow, and both here because the alternative is the same logic written
//! separately in `anim_sim/tests/determinism.rs` and in `tests/wp05_isolation.rs` — two
//! comparators that can disagree about what "the same" means.
//!
//! 1. **Read a headless report** ([`Report::parse`]), so a test that spawns a process is
//!    comparing structured values rather than diffing text it hopes has a fixed shape, and so a
//!    run can say how many checkpoint entries it ended with (`checkpoints_stored`).
//! 2. **Compare two runs that cached differently** ([`assert_same_run`]), with the failure
//!    message naming the tick and both hashes, which is the difference between a gate that
//!    reports a defect and one that reports a red line. The identity fields are checked first,
//!    so a drifted fixture is reported as that rather than as nondeterminism.
//!
//! ## The assertion that makes the rest of it worth running
//!
//! A cadence comparison between two runs that cached identically proves nothing, and a
//! configuration list that happens to collapse to the same cache size is exactly how that
//! happens by accident. [`assert_caching_differed`] is therefore called by the cadence
//! comparison in `tests/wp05_isolation.rs`, on the reasoning DECISIONS.md #31.13 applied to the
//! kill-point suite: an assertion about the *residue* of a scenario is what proves the scenario
//! was the scenario. The child reports its own residue as `checkpoints_stored`, so the claim
//! travels across the process boundary instead of being asserted about an in-process cache the
//! comparison never looked at.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "these are comparison helpers for a scenario, not a production path: a fixture \
              that will not load or a step that will not run leaves nothing to compare, and \
              a panic naming the call is the diagnosis. The workspace denial still governs \
              every production crate, and this is not one -- the same exemption \
              `wp04_fixture.rs` carries."
)]

/// One parsed headless report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Every `key value` line in the order the report wrote them.
    pub header: Vec<(String, String)>,
    /// The per-tick hashes, in tick order. Empty when the report was written brief.
    pub ticks: Vec<(u64, String)>,
    /// The aggregate digest over the tick hashes.
    pub aggregate: String,
    /// How many hash lines the report claims.
    pub count: u64,
    /// The final tick's hash.
    pub final_hash: String,
    /// How many checkpoint entries the run actually held at the end. A result of the policy,
    /// not of the project, which is why it is not part of [`Report::identity`].
    pub checkpoints_stored: usize,
}

impl Report {
    /// Parse the text
    /// [`anim_headless::replay::report`](../anim_headless/replay/fn.report.html) produces.
    ///
    /// Strict, on purpose: an unrecognised line is a report format the caller does not
    /// understand, and a comparator that silently skips lines is how two different reports get
    /// called equal.
    ///
    /// # Errors
    ///
    /// [`DeterminismError::UnrecognisedLine`] or [`DeterminismError::MissingField`].
    pub fn parse(text: &str) -> Result<Self, DeterminismError> {
        let mut header = Vec::new();
        let mut ticks = Vec::new();
        let mut aggregate = None;
        let mut count = None;
        let mut final_hash = None;
        let mut checkpoints_stored = None;
        for line in text.lines() {
            if line == anim_headless::replay::REPORT_TAG {
                continue;
            }
            if line.is_empty() {
                continue;
            }
            let (key, value) =
                line.split_once(' ')
                    .ok_or_else(|| DeterminismError::UnrecognisedLine {
                        line: line.to_string(),
                    })?;
            match key {
                "tick" => {
                    // "tick <index> hash <hex>"
                    let mut parts = value.split(' ');
                    let index = parts.next().and_then(|v| v.parse().ok()).ok_or_else(|| {
                        DeterminismError::UnrecognisedLine {
                            line: line.to_string(),
                        }
                    })?;
                    if parts.next() != Some("hash") {
                        return Err(DeterminismError::UnrecognisedLine {
                            line: line.to_string(),
                        });
                    }
                    let hash = parts
                        .next()
                        .ok_or_else(|| DeterminismError::UnrecognisedLine {
                            line: line.to_string(),
                        })?;
                    if parts.next().is_some() {
                        return Err(DeterminismError::UnrecognisedLine {
                            line: line.to_string(),
                        });
                    }
                    ticks.push((index, hash.to_string()));
                }
                "aggregate" => aggregate = Some(value.to_string()),
                "count" => {
                    count = Some(
                        value
                            .parse()
                            .map_err(|_| DeterminismError::UnrecognisedLine {
                                line: line.to_string(),
                            })?,
                    )
                }
                "final_hash" => final_hash = Some(value.to_string()),
                "checkpoints_stored" => {
                    checkpoints_stored =
                        Some(
                            value
                                .parse()
                                .map_err(|_| DeterminismError::UnrecognisedLine {
                                    line: line.to_string(),
                                })?,
                        );
                }
                other => header.push((other.to_string(), value.to_string())),
            }
        }
        Ok(Self {
            header,
            ticks,
            aggregate: aggregate.ok_or(DeterminismError::MissingField { field: "aggregate" })?,
            count: count.ok_or(DeterminismError::MissingField { field: "count" })?,
            final_hash: final_hash.ok_or(DeterminismError::MissingField {
                field: "final_hash",
            })?,
            checkpoints_stored: checkpoints_stored.ok_or(DeterminismError::MissingField {
                field: "checkpoints_stored",
            })?,
        })
    }

    /// The value of one header field.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.header
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The identity fields a comparison is meaningless without: same project, same revision,
    /// same rates, same seed, same scene. Two reports of two different documents agreeing on
    /// hashes would be a coincidence worth failing on, so the documents are compared first.
    #[must_use]
    pub fn identity(&self) -> Vec<(String, String)> {
        [
            "schema_version",
            "project_id",
            "revision",
            "frame_rate",
            "ticks_per_frame",
            "simulation_rate",
            "seed",
            "scene",
            "last_tick",
        ]
        .iter()
        .filter_map(|name| {
            self.field(name)
                .map(|value| ((*name).to_string(), value.to_string()))
        })
        .collect()
    }
}

/// The ways this package's own helpers can fail.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DeterminismError {
    /// A line the report format does not define, or one whose shape is wrong.
    #[error("unrecognised report line: {line}")]
    UnrecognisedLine { line: String },

    /// A field the comparator requires that the report did not write.
    #[error("report has no {field} line")]
    MissingField { field: &'static str },

    /// Two reports whose identity fields disagree, so their hashes are not comparable.
    #[error("reports describe different projects: {detail}")]
    DifferentProjects { detail: String },

    /// Two reports of one project whose hashes disagree.
    #[error("state hashes disagree at {which}: {detail}")]
    HashMismatch { which: String, detail: String },
}

/// Assert that two reports are the same run of the same project.
///
/// The identity check runs before the hash check, because a difference in the header is a
/// different question from a difference in the hashes: one says the fixture drifted, the other
/// says the engine is nondeterministic, and a test that reports the second when the first is
/// true sends the reader to the wrong file.
///
/// # Errors
///
/// [`DeterminismError::DifferentProjects`] or [`DeterminismError::HashMismatch`].
pub fn assert_same_run(a: &Report, b: &Report, context: &str) -> Result<(), DeterminismError> {
    let left = a.identity();
    let right = b.identity();
    if left != right {
        // Only the disagreeing fields are named: two reports of one project differ in one
        // header value when a fixture drifts, and dumping all nine would bury it.
        let differences: Vec<String> = left
            .iter()
            .zip(right.iter())
            .filter(|(l, r)| l != r)
            .map(|(l, r)| format!("{}: {} vs {}", l.0, l.1, r.1))
            .collect();
        return Err(DeterminismError::DifferentProjects {
            detail: format!("{context}: {}", differences.join(", ")),
        });
    }
    if a.ticks != b.ticks && !a.ticks.is_empty() && !b.ticks.is_empty() {
        for (x, y) in a.ticks.iter().zip(b.ticks.iter()) {
            if x != y {
                return Err(DeterminismError::HashMismatch {
                    which: format!("{context} tick {}", x.0),
                    detail: format!("{} vs {}", x.1, y.1),
                });
            }
        }
        return Err(DeterminismError::HashMismatch {
            which: format!("{context} length"),
            detail: format!("{} lines vs {} lines", a.ticks.len(), b.ticks.len()),
        });
    }
    if a.aggregate != b.aggregate {
        return Err(DeterminismError::HashMismatch {
            which: format!("{context} aggregate"),
            detail: format!("{} vs {}", a.aggregate, b.aggregate),
        });
    }
    if a.final_hash != b.final_hash {
        return Err(DeterminismError::HashMismatch {
            which: format!("{context} final hash"),
            detail: format!("{} vs {}", a.final_hash, b.final_hash),
        });
    }
    Ok(())
}

/// Assert that two configurations cached differently, which is the precondition that makes a
/// comparison between them a test rather than a tautology.
///
/// # Errors
///
/// [`DeterminismError::HashMismatch`] naming what was actually observed, reusing the one
/// variant that fits: two supposedly different configurations that behaved identically *is* a
/// comparison that failed to compare.
pub fn assert_caching_differed(
    left_label: &str,
    left: usize,
    right_label: &str,
    right: usize,
) -> Result<(), DeterminismError> {
    if left == right {
        return Err(DeterminismError::HashMismatch {
            which: format!("{left_label} against {right_label}"),
            detail: format!(
                "both cached {left} entries, so this pair of configurations is one \
                 configuration wearing two labels"
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "test assertions only; the production rule is enforced at the workspace level"
    )]

    use super::*;

    /// A synthetic report, shaped exactly as `anim_headless::replay::report` writes one.
    ///
    /// `checkpoints_stored` is here because the real writer emits it: the parser requires the
    /// field, and a fixture that drifted from the format would test the parser against a
    /// document no process ever produces.
    fn sample_report(ticks: usize, aggregate: &str) -> String {
        let mut lines = vec![
            anim_headless::replay::REPORT_TAG.to_string(),
            "schema_version 3".to_string(),
            "project_id 00000000-0000-5000-8000-000000000000".to_string(),
            "revision 23".to_string(),
            "frame_rate 30000/1001".to_string(),
            "ticks_per_frame 4".to_string(),
            "simulation_rate 120000/1001".to_string(),
            "seed 5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c".to_string(),
            "scene 11111111-1111-5111-8111-111111111111".to_string(),
            "cadence_ticks 1".to_string(),
            "capacity 8".to_string(),
            format!("last_tick {}", ticks.saturating_sub(1)),
        ];
        for index in 0..ticks {
            lines.push(format!("tick {index} hash h{index}"));
        }
        lines.push(format!("checkpoints_stored {}", ticks / 2));
        lines.push(format!("count {ticks}"));
        lines.push("final_hash hlast".to_string());
        lines.push(format!("aggregate {aggregate}"));
        lines.join("\n") + "\n"
    }

    #[test]
    fn a_report_round_trips_into_its_fields() {
        let report = Report::parse(&sample_report(4, "abc")).unwrap();
        assert_eq!(report.ticks.len(), 4);
        assert_eq!(report.ticks[2], (2, "h2".to_string()));
        assert_eq!(report.aggregate, "abc");
        assert_eq!(report.count, 4);
        assert_eq!(report.final_hash, "hlast");
        assert_eq!(report.field("ticks_per_frame"), Some("4"));
        assert_eq!(
            report.checkpoints_stored, 2,
            "sample_report stores half its tick count"
        );
        assert_eq!(
            report.identity().len(),
            9,
            "checkpoints_stored is a result of the run, not an identity of the project, so              two configurations may differ in it and still be comparable"
        );
        // The header must not have swallowed a tick line or a field it did not need.
        assert!(
            !report.header.iter().any(|(key, _)| key == "tick"),
            "tick lines are not header"
        );
    }

    #[test]
    fn an_unrecognised_line_is_refused_rather_than_skipped() {
        for broken in [
            "nonsense\n".to_string(),
            "tick 0 notahash abc\n".to_string(),
            "tick x hash abc\n".to_string(),
            "count notanumber\naggregate a\nfinal_hash b\n".to_string(),
            "tick 0 hash abc extra\n".to_string(),
        ] {
            assert!(
                matches!(
                    Report::parse(&broken),
                    Err(DeterminismError::UnrecognisedLine { .. })
                        | Err(DeterminismError::MissingField { .. })
                ),
                "{broken:?} parses, which means the comparator would compare nothing"
            );
        }
        assert!(matches!(
            Report::parse("schema_version 3\ncount 1\n"),
            Err(DeterminismError::MissingField { field: "aggregate" })
        ));
    }

    #[test]
    fn two_identical_runs_are_the_same_run() {
        let text = sample_report(3, "agg");
        let a = Report::parse(&text).unwrap();
        let b = Report::parse(&text).unwrap();
        assert_same_run(&a, &b, "fixture").unwrap();
    }

    #[test]
    fn a_hash_difference_is_named_by_its_tick() {
        let mut other = Report::parse(&sample_report(3, "agg")).unwrap();
        other.ticks[1].1 = "different".to_string();
        let a = Report::parse(&sample_report(3, "agg")).unwrap();
        let error = assert_same_run(&a, &other, "cadence 7").unwrap_err();
        match error {
            DeterminismError::HashMismatch { which, detail } => {
                assert!(which.contains("cadence 7"), "{which}");
                assert!(which.contains("tick 1"), "{which}");
                assert!(detail.contains("different"), "{detail}");
            }
            unexpected => {
                panic!("a hash line changed and a different thing was reported: {unexpected}")
            }
        }
    }

    #[test]
    fn a_different_project_is_reported_as_that_and_not_as_a_hash_failure() {
        // The order matters for the reader: this failure means the fixture drifted, and a
        // report that said "hashes disagree" would send someone to anim_sim.
        let mut other = Report::parse(&sample_report(3, "agg")).unwrap();
        other.header.retain(|(key, _)| key != "seed");
        other.header.push(("seed".to_string(), "00".to_string()));
        let a = Report::parse(&sample_report(3, "agg")).unwrap();
        assert!(matches!(
            assert_same_run(&a, &other, "pair"),
            Err(DeterminismError::DifferentProjects { .. })
        ));
    }

    #[test]
    fn a_brief_report_compares_on_its_aggregate_alone() {
        let mut full = Report::parse(&sample_report(8, "agg")).unwrap();
        let brief = Report {
            ticks: Vec::new(),
            ..full.clone()
        };
        assert_same_run(&brief, &brief, "brief").unwrap();
        full.aggregate = "other".to_string();
        assert!(matches!(
            assert_same_run(&brief, &full, "brief"),
            Err(DeterminismError::HashMismatch { .. })
        ));
    }

    #[test]
    fn equal_caching_is_reported_as_a_vacuous_comparison() {
        assert!(assert_caching_differed("cadence 1", 8, "cadence 7", 8).is_err());
        assert!(assert_caching_differed("cadence 1", 8, "cadence 64", 1).is_ok());
    }
}
