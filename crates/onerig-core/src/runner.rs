//! The interface the run state machine uses to run jobs, and the data that
//! crosses it. The real runner combines an adapter with the process driver;
//! tests use a scripted one.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Planner,
    Worker,
    Reviewer,
}

/// One subprocess call of a harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub role: Role,
    pub prompt: String,
    /// The run's worktree.
    pub cwd: PathBuf,
    /// Where files a harness call needs are written.
    pub run_dir: PathBuf,
    /// Where the raw output of a failed job is kept.
    pub raw_log: PathBuf,
    /// JSON Schema text, when a structured result is wanted.
    pub schema: Option<&'static str>,
}

/// What a harness reports while a job runs, reduced to what onerig shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEvent {
    Started { model: Option<String> },
    Text { text: String },
    Tool { name: String, detail: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outcome {
    Done {
        text: String,
        structured: Option<serde_json::Value>,
    },
    Failed {
        kind: FailureKind,
        message: String,
        /// Unix milliseconds.
        reset_at: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The account's allowance is used up, or the provider refuses for load or balance.
    Quota,
    /// The harness says the user is not logged in.
    Auth,
    /// The driver killed the job.
    Timeout,
    /// Anything else, including a result the adapter does not recognise.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "type", content = "message", rename_all = "snake_case")]
pub enum ProbeError {
    #[error("{0}")]
    Missing(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    NotLoggedIn(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QuotaReport {
    pub remaining_fraction: f64,
    /// Unix milliseconds.
    pub reset_at: Option<u64>,
}

pub trait Runner {
    /// Check that a lane can run. Makes no model call.
    fn preflight(&mut self, lane: &str) -> Result<Probe, ProbeError>;
    /// Remaining quota of a lane, when its harness can report it for free.
    fn quota(&mut self, lane: &str) -> Option<QuotaReport>;
    /// Run one job to its end, reporting events as they arrive.
    fn run(&mut self, lane: &str, job: &Job, on_event: &mut dyn FnMut(HarnessEvent)) -> Outcome;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The run log and the test scripts both depend on these exact shapes.
    #[test]
    fn a_failed_outcome_is_written_the_way_the_log_documents_it() {
        let outcome = Outcome::Failed {
            kind: FailureKind::Quota,
            message: "balance is empty".into(),
            reset_at: None,
        };

        assert_eq!(
            serde_json::to_value(&outcome).unwrap(),
            json!({ "type": "failed", "kind": "quota", "message": "balance is empty", "reset_at": null })
        );
    }

    #[test]
    fn harness_events_are_tagged_with_a_snake_case_type() {
        let tool = HarnessEvent::Tool {
            name: "edit".into(),
            detail: "src/http.rs".into(),
        };
        let text: HarnessEvent =
            serde_json::from_value(json!({ "type": "text", "text": "hi" })).unwrap();

        assert_eq!(
            serde_json::to_value(&tool).unwrap(),
            json!({ "type": "tool", "name": "edit", "detail": "src/http.rs" })
        );
        assert_eq!(text, HarnessEvent::Text { text: "hi".into() });
    }
}
