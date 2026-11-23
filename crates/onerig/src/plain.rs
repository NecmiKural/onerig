//! One line of plain output per log record (design section 12).

use onerig_core::runner::{HarnessEvent, Outcome};
use onerig_core::store::{LogEntry, LogRecord, PolicyEvent, StateEvent, format_time};
use serde::Serialize;

const TEXT_WIDTH: usize = 100;

/// The line a record prints, or `None` for records that print nothing.
pub fn line(record: &LogRecord) -> Option<String> {
    let body = match &record.entry {
        LogEntry::State(event) => state_line(record, event)?,
        LogEntry::Policy(event) => policy_line(record, event),
        LogEntry::Harness(event) => format!("{}  {}", who(record), harness_text(event)),
        LogEntry::Outcome(outcome) => format!("{}  {}", who(record), outcome_text(outcome)),
    };
    Some(format!("{} {body}", format_time(record.ts)))
}

fn state_line(record: &LogRecord, event: &StateEvent) -> Option<String> {
    let task = record.task.map(|n| format!("t{n}")).unwrap_or_default();
    Some(match event {
        StateEvent::Phase { .. } => return None,
        StateEvent::Plan { tasks } => format!("plan      {tasks} tasks"),
        StateEvent::Verdict { verdict, .. } => format!("{task} review       {}", word(verdict)),
        StateEvent::Commit { hash, title } => format!("{task} commit       {hash} {title}"),
        StateEvent::Paused { reason, message } => format!("paused    {}: {message}", word(reason)),
        StateEvent::Done { branch } => format!("done      branch {branch}"),
        StateEvent::Failed { message } => format!("failed    {message}"),
    })
}

fn policy_line(record: &LogRecord, event: &PolicyEvent) -> String {
    match event {
        PolicyEvent::Probe {
            lane,
            ok: true,
            message,
        } => format!("probe {lane}  ok {message}"),
        PolicyEvent::Probe { lane, message, .. } => format!("probe {lane}  failed: {message}"),
        PolicyEvent::LaneSelected { manual, .. } => {
            let how = if *manual {
                "selected (manual)"
            } else {
                "selected"
            };
            format!("{}  {how}", who(record))
        }
        PolicyEvent::LaneExhausted { lane, until } => {
            let until = until.map_or_else(|| "resume".to_string(), format_time);
            format!("{lane}  exhausted until {until}")
        }
        PolicyEvent::LaneSkipped { lane } => format!("{lane}  skipped for this task"),
        PolicyEvent::Handover { path } => {
            format!("{}  hand-over {}", who(record), path.display())
        }
    }
}

fn harness_text(event: &HarnessEvent) -> String {
    match event {
        HarnessEvent::Started { model } => {
            format!("started ({})", model.as_deref().unwrap_or("default model"))
        }
        HarnessEvent::Text { text } => text
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(TEXT_WIDTH)
            .collect(),
        HarnessEvent::Tool { name, detail } => format!("tool {name} {detail}"),
    }
}

fn outcome_text(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Done { .. } => "done".to_string(),
        Outcome::Failed { kind, message, .. } => format!("failed {}: {message}", word(kind)),
    }
}

/// `t<task> a<attempt> <lane>`, leaving out what the record does not have.
fn who(record: &LogRecord) -> String {
    let parts = [
        record.task.map(|n| format!("t{n}")),
        record.attempt.map(|n| format!("a{n}")),
        record.lane.clone(),
    ];
    parts.into_iter().flatten().collect::<Vec<_>>().join(" ")
}

/// The snake_case name serde gives an enum value.
fn word(value: &impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use onerig_core::plan::VerdictKind;
    use onerig_core::runner::FailureKind;
    use onerig_core::store::{PauseReason, Phase};

    const TS: u64 = 1_795_464_901_000;

    fn record(
        task: Option<usize>,
        attempt: Option<u32>,
        lane: Option<&str>,
        entry: LogEntry,
    ) -> LogRecord {
        LogRecord {
            ts: TS,
            task,
            attempt,
            lane: lane.map(str::to_string),
            entry,
        }
    }

    #[test]
    fn worker_records_name_the_task_attempt_and_lane() {
        let tool = record(
            Some(1),
            Some(2),
            Some("deepseek"),
            LogEntry::Harness(HarnessEvent::Tool {
                name: "edit".into(),
                detail: "src/http.rs".into(),
            }),
        );
        let failed = record(
            Some(1),
            Some(2),
            Some("deepseek"),
            LogEntry::Outcome(Outcome::Failed {
                kind: FailureKind::Quota,
                message: "balance is empty".into(),
                reset_at: None,
            }),
        );

        assert_eq!(
            line(&tool).unwrap(),
            "20:15:01 t1 a2 deepseek  tool edit src/http.rs"
        );
        assert_eq!(
            line(&failed).unwrap(),
            "20:15:01 t1 a2 deepseek  failed quota: balance is empty"
        );
    }

    #[test]
    fn run_milestones_have_their_own_lines() {
        let plan = record(
            None,
            None,
            None,
            LogEntry::State(StateEvent::Plan { tasks: 3 }),
        );
        let verdict = record(
            Some(1),
            Some(1),
            None,
            LogEntry::State(StateEvent::Verdict {
                verdict: VerdictKind::Approve,
                summary: String::new(),
                issues: vec![],
            }),
        );
        let paused = record(
            None,
            None,
            None,
            LogEntry::State(StateEvent::Paused {
                reason: PauseReason::NoLane,
                message: "deepseek: quota exhausted".into(),
            }),
        );
        let done = record(
            None,
            None,
            None,
            LogEntry::State(StateEvent::Done {
                branch: "onerig/x".into(),
            }),
        );

        assert_eq!(line(&plan).unwrap(), "20:15:01 plan      3 tasks");
        assert_eq!(line(&verdict).unwrap(), "20:15:01 t1 review       approve");
        assert_eq!(
            line(&paused).unwrap(),
            "20:15:01 paused    no_lane: deepseek: quota exhausted"
        );
        assert_eq!(line(&done).unwrap(), "20:15:01 done      branch onerig/x");
    }

    #[test]
    fn a_phase_change_prints_nothing() {
        let phase = record(
            None,
            None,
            None,
            LogEntry::State(StateEvent::Phase {
                phase: Phase::Executing,
            }),
        );

        assert_eq!(line(&phase), None);
    }

    #[test]
    fn text_shows_only_its_first_line_cut_to_a_fixed_width() {
        let text = format!("{}\nsecond line", "x".repeat(150));
        let event = record(
            Some(1),
            Some(1),
            Some("w"),
            LogEntry::Harness(HarnessEvent::Text { text }),
        );

        let printed = line(&event).unwrap();

        assert_eq!(
            printed,
            format!("20:15:01 t1 a1 w  {}", "x".repeat(TEXT_WIDTH))
        );
    }

    #[test]
    fn lane_decisions_say_what_happened() {
        let exhausted = record(
            Some(1),
            None,
            None,
            LogEntry::Policy(PolicyEvent::LaneExhausted {
                lane: "deepseek".into(),
                until: None,
            }),
        );
        let probe = record(
            None,
            None,
            None,
            LogEntry::Policy(PolicyEvent::Probe {
                lane: "plan".into(),
                ok: false,
                message: "not logged in".into(),
            }),
        );

        assert_eq!(
            line(&exhausted).unwrap(),
            "20:15:01 deepseek  exhausted until resume"
        );
        assert_eq!(
            line(&probe).unwrap(),
            "20:15:01 probe plan  failed: not logged in"
        );
    }
}
