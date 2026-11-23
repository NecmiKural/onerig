//! The plan a planner returns and the verdict a reviewer returns, with the
//! checks that decide whether a harness's answer is usable.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

pub const MAX_TASKS: usize = 20;
const MAX_ID_CHARS: usize = 32;
const MAX_TITLE_CHARS: usize = 100;

/// JSON Schema text handed to a harness that supports structured output.
/// It only shapes the answer; `parse_plan` is what decides.
pub const PLAN_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "tasks": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "id": { "type": "string", "description": "lowercase letters, digits, '-' and '_'; at most 32 characters; unique" },
          "title": { "type": "string", "description": "one line, at most 100 characters" },
          "instructions": { "type": "string" },
          "acceptance": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["id", "title", "instructions", "acceptance"]
      }
    }
  },
  "required": ["tasks"]
}"#;

/// JSON Schema text handed to a harness that supports structured output.
/// It only shapes the answer; `parse_verdict` is what decides.
pub const VERDICT_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "verdict": { "type": "string", "enum": ["approve", "revise"] },
    "summary": { "type": "string" },
    "issues": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "file": { "type": "string" },
          "note": { "type": "string" }
        },
        "required": ["file", "note"]
      }
    }
  },
  "required": ["verdict", "summary"]
}"#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub instructions: String,
    pub acceptance: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    Approve,
    Revise,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub file: String,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub verdict: VerdictKind,
    pub summary: String,
    #[serde(default)]
    pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    #[error("the result does not have the expected shape: {0}")]
    Shape(String),
    #[error("a plan needs 1 to {MAX_TASKS} tasks, got {0}")]
    TaskCount(usize),
    #[error(
        "task id {0:?} is not valid: use lowercase letters, digits, '-' and '_', at most {MAX_ID_CHARS} characters"
    )]
    BadId(String),
    #[error("task id {0:?} is used twice")]
    DuplicateId(String),
    #[error("task {0:?}: {1}")]
    BadTask(String, &'static str),
    #[error("a revise verdict needs at least one issue")]
    ReviseWithoutIssues,
}

/// Turn a planner's structured answer into a plan, or say why it is unusable.
pub fn parse_plan(value: &serde_json::Value) -> Result<Plan, FormatError> {
    let plan = Plan::deserialize(value).map_err(|e| FormatError::Shape(e.to_string()))?;
    if plan.tasks.is_empty() || plan.tasks.len() > MAX_TASKS {
        return Err(FormatError::TaskCount(plan.tasks.len()));
    }
    let mut seen = HashSet::new();
    for task in &plan.tasks {
        check_task(task)?;
        if !seen.insert(task.id.as_str()) {
            return Err(FormatError::DuplicateId(task.id.clone()));
        }
    }
    Ok(plan)
}

/// Turn a reviewer's structured answer into a verdict, or say why it is unusable.
pub fn parse_verdict(value: &serde_json::Value) -> Result<Verdict, FormatError> {
    let verdict = Verdict::deserialize(value).map_err(|e| FormatError::Shape(e.to_string()))?;
    if verdict.verdict == VerdictKind::Revise && verdict.issues.is_empty() {
        return Err(FormatError::ReviseWithoutIssues);
    }
    Ok(verdict)
}

fn check_task(task: &Task) -> Result<(), FormatError> {
    if !is_valid_id(&task.id) {
        return Err(FormatError::BadId(task.id.clone()));
    }
    let problem = if !is_valid_title(&task.title) {
        "the title must be one non-empty line of at most 100 characters"
    } else if task.instructions.trim().is_empty() {
        "the instructions are empty"
    } else if task.acceptance.is_empty() || task.acceptance.iter().any(|a| a.trim().is_empty()) {
        "at least one acceptance item is needed and none may be empty"
    } else {
        return Ok(());
    };
    Err(FormatError::BadTask(task.id.clone(), problem))
}

/// The id ends up in file names and log lines, so it is kept to a safe alphabet.
fn is_valid_id(id: &str) -> bool {
    let is_first = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let is_rest = |c: char| is_first(c) || c == '-' || c == '_';
    id.len() <= MAX_ID_CHARS && id.starts_with(is_first) && id.chars().all(is_rest)
}

