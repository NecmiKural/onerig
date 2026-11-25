//! Harness adapters. An adapter knows one harness's command line and output
//! format. Every method is pure: it starts nothing and reads nothing, so an
//! adapter is tested with captured output alone.

use std::path::PathBuf;

use serde_json::Value;

use crate::prompt::{PLANNER_ROLE, REVIEWER_ROLE};
use crate::runner::{
    FailureKind, HarnessEvent, Job, Outcome, Probe, ProbeError, QuotaReport, Role,
};

pub(crate) fn role_prompt(role: Role) -> Option<&'static str> {
    match role {
        Role::Planner => Some(PLANNER_ROLE),
        Role::Reviewer => Some(REVIEWER_ROLE),
        Role::Worker => None,
    }
}

pub(crate) fn prepend_role(role: Role, prompt: &str) -> String {
    match role_prompt(role) {
        Some(prefix) => format!("{prefix}\n\n{prompt}"),
        None => prompt.to_string(),
    }
}

pub(crate) fn json_lines(stdout: &[String]) -> Vec<Value> {
    stdout
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

pub(crate) fn exit_message(prefix: &str, output: &Output) -> String {
    let exit = output
        .exit_code
        .map_or("a signal".to_string(), |code| format!("exit code {code}"));
    let tail = output.stderr_tail.trim();
    if tail.is_empty() {
        format!("{prefix} ({exit})")
    } else {
        format!("{prefix} ({exit}): {tail}")
    }
}

pub(crate) fn classify_error_text(
    quota_signs: &[&str],
    auth_signs: &[&str],
    message: String,
) -> Outcome {
    let lower = message.to_lowercase();
    let kind = if quota_signs.iter().any(|sign| lower.contains(sign)) {
        FailureKind::Quota
    } else if auth_signs.iter().any(|sign| lower.contains(sign)) {
        FailureKind::Auth
    } else {
        FailureKind::Unknown
    };
    Outcome::failed(kind, message)
}

/// A subprocess call, described but not started.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Spawn {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env_set: Vec<(String, String)>,
    /// Variable names to remove; a trailing `*` removes every variable with that prefix.
    pub env_remove: Vec<String>,
    pub stdin: Option<String>,
    /// Files the driver writes before it starts the process.
    pub files: Vec<(PathBuf, String)>,
}

/// Everything a finished process produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Output {
    pub exit_code: Option<i32>,
    pub stdout: Vec<String>,
    pub stderr_tail: String,
}

/// One value per lane, built from that lane's configuration.
/// `Send` so a runner holding adapters can work on a background thread.
pub trait Adapter: Send {
    fn probe_spawns(&self) -> Vec<Spawn>;
    fn probe_result(&self, outputs: &[Output]) -> Result<Probe, ProbeError>;
    fn quota_spawn(&self) -> Option<Spawn>;
    fn quota_result(&self, output: &Output) -> Option<QuotaReport>;
    fn spawn(&self, job: &Job) -> Spawn;
    /// One stdout line to zero or more events. Unknown lines yield nothing.
    fn parse(&self, line: &str) -> Vec<HarnessEvent>;
    /// The outcome of a job that ended by itself.
    fn outcome(&self, job: &Job, output: &Output) -> Outcome;
}

pub mod agy;
pub mod claude;
pub mod codex;

/// Extracts a JSON object from raw or fenced/prose-wrapped model output.
pub fn extract_json_object(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
        && value.is_object()
    {
        return Some(value);
    }
    trimmed.match_indices('{').find_map(|(start, _)| {
        serde_json::Deserializer::from_str(&trimmed[start..])
            .into_iter::<serde_json::Value>()
            .next()?
            .ok()
            .filter(serde_json::Value::is_object)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_json_object_handles_raw_fenced_and_prose_wrapped_objects() {
        assert_eq!(
            extract_json_object(r#" {"a": 1} "#),
            Some(json!({ "a": 1 }))
        );
        assert_eq!(
            extract_json_object("Here is the plan:\n```json\n{\"tasks\": []}\n```\nDone {note}."),
            Some(json!({ "tasks": [] }))
        );
        assert_eq!(extract_json_object("[1, 2, 3]"), None);
        assert_eq!(extract_json_object("no json here {broken}"), None);
    }
}
