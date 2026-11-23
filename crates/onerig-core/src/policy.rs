//! Lane selection and failure policy: which lane takes the next worker
//! attempt, and what a failed worker job leads to. Every function is a pure
//! decision: the caller reads the clock and the quota, and stores the result.

use crate::runner::{FailureKind, QuotaReport};
use crate::store::{LaneMark, LastEnd};

/// One lane of the worker chain, as selection sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LaneView<'a> {
    pub name: &'a str,
    pub off: bool,
    pub manual: bool,
    /// Message of the lane's failed probe.
    pub probe_error: Option<&'a str>,
    pub mark: LaneMark,
    /// The harness's report for this selection; `None` when it gives none.
    pub quota: Option<QuotaReport>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Lane {
        name: String,
        manual: bool,
    },
    NoLane {
        /// Every lane with the reason it cannot be used.
        message: String,
        /// Earliest known reset time, unix milliseconds.
        until: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    pub selection: Selection,
    /// Exhausted marks set by low quota reports, for the caller to store.
    pub new_marks: Vec<(String, LaneMark)>,
}

const EXHAUSTED: &str = "quota exhausted";
const SKIPPED: &str = "skipped for this task";
/// A quota failure keeps its lane out for at least this long, so a wrong reset
/// time cannot make the run spin.
const MIN_EXHAUSTION_MS: u64 = 60_000;

/// Chooses the lane for the next worker attempt. `lanes` is in chain order.
pub fn select(
    lanes: &[LaneView<'_>],
    current: Option<&str>,
    skipped: &[String],
    min_remaining: f64,
    now_ms: u64,
) -> Selected {
    let judged: Vec<Judged<'_>> = lanes
        .iter()
        .map(|lane| judge(lane, skipped, min_remaining, now_ms))
        .collect();
    let new_marks = judged
        .iter()
        .filter_map(|j| j.new_mark.map(|mark| (j.lane.name.to_string(), mark)))
        .collect();
    let selection = pick(&judged, current).unwrap_or_else(|| no_lane(&judged));
    Selected {
        selection,
        new_marks,
    }
}

/// Why a lane cannot take the attempt.
struct Blocked {
    reason: String,
    until: Option<u64>,
}

struct Judged<'a> {
    lane: &'a LaneView<'a>,
    blocked: Option<Blocked>,
    new_mark: Option<LaneMark>,
}

fn judge<'a>(
    lane: &'a LaneView<'a>,
    skipped: &[String],
    min_remaining: f64,
    now_ms: u64,
) -> Judged<'a> {
    let (blocked, new_mark) = blocked_by(lane, skipped, min_remaining, now_ms);
    Judged {
        lane,
        blocked,
        new_mark,
    }
}

fn blocked_by(
    lane: &LaneView<'_>,
    skipped: &[String],
    min_remaining: f64,
    now_ms: u64,
) -> (Option<Blocked>, Option<LaneMark>) {
    let block = |reason: &str, until| {
        Some(Blocked {
            reason: reason.to_string(),
            until,
        })
    };
    if lane.manual {
        return (None, None);
    }
    if lane.off {
        return (block("off", None), None);
    }
    if let Some(error) = lane.probe_error {
        return (block(error, None), None);
    }
    if is_exhausted(lane.mark, now_ms) {
        return (block(EXHAUSTED, lane.mark.exhausted_until), None);
    }
    if skipped.iter().any(|name| name == lane.name) {
        return (block(SKIPPED, None), None);
    }
    match lane.quota {
        Some(report) if report.remaining_fraction < min_remaining => {
            let mark = LaneMark {
                exhausted: true,
                exhausted_until: report.reset_at,
            };
            (block(EXHAUSTED, report.reset_at), Some(mark))
        }
        _ => (None, None),
    }
}

fn is_exhausted(mark: LaneMark, now_ms: u64) -> bool {
    mark.exhausted && mark.exhausted_until.is_none_or(|until| until > now_ms)
}

