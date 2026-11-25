//! Conversation formatting for transcript entries and run log records
//! (design section 8).

use onerig_core::plan::{Plan, VerdictKind};
use onerig_core::runner::HarnessEvent;
use onerig_core::session::TranscriptEntry;
use onerig_core::store::{LogEntry, LogRecord, PauseReason, PolicyEvent, StateEvent};

/// Formats one transcript record into display lines.
pub fn format_transcript_entry(entry: &TranscriptEntry) -> Vec<String> {
    match entry {
        TranscriptEntry::Owner { text, btw } => {
            let head = if *btw {
                "owner → planner (btw)"
            } else {
                "owner → planner"
            };
            labelled_lines(head, text)
        }
        TranscriptEntry::Planner { text, plan, btw } => {
            let head = if *btw {
                "planner → owner (btw)"
            } else {
                "planner → owner"
            };
            let mut out = labelled_lines(head, text);
            if let Some(proposal) = plan {
                out.push(format!("  proposed plan: {}", proposal.goal));
                for (i, task) in proposal.plan.tasks.iter().enumerate() {
                    out.push(format!("    t{}: {}", i + 1, task.title));
                }
                out.push("  type /go to approve and start, or keep chatting to revise".into());
            }
            out
        }
        TranscriptEntry::Run { .. } => Vec::new(),
        TranscriptEntry::System { text } => text.lines().map(|l| format!("onerig  {l}")).collect(),
    }
}

fn labelled_lines(head: &str, body: &str) -> Vec<String> {
    let mut lines = body.lines();
    let Some(first) = lines.next() else {
        return vec![format!("{head}:")];
    };
    let mut out = vec![format!("{head}: {first}")];
    out.extend(lines.map(|l| format!("  {l}")));
    out
}

#[derive(Debug, Default)]
pub struct LogFormatter {
    task_titles: Vec<String>,
    task_from_owner: Vec<bool>,
}

impl LogFormatter {
    pub fn load_plan(&mut self, plan: &Plan) {
        self.task_titles = plan.tasks.iter().map(|t| t.title.clone()).collect();
        self.task_from_owner = plan.tasks.iter().map(|t| t.from_owner).collect();
    }

    pub fn format(&mut self, record: &LogRecord) -> Vec<String> {
        match &record.entry {
            LogEntry::State(event) => self.format_state(event),
            LogEntry::Policy(event) => self.format_policy(record, event),
            LogEntry::Harness(HarnessEvent::Tool { name, detail }) => {
                let lane = record.lane.as_deref().unwrap_or("?");
                vec![format!("  {lane}  {name} {detail}")]
            }
            LogEntry::Harness(_) | LogEntry::Outcome(_) => Vec::new(),
        }
    }

    fn format_state(&mut self, event: &StateEvent) -> Vec<String> {
        match event {
            StateEvent::Run { run_id, goal, .. } => vec![format!("onerig  run {run_id}: {goal}")],
            StateEvent::Plan { tasks, titles } => self.record_plan(*tasks, titles),
            StateEvent::Verdict {
                verdict, summary, ..
            } => vec![format_verdict(*verdict, summary)],
            StateEvent::Commit { hash, title } => vec![format!("onerig  commit {hash} {title}")],
            StateEvent::Paused { reason, message } => first_and_rest(
                &format!("onerig  paused ({})", pause_reason_word(*reason)),
                message,
            ),
            StateEvent::Failed { message } => first_and_rest("onerig  failed", message),
            StateEvent::Done { branch } => vec![format!("onerig  done: branch {branch}")],
            StateEvent::Phase { .. } => Vec::new(),
        }
    }

    fn record_plan(&mut self, tasks: usize, titles: &[String]) -> Vec<String> {
        self.task_titles = titles.to_vec();
        if self.task_from_owner.len() != tasks {
            self.task_from_owner = vec![false; tasks];
        }
        vec![format!("onerig  plan: {tasks} tasks")]
    }

    fn format_policy(&self, record: &LogRecord, event: &PolicyEvent) -> Vec<String> {
        match event {
            PolicyEvent::LaneSelected { lane, .. } => {
                let num = record.task.unwrap_or(1);
                let idx = num.saturating_sub(1);
                let title = self
                    .task_titles
                    .get(idx)
                    .map(String::as_str)
                    .unwrap_or("task");
                let from_owner = self.task_from_owner.get(idx).copied().unwrap_or(false);
                let (who, tag) = if from_owner {
                    ("owner", "owner")
                } else {
                    ("planner", "agent")
                };
                vec![format!("{who} → {lane} [{tag}] t{num} {title}")]
            }
            PolicyEvent::LaneExhausted { lane, .. } => {
                vec![format!("onerig  {lane} out of quota")]
            }
            PolicyEvent::Handover { path } => {
                let lane = record.lane.as_deref().unwrap_or("?");
                vec![format!("onerig  {lane} hand-over → {}", path.display())]
            }
            PolicyEvent::Quota { .. }
            | PolicyEvent::Probe { .. }
            | PolicyEvent::LaneSkipped { .. } => Vec::new(),
        }
    }
}

fn format_verdict(verdict: VerdictKind, summary: &str) -> String {
    let word = match verdict {
        VerdictKind::Approve => "approve",
        VerdictKind::Revise => "revise",
    };
    format!("reviewer → owner  {word}: {summary}")
}