/// The title becomes a commit subject.
fn is_valid_title(title: &str) -> bool {
    !title.trim().is_empty()
        && !title.contains(['\n', '\r'])
        && title.chars().count() <= MAX_TITLE_CHARS
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn task(id: &str) -> Value {
        json!({
            "id": id,
            "title": "Add retry to the HTTP client",
            "instructions": "Wrap send() in a retry loop.",
            "acceptance": ["Requests are retried three times on 5xx"]
        })
    }

    fn plan_of(tasks: Vec<Value>) -> Value {
        json!({ "tasks": tasks })
    }

    fn with(mut task: Value, key: &str, value: Value) -> Value {
        task[key] = value;
        task
    }

    #[test]
    fn accepts_a_valid_plan() {
        let plan = parse_plan(&plan_of(vec![task("retry-helper"), task("wire_in2")])).unwrap();

        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[0].id, "retry-helper");
        assert_eq!(plan.tasks[1].acceptance.len(), 1);
    }

    #[test]
    fn rejects_a_result_without_tasks() {
        let error = parse_plan(&json!({ "plan": [] })).unwrap_err();

        assert!(matches!(error, FormatError::Shape(_)));
    }

    #[test]
    fn rejects_a_plan_with_no_tasks_or_too_many() {
        let too_many = (0..=MAX_TASKS).map(|i| task(&format!("t{i}"))).collect();

        assert_eq!(parse_plan(&plan_of(vec![])), Err(FormatError::TaskCount(0)));
        assert_eq!(
            parse_plan(&plan_of(too_many)),
            Err(FormatError::TaskCount(MAX_TASKS + 1))
        );
    }

    #[test]
    fn accepts_the_largest_allowed_plan_and_id() {
        let tasks = (0..MAX_TASKS).map(|i| task(&format!("t{i}"))).collect();
        let longest_id = "a".repeat(32);

        assert!(parse_plan(&plan_of(tasks)).is_ok());
        assert!(parse_plan(&plan_of(vec![task(&longest_id)])).is_ok());
    }

    #[test]
    fn rejects_ids_that_are_not_safe_in_file_names() {
        for id in [
            "",
            "Retry",
            "-lead",
            "_lead",
            "a b",
            "a/b",
            "é",
            &"a".repeat(33),
        ] {
            assert_eq!(
                parse_plan(&plan_of(vec![task(id)])),
                Err(FormatError::BadId(id.to_string())),
                "id {id:?}"
            );
        }
    }

    #[test]
    fn rejects_an_id_used_twice() {
        let error = parse_plan(&plan_of(vec![task("a"), task("b"), task("a")])).unwrap_err();

        assert_eq!(error, FormatError::DuplicateId("a".into()));
    }

    #[test]
    fn rejects_a_title_that_is_empty_long_or_multi_line() {
        for title in [
            "",
            "   ",
            "two\nlines",
            "carriage\rreturn",
            &"x".repeat(101),
        ] {
            let bad = with(task("a"), "title", json!(title));
            assert!(
                matches!(parse_plan(&plan_of(vec![bad])), Err(FormatError::BadTask(id, _)) if id == "a"),
                "title {title:?}"
            );
        }
        let longest = with(task("a"), "title", json!("ü".repeat(100)));
        assert!(parse_plan(&plan_of(vec![longest])).is_ok());
    }

    #[test]
    fn rejects_empty_instructions_and_missing_acceptance() {
        let no_instructions = with(task("a"), "instructions", json!(" \n"));
        let no_acceptance = with(task("a"), "acceptance", json!([]));
        let blank_acceptance = with(task("a"), "acceptance", json!(["ok", " "]));

        for bad in [no_instructions, no_acceptance, blank_acceptance] {
            assert!(matches!(
                parse_plan(&plan_of(vec![bad])),
                Err(FormatError::BadTask(_, _))
            ));
        }
    }

    #[test]
    fn accepts_an_approval_without_issues() {
        let verdict = parse_verdict(&json!({ "verdict": "approve", "summary": "Good." })).unwrap();

        assert_eq!(verdict.verdict, VerdictKind::Approve);
        assert!(verdict.issues.is_empty());
    }

    #[test]
    fn accepts_a_revise_with_issues() {
        let verdict = parse_verdict(&json!({
            "verdict": "revise",
            "summary": "One problem.",
            "issues": [{ "file": "src/http.rs", "note": "Retry also fires on 4xx." }]
        }))
        .unwrap();

        assert_eq!(verdict.verdict, VerdictKind::Revise);
        assert_eq!(verdict.issues[0].file, "src/http.rs");
    }

    #[test]
    fn rejects_a_revise_without_issues() {
        let error = parse_verdict(&json!({ "verdict": "revise", "summary": "Hm." })).unwrap_err();

        assert_eq!(error, FormatError::ReviseWithoutIssues);
    }

    #[test]
    fn rejects_an_unknown_verdict_word() {
        let error = parse_verdict(&json!({ "verdict": "lgtm", "summary": "" })).unwrap_err();

        assert!(matches!(error, FormatError::Shape(_)));
    }

    #[test]
    fn schemas_are_json_objects() {
        for schema in [PLAN_SCHEMA, VERDICT_SCHEMA] {
            let value: Value = serde_json::from_str(schema).unwrap();
            assert_eq!(value["type"], "object");
        }
    }
}