/// The current lane if it is usable, else the first usable automatic lane,
/// else the first manual lane.
fn pick(judged: &[Judged<'_>], current: Option<&str>) -> Option<Selection> {
    let usable = |j: &&Judged<'_>| j.blocked.is_none();
    judged
        .iter()
        .filter(usable)
        .find(|j| Some(j.lane.name) == current)
        .or_else(|| judged.iter().filter(usable).find(|j| !j.lane.manual))
        .or_else(|| judged.iter().find(|j| j.lane.manual))
        .map(|j| Selection::Lane {
            name: j.lane.name.to_string(),
            manual: j.lane.manual,
        })
}

fn no_lane(judged: &[Judged<'_>]) -> Selection {
    let blocked: Vec<(&str, &Blocked)> = judged
        .iter()
        .filter_map(|j| j.blocked.as_ref().map(|b| (j.lane.name, b)))
        .collect();
    let message = blocked
        .iter()
        .map(|(name, b)| format!("{name}: {}", b.reason))
        .collect::<Vec<_>>()
        .join("; ");
    let until = blocked.iter().filter_map(|(_, b)| b.until).min();
    Selection::NoLane { message, until }
}

/// What the run does after a failed worker job, before it selects again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterFailure {
    /// `None` keeps the mark until the next resume.
    MarkExhausted {
        until: Option<u64>,
    },
    PauseAuth,
    SkipLane,
    RetrySameLane,
}

/// `unknown_streak` counts the `Unknown` failures in a row before this one.
pub fn after_failure(
    kind: FailureKind,
    reset_at: Option<u64>,
    unknown_streak: u32,
    now_ms: u64,
) -> AfterFailure {
    match kind {
        FailureKind::Quota => AfterFailure::MarkExhausted {
            until: reset_at.map(|at| at.max(now_ms + MIN_EXHAUSTION_MS)),
        },
        FailureKind::Auth => AfterFailure::PauseAuth,
        FailureKind::Timeout => AfterFailure::SkipLane,
        FailureKind::Unknown if unknown_streak == 0 => AfterFailure::RetrySameLane,
        FailureKind::Unknown => AfterFailure::SkipLane,
    }
}

pub fn budget_exhausted(attempts_used: u32, max_attempts: u32) -> bool {
    attempts_used >= max_attempts
}

/// How a failed attempt is described in the next hand-over.
pub fn last_end(kind: FailureKind) -> LastEnd {
    match kind {
        FailureKind::Quota => LastEnd::Quota,
        FailureKind::Timeout => LastEnd::Timeout,
        FailureKind::Auth | FailureKind::Unknown => LastEnd::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;
    const MIN: f64 = 0.05;
    const EXHAUSTED: &str = "quota exhausted";

    fn auto(name: &str) -> LaneView<'_> {
        LaneView {
            name,
            off: false,
            manual: false,
            probe_error: None,
            mark: LaneMark::default(),
            quota: None,
        }
    }

    fn manual(name: &str) -> LaneView<'_> {
        LaneView {
            manual: true,
            ..auto(name)
        }
    }

    fn off(name: &str) -> LaneView<'_> {
        LaneView {
            off: true,
            ..auto(name)
        }
    }

    fn marked(name: &str, until: Option<u64>) -> LaneView<'_> {
        LaneView {
            mark: exhausted(until),
            ..auto(name)
        }
    }

    fn reporting(name: &str, remaining_fraction: f64, reset_at: Option<u64>) -> LaneView<'_> {
        LaneView {
            quota: Some(QuotaReport {
                remaining_fraction,
                reset_at,
            }),
            ..auto(name)
        }
    }

    fn exhausted(until: Option<u64>) -> LaneMark {
        LaneMark {
            exhausted: true,
            exhausted_until: until,
        }
    }

    fn pick(lanes: &[LaneView<'_>], current: Option<&str>) -> Selected {
        select(lanes, current, &[], MIN, NOW)
    }

    fn chosen(name: &str, manual: bool) -> Selected {
        Selected {
            selection: Selection::Lane {
                name: name.into(),
                manual,
            },
            new_marks: vec![],
        }
    }

    fn no_lane(message: &str, until: Option<u64>) -> Selected {
        Selected {
            selection: Selection::NoLane {
                message: message.into(),
                until,
            },
            new_marks: vec![],
        }
    }

    #[test]
    fn an_off_lane_is_unavailable() {
        let lanes = [off("gemini")];

        let selected = pick(&lanes, None);

        assert_eq!(selected, no_lane("gemini: off", None));
    }

    #[test]
    fn a_lane_whose_probe_failed_is_unavailable_and_gives_the_probe_message() {
        let lanes = [LaneView {
            probe_error: Some("codex was not found on PATH"),
            ..auto("deepseek")
        }];

        let selected = pick(&lanes, None);

        assert_eq!(
            selected,
            no_lane("deepseek: codex was not found on PATH", None)
        );
    }

    #[test]
    fn a_lane_with_an_exhausted_mark_is_unavailable_before_its_reset_time() {
        let lanes = [marked("deepseek", Some(NOW + 1))];

        let selected = pick(&lanes, None);

        assert_eq!(
            selected,
            no_lane("deepseek: quota exhausted", Some(NOW + 1))
        );
    }

    #[test]
    fn an_exhausted_mark_without_a_reset_time_stays() {
        let lanes = [marked("deepseek", None)];

        let selected = pick(&lanes, None);

        assert_eq!(selected, no_lane("deepseek: quota exhausted", None));
    }

    #[test]
    fn a_mark_whose_reset_time_has_passed_is_available() {
        let lanes = [marked("deepseek", Some(NOW))];

        let selected = pick(&lanes, None);

        assert_eq!(selected, chosen("deepseek", false));
    }

    #[test]
    fn a_lane_skipped_for_the_task_is_unavailable() {
        let lanes = [auto("deepseek")];
        let skipped = ["deepseek".to_string()];

        let selected = select(&lanes, None, &skipped, MIN, NOW);

        assert_eq!(selected, no_lane("deepseek: skipped for this task", None));
    }

    #[test]
    fn a_quota_report_below_the_minimum_marks_the_lane_with_the_reports_reset_time() {
        let lanes = [reporting("gemini", 0.04, Some(NOW + 5_000))];

        let selected = pick(&lanes, None);

        assert_eq!(
            selected,
            Selected {
                new_marks: vec![("gemini".into(), exhausted(Some(NOW + 5_000)))],
                ..no_lane("gemini: quota exhausted", Some(NOW + 5_000))
            }
        );
    }

    #[test]
    fn a_quota_report_at_the_minimum_is_available() {
        let lanes = [reporting("gemini", MIN, Some(NOW + 5_000))];

        let selected = pick(&lanes, None);

        assert_eq!(selected, chosen("gemini", false));
    }

    #[test]
    fn a_lane_without_a_quota_report_is_available() {
        let lanes = [auto("deepseek")];

        let selected = pick(&lanes, None);

        assert_eq!(selected, chosen("deepseek", false));
    }

    #[test]
    fn a_low_report_marks_only_a_lane_that_was_available_without_it() {
        let lanes = [reporting("gemini", 0.0, None)];
        let skipped = ["gemini".to_string()];

        let selected = select(&lanes, None, &skipped, MIN, NOW);

        assert_eq!(selected, no_lane("gemini: skipped for this task", None));
    }

    #[test]
    fn a_manual_lane_is_always_available() {
        let lanes = [LaneView {
            manual: true,
            probe_error: Some("never probed"),
            mark: exhausted(None),
            quota: Some(QuotaReport {
                remaining_fraction: 0.0,
                reset_at: None,
            }),
            ..auto("gemini")
        }];
        let skipped = ["gemini".to_string()];

        let selected = select(&lanes, None, &skipped, MIN, NOW);

        assert_eq!(selected, chosen("gemini", true));
    }

    #[test]
    fn the_current_lane_is_kept_when_available() {
        let lanes = [auto("deepseek"), auto("kimi")];

        let selected = pick(&lanes, Some("kimi"));

        assert_eq!(selected, chosen("kimi", false));
    }

    #[test]
    fn a_current_manual_lane_is_kept_while_an_automatic_lane_is_available() {
        let lanes = [auto("deepseek"), manual("gemini")];

        let selected = pick(&lanes, Some("gemini"));

        assert_eq!(selected, chosen("gemini", true));
    }

    #[test]
    fn the_first_available_automatic_lane_replaces_an_unavailable_current_lane() {
        let lanes = [
            off("glm"),
            marked("deepseek", None),
            auto("kimi"),
            auto("qwen"),
        ];

        let selected = pick(&lanes, Some("deepseek"));

        assert_eq!(selected, chosen("kimi", false));
    }

    #[test]
    fn a_manual_lane_ahead_in_the_chain_waits_while_an_automatic_lane_is_available() {
        let lanes = [manual("gemini"), off("glm"), auto("deepseek")];

        let selected = pick(&lanes, None);

        assert_eq!(selected, chosen("deepseek", false));
    }

    #[test]
    fn the_first_manual_lane_is_taken_when_no_automatic_lane_is_available() {
        let lanes = [
            marked("deepseek", None),
            manual("gemini"),
            manual("gemini-pro"),
            off("glm"),
        ];

        let selected = pick(&lanes, Some("deepseek"));

        assert_eq!(selected, chosen("gemini", true));
    }

    #[test]
    fn no_lane_lists_every_lane_with_its_reason() {
        let lanes = [marked("deepseek", None), off("gemini")];

        let selected = pick(&lanes, None);

        assert_eq!(
            selected,
            no_lane("deepseek: quota exhausted; gemini: off", None)
        );
    }

    #[test]
    fn no_lane_gives_the_earliest_known_reset_time_among_exhausted_lanes() {
        let lanes = [
            marked("deepseek", None),
            marked("kimi", Some(NOW + 9_000)),
            reporting("gemini", 0.01, Some(NOW + 3_000)),
            marked("qwen", Some(NOW + 6_000)),
        ];

        let selected = pick(&lanes, None);

        assert_eq!(
            selected,
            Selected {
                new_marks: vec![("gemini".into(), exhausted(Some(NOW + 3_000)))],
                ..no_lane(
                    &["deepseek", "kimi", "gemini", "qwen"]
                        .map(|name| format!("{name}: {EXHAUSTED}"))
                        .join("; "),
                    Some(NOW + 3_000)
                )
            }
        );
    }

    #[test]
    fn a_quota_failure_marks_the_lane_exhausted_until_the_reset_time() {
        let reset_at = Some(NOW + 120_000);

        let action = after_failure(FailureKind::Quota, reset_at, 0, NOW);

        assert_eq!(action, AfterFailure::MarkExhausted { until: reset_at });
    }

    #[test]
    fn a_quota_mark_lasts_at_least_sixty_seconds() {
        let reset_at = Some(NOW + 59_999);

        let action = after_failure(FailureKind::Quota, reset_at, 0, NOW);

        assert_eq!(
            action,
            AfterFailure::MarkExhausted {
                until: Some(NOW + 60_000)
            }
        );
    }

    #[test]
    fn a_quota_failure_without_a_reset_time_marks_the_lane_until_resume() {
        let action = after_failure(FailureKind::Quota, None, 0, NOW);

        assert_eq!(action, AfterFailure::MarkExhausted { until: None });
    }

    #[test]
    fn an_auth_failure_pauses() {
        let action = after_failure(FailureKind::Auth, None, 0, NOW);

        assert_eq!(action, AfterFailure::PauseAuth);
    }

    #[test]
    fn a_timeout_skips_the_lane() {
        let action = after_failure(FailureKind::Timeout, None, 0, NOW);

        assert_eq!(action, AfterFailure::SkipLane);
    }

    #[test]
    fn a_first_unknown_failure_retries_the_same_lane() {
        let action = after_failure(FailureKind::Unknown, None, 0, NOW);

        assert_eq!(action, AfterFailure::RetrySameLane);
    }

    #[test]
    fn a_second_unknown_failure_in_a_row_skips_the_lane() {
        let action = after_failure(FailureKind::Unknown, None, 1, NOW);

        assert_eq!(action, AfterFailure::SkipLane);
    }

    #[test]
    fn the_budget_holds_while_attempts_remain() {
        let exhausted = budget_exhausted(4, 5);

        assert!(!exhausted);
    }

    #[test]
    fn the_budget_is_exhausted_once_the_maximum_is_used() {
        let at_the_maximum = budget_exhausted(5, 5);
        let past_the_maximum = budget_exhausted(6, 5);

        assert!(at_the_maximum);
        assert!(past_the_maximum);
    }

    #[test]
    fn the_hand_over_reason_follows_the_failure_kind() {
        let ends = [
            FailureKind::Quota,
            FailureKind::Timeout,
            FailureKind::Auth,
            FailureKind::Unknown,
        ]
        .map(last_end);

        assert_eq!(
            ends,
            [
                LastEnd::Quota,
                LastEnd::Timeout,
                LastEnd::Failed,
                LastEnd::Failed
            ]
        );
    }
}