fn first_and_rest(prefix: &str, message: &str) -> Vec<String> {
    let mut lines = message.lines();
    let first = lines.next().unwrap_or_default();
    let mut out = vec![format!("{prefix}: {first}")];
    out.extend(lines.map(|l| format!("  {l}")));
    out
}

fn pause_reason_word(reason: PauseReason) -> &'static str {
    match reason {
        PauseReason::NoLane => "no_lane",
        PauseReason::Handover => "handover",
        PauseReason::Quota => "quota",
        PauseReason::Auth => "auth",
        PauseReason::AttemptsExhausted => "attempts_exhausted",
        PauseReason::JobFailed => "job_failed",
        PauseReason::GitError => "git_error",
        PauseReason::Interrupted => "interrupted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onerig_core::plan::Task;
    use onerig_core::turn::Proposal;

    #[test]
    fn transcript_formatting_marks_btw_and_proposals() {
        let owner_btw = TranscriptEntry::Owner {
            text: "status?".into(),
            btw: true,
        };
        let proposal = TranscriptEntry::Planner {
            text: "Ready.".into(),
            plan: Some(Proposal {
                goal: "Add sub".into(),
                plan: Plan {
                    tasks: vec![Task {
                        id: "t1".into(),
                        title: "Add subtract".into(),
                        instructions: "do".into(),
                        acceptance: vec!["ok".into()],
                        lane: None,
                        from_owner: false,
                    }],
                },
            }),
            btw: false,
        };

        assert_eq!(
            format_transcript_entry(&owner_btw),
            ["owner → planner (btw): status?"]
        );
        let lines = format_transcript_entry(&proposal);
        assert!(lines[0] == "planner → owner: Ready.");
        assert!(lines.iter().any(|l| l.contains("t1: Add subtract")));
        assert!(lines.iter().any(|l| l.contains("/go")));
    }

    #[test]
    fn log_formatter_labels_owner_and_planner_tasks_commits_and_done() {
        let mut fmt = LogFormatter::default();
        fmt.load_plan(&Plan {
            tasks: vec![Task {
                id: "owner-1".into(),
                title: "Fix README".into(),
                instructions: "Fix".into(),
                acceptance: vec!["ok".into()],
                lane: Some("w1".into()),
                from_owner: true,
            }],
        });
        let rec = |task, lane: Option<&str>, entry| LogRecord {
            ts: 1,
            task,
            attempt: Some(1),
            lane: lane.map(str::to_string),
            entry,
        };

        assert_eq!(
            fmt.format(&rec(
                Some(1),
                Some("w1"),
                LogEntry::Policy(PolicyEvent::LaneSelected {
                    lane: "w1".into(),
                    manual: false
                })
            )),
            ["owner → w1 [owner] t1 Fix README"]
        );
        assert_eq!(
            fmt.format(&rec(
                Some(1),
                Some("w1"),
                LogEntry::Harness(HarnessEvent::Tool {
                    name: "edit".into(),
                    detail: "README.md".into()
                })
            )),
            ["  w1  edit README.md"]
        );
        assert_eq!(
            fmt.format(&rec(
                None,
                None,
                LogEntry::State(StateEvent::Done {
                    branch: "onerig/r1".into()
                })
            )),
            ["onerig  done: branch onerig/r1"]
        );
    }

    #[test]
    fn format_covers_verdicts_pauses_failures_and_handover() {
        let mut fmt = LogFormatter::default();
        let rec = |lane: Option<&str>, entry| LogRecord {
            ts: 1,
            task: Some(1),
            attempt: Some(1),
            lane: lane.map(str::to_string),
            entry,
        };
        assert_eq!(
            fmt.format(&rec(
                Some("review"),
                LogEntry::State(StateEvent::Verdict {
                    verdict: VerdictKind::Revise,
                    summary: "missing test".into(),
                    issues: vec![],
                })
            )),
            ["reviewer → owner  revise: missing test"]
        );
        assert_eq!(
            fmt.format(&rec(
                None,
                LogEntry::State(StateEvent::Failed {
                    message: "line1\nline2".into(),
                })
            )),
            ["onerig  failed: line1", "  line2"]
        );
        for (reason, expected) in [
            (PauseReason::NoLane, "no_lane"),
            (PauseReason::Handover, "handover"),
            (PauseReason::Quota, "quota"),
            (PauseReason::Auth, "auth"),
            (PauseReason::AttemptsExhausted, "attempts_exhausted"),
            (PauseReason::JobFailed, "job_failed"),
            (PauseReason::GitError, "git_error"),
            (PauseReason::Interrupted, "interrupted"),
        ] {
            let lines = fmt.format(&rec(
                None,
                LogEntry::State(StateEvent::Paused {
                    reason,
                    message: "stop".into(),
                }),
            ));
            assert!(lines[0].contains(expected));
        }
        assert_eq!(
            fmt.format(&rec(
                Some("w1"),
                LogEntry::Policy(PolicyEvent::LaneExhausted {
                    lane: "w1".into(),
                    until: None,
                })
            )),
            ["onerig  w1 out of quota"]
        );
        assert_eq!(
            fmt.format(&rec(
                Some("gemini"),
                LogEntry::Policy(PolicyEvent::Handover {
                    path: std::path::PathBuf::from("/tmp/h.md"),
                })
            )),
            ["onerig  gemini hand-over → /tmp/h.md"]
        );
    }
}
