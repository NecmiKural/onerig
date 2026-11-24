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
    /// Only this lane may work on the task; no fallback to another lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// The task's words were typed by the owner, not written by the planner.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_owner: bool,
}

const OWNER_TASK_ID: &str = "owner-1";
const OWNER_ACCEPTANCE: &str = "What the owner asked for is done.";

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
        if task.lane.is_some() || task.from_owner {
            return Err(FormatError::BadTask(
                task.id.clone(),
                "only the owner can pin a lane or speak as the owner",
            ));
        }
        check_task(task)?;
        if !seen.insert(task.id.as_str()) {
            return Err(FormatError::DuplicateId(task.id.clone()));
        }
    }
    Ok(plan)
}

/// A one-task plan from text the owner typed for one lane (`@lane text`).
/// The title is the first non-blank line, cut to fit a commit subject.
pub fn owner_plan(lane: &str, text: &str) -> Result<Plan, FormatError> {
    let text = text.trim();
    let Some(first) = text.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return Err(FormatError::TaskCount(0));
    };
    let task = Task {
        id: OWNER_TASK_ID.to_string(),
        title: first.chars().take(MAX_TITLE_CHARS).collect(),
        instructions: text.to_string(),
        acceptance: vec![OWNER_ACCEPTANCE.to_string()],
        lane: Some(lane.to_string()),
        from_owner: true,
    };
    check_task(&task)?;
    Ok(Plan { tasks: vec![task] })
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
pub(crate) fn is_valid_id(id: &str) -> bool {
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

    #[test]
    fn a_planner_can_neither_pin_a_lane_nor_claim_the_owners_words() {
        let pinned = with(task("a"), "lane", json!("w1"));
        let claimed = with(task("b"), "from_owner", json!(true));

        let pinned = parse_plan(&plan_of(vec![pinned])).unwrap_err();
        let claimed = parse_plan(&plan_of(vec![claimed])).unwrap_err();

        assert_eq!(
            pinned,
            FormatError::BadTask(
                "a".into(),
                "only the owner can pin a lane or speak as the owner"
            )
        );
        assert!(matches!(claimed, FormatError::BadTask(id, _) if id == "b"));
    }

    #[test]
    fn an_owner_task_is_pinned_to_its_lane_and_carries_the_owners_words() {
        let plan = owner_plan("w2", "\n  Fix the typo in README  \nIt says helo.").unwrap();

        assert_eq!(
            plan.tasks,
            [Task {
                id: "owner-1".into(),
                title: "Fix the typo in README".into(),
                instructions: "Fix the typo in README  \nIt says helo.".into(),
                acceptance: vec!["What the owner asked for is done.".into()],
                lane: Some("w2".into()),
                from_owner: true,
            }]
        );
    }

    #[test]
    fn an_owner_title_is_the_first_line_cut_at_100_characters() {
        let long = "é".repeat(150);

        let plan = owner_plan("w1", &long).unwrap();

        assert_eq!(plan.tasks[0].title, "é".repeat(100));
        assert_eq!(owner_plan("w1", " \n\t "), Err(FormatError::TaskCount(0)));
    }

    #[test]
    fn a_plan_without_owner_fields_keeps_the_version_1_shape() {
        let plan = parse_plan(&plan_of(vec![task("a")])).unwrap();

        let written = serde_json::to_value(&plan).unwrap();

        assert_eq!(written, plan_of(vec![task("a")]));
        assert_eq!(plan.tasks[0].lane, None);
        assert!(!plan.tasks[0].from_owner);
    }
}
