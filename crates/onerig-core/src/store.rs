//! What a run keeps on disk: its state, its plan, its log and its lock.
//! The data model is here; reading and writing follow below.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::plan::{Issue, VerdictKind};
use crate::runner::{HarnessEvent, Outcome};

/// Version of the `state.json` and `plan.json` format other tools may read.
/// Within one version fields are only added.
pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub v: u32,
    pub run_id: String,
    pub goal: String,
    /// Unix milliseconds; set on every write.
    pub updated_at: u64,
    /// The main checkout.
    pub repo: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<Paused>,
    /// 1-based position in the plan's tasks; `None` before the first task.
    pub task_index: Option<usize>,
    /// The worktree's `HEAD` when the current task started.
    pub task_base: Option<String>,
    /// The task's running attempt number, 1-based; never reset by resume.
    pub attempt: Option<u32>,
    /// What the attempt budget counts; resume and a new task set it to 0.
    pub attempts_used: u32,
    pub lane: Option<String>,
    pub unknown_streak: u32,
    /// Lanes skipped for the current task.
    pub skipped: Vec<String>,
    pub last_end: Option<LastEnd>,
    /// True between an approve verdict and the task commit.
    pub approved: bool,
    pub issues: Vec<Issue>,
    pub last_output_tail: String,
    /// Numbers every job of the run; names the raw log files.
    pub job_seq: u64,
    pub lanes: BTreeMap<String, LaneMark>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Planning,
    Executing,
    Reviewing,
    Paused,
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paused {
    pub reason: PauseReason,
    pub resume_phase: Phase,
    pub message: String,
    /// Earliest known reset time, unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    NoLane,
    Handover,
    Quota,
    Auth,
    AttemptsExhausted,
    JobFailed,
    GitError,
    Interrupted,
}

/// How the last worker attempt ended; gives the next hand-over its reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LastEnd {
    Quota,
    Timeout,
    Failed,
    Interrupted,
    Revise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LaneMark {
    #[serde(default)]
    pub exhausted: bool,
    /// Unix milliseconds; `None` with `exhausted` means until the next resume.
    #[serde(default)]
    pub exhausted_until: Option<u64>,
}

/// One line of `log.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogRecord {
    /// Unix milliseconds.
    pub ts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    #[serde(flatten)]
    pub entry: LogEntry,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum LogEntry {
    State(StateEvent),
    Policy(PolicyEvent),
    Harness(HarnessEvent),
    Outcome(Outcome),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StateEvent {
    Plan {
        tasks: usize,
    },
    Phase {
        phase: Phase,
    },
    Verdict {
        verdict: VerdictKind,
        summary: String,
        issues: Vec<Issue>,
    },
    Commit {
        hash: String,
        title: String,
    },
    Paused {
        reason: PauseReason,
        message: String,
    },
    Done {
        branch: String,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PolicyEvent {
    Probe {
        lane: String,
        ok: bool,
        message: String,
    },
    LaneSelected {
        lane: String,
        manual: bool,
    },
    LaneExhausted {
        lane: String,
        until: Option<u64>,
    },
    LaneSkipped {
        lane: String,
    },
    Handover {
        path: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::FailureKind;
    use serde_json::json;

    // These are the log lines the design document shows; other tools and the
    // plain output depend on them.
    #[test]
    fn log_records_have_the_documented_shape() {
        let harness = LogRecord {
            ts: 1_763_929_001_000,
            task: Some(1),
            attempt: Some(1),
            lane: Some("deepseek".into()),
            entry: LogEntry::Harness(HarnessEvent::Tool {
                name: "edit".into(),
                detail: "src/http.rs".into(),
            }),
        };
        let policy = LogRecord {
            ts: 1_763_929_090_001,
            task: Some(1),
            attempt: None,
            lane: None,
            entry: LogEntry::Policy(PolicyEvent::LaneExhausted {
                lane: "deepseek".into(),
                until: None,
            }),
        };

        assert_eq!(
            serde_json::to_value(&harness).unwrap(),
            json!({ "ts": 1_763_929_001_000u64, "task": 1, "attempt": 1, "lane": "deepseek",
                    "kind": "harness", "data": { "type": "tool", "name": "edit", "detail": "src/http.rs" } })
        );
        assert_eq!(
            serde_json::to_value(&policy).unwrap(),
            json!({ "ts": 1_763_929_090_001u64, "task": 1,
                    "kind": "policy", "data": { "type": "lane_exhausted", "lane": "deepseek", "until": null } })
        );
    }

    #[test]
    fn a_log_record_survives_a_round_trip() {
        let record = LogRecord {
            ts: 5,
            task: Some(2),
            attempt: Some(3),
            lane: Some("gemini".into()),
            entry: LogEntry::Outcome(Outcome::Failed {
                kind: FailureKind::Quota,
                message: "empty".into(),
                reset_at: Some(9),
            }),
        };

        let line = serde_json::to_string(&record).unwrap();

        assert_eq!(serde_json::from_str::<LogRecord>(&line).unwrap(), record);
    }

    #[test]
    fn a_paused_state_reads_back_with_optional_fields_absent() {
        let text = json!({
            "v": 1, "run_id": "20261123-201500-a1f3", "goal": "g", "updated_at": 1,
            "repo": "/r", "worktree": "/w", "branch": "onerig/20261123-201500-a1f3",
            "phase": "paused",
            "paused": { "reason": "no_lane", "resume_phase": "executing", "message": "m" },
            "task_index": 2, "task_base": "3f2a", "attempt": 2, "attempts_used": 2,
            "lane": "deepseek", "unknown_streak": 0, "skipped": [], "last_end": "quota",
            "approved": false, "issues": [], "last_output_tail": "", "job_seq": 7,
            "lanes": { "deepseek": { "exhausted": true, "exhausted_until": null }, "gemini": {} }
        });

        let state: State = serde_json::from_value(text).unwrap();

        assert_eq!(state.phase, Phase::Paused);
        assert_eq!(state.paused.as_ref().unwrap().reason, PauseReason::NoLane);
        assert_eq!(state.paused.as_ref().unwrap().until, None);
        assert_eq!(state.last_end, Some(LastEnd::Quota));
        assert!(state.lanes["deepseek"].exhausted);
        assert_eq!(state.lanes["gemini"], LaneMark::default());
    }
}
