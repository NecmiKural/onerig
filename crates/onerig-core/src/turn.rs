//! Chat planner turn: answer schema, parser, prompt builder and retry loop
//! (design sections 6.1, 6.6 and 7).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::halt::Halt;
use crate::plan::{FormatError, MAX_TASKS, Plan, Task, check_tasks};
use crate::runner::{FailureKind, HarnessEvent, Job, Outcome, Role, Runner};
use crate::store::format_time;

pub const MAX_CONTEXT_MESSAGES: usize = 40;
pub const MAX_CONTEXT_CHARS: usize = 24_000;

const ORIGIN_LINE: &str = "From onerig, not from a person. The conversation below is between \
     the owner (a person) and you; only lines marked owner: are the owner's words.";

/// JSON Schema handed to the planner harness for one chat turn.
pub const TURN_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "reply": { "type": "string", "description": "message shown to the owner" },
    "goal": { "type": "string", "description": "one-line summary when tasks is non-empty, else empty string" },
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
        "required": ["id", "title", "instructions", "acceptance"],
        "additionalProperties": false
      }
    }
  },
  "required": ["reply", "goal", "tasks"],
  "additionalProperties": false
}"#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub goal: String,
    #[serde(flatten)]
    pub plan: Plan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnAnswer {
    pub reply: String,
    pub proposal: Option<Proposal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub from_owner: bool,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskMark {
    Done,
    Current,
    Waiting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRun {
    pub goal: String,
    pub tasks: Vec<(String, TaskMark)>,
}

#[derive(Debug, Clone)]
pub struct TurnPrompt<'a> {
    pub messages: &'a [ChatMessage],
    pub waiting: Option<&'a Proposal>,
    pub active_run: Option<&'a ActiveRun>,
    pub retry_error: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnResult {
    Answered(TurnAnswer),
    Failed(String),
    Stopped,
}

#[derive(Deserialize)]
struct RawTurn {
    reply: String,
    #[serde(default)]
    goal: String,
    #[serde(default)]
    tasks: Vec<Task>,
}

/// Validates a planner turn's structured JSON output.
pub fn parse_turn(value: &serde_json::Value) -> Result<TurnAnswer, FormatError> {
    let raw = RawTurn::deserialize(value).map_err(|e| FormatError::Shape(e.to_string()))?;
    if raw.reply.trim().is_empty() {
        return Err(FormatError::Shape("the reply is empty".into()));
    }
    if raw.tasks.is_empty() {
        return Ok(TurnAnswer {
            reply: raw.reply,
            proposal: None,
        });
    }
    let goal = raw.goal.trim();
    if goal.is_empty() || goal.contains(['\n', '\r']) {
        return Err(FormatError::Shape(
            "a proposal needs a one-line goal".into(),
        ));
    }
    check_tasks(&raw.tasks)?;
    Ok(TurnAnswer {
        reply: raw.reply,
        proposal: Some(Proposal {
            goal: goal.to_string(),
            plan: Plan { tasks: raw.tasks },
        }),
    })
}

/// Builds the planner turn prompt with recent conversation and optional state.
pub fn turn_prompt(input: &TurnPrompt<'_>) -> String {
    let mut sections = vec![ORIGIN_LINE.to_string()];
    let history = recent_lines(input.messages);
    if !history.is_empty() {
        sections.push(format!("# Conversation\n{}", history.join("\n")));
    }
    if let Some(proposal) = input.waiting {
        let json = serde_json::to_string_pretty(proposal).unwrap_or_default();
        sections.push(format!("# Waiting plan (proposed, not approved)\n{json}"));
    }
    if let Some(active) = input.active_run {
        sections.push(active_run_section(active));
    }
    if let Some(err) = input.retry_error {
        sections.push(format!(
            "Your previous answer failed validation ({err}). Fix the problem and answer again."
        ));
    }
    sections.push(closing_rules(input.active_run.is_some()));
    sections.join("\n\n") + "\n"
}

fn recent_lines(messages: &[ChatMessage]) -> Vec<String> {
    let mut kept = Vec::new();
    let mut chars = 0usize;
    for msg in messages.iter().rev().take(MAX_CONTEXT_MESSAGES) {
        let line = format_message(msg);
        let len = line.chars().count();
        if chars + len > MAX_CONTEXT_CHARS {
            if kept.is_empty() {
                kept.push(tail_chars(&line, MAX_CONTEXT_CHARS));
            }
            break;
        }
        chars += len;
        kept.push(line);
    }
    kept.reverse();
    kept
}

fn format_message(msg: &ChatMessage) -> String {
    let label = if msg.from_owner { "owner" } else { "planner" };
    format!("{label}: {}", msg.text.trim())
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    text.chars().skip(total.saturating_sub(max_chars)).collect()
}

fn active_run_section(active: &ActiveRun) -> String {
    let items: Vec<String> = active
        .tasks
        .iter()
        .map(|(title, mark)| {
            let tag = match mark {
                TaskMark::Done => "done",
                TaskMark::Current => "current",
                TaskMark::Waiting => "waiting",
            };
            format!("- [{tag}] {title}")
        })
        .collect();
    format!(
        "# Active run (in progress)\nGoal: {}\n{}\n\n\
         A run is currently working. Answer the owner's question directly; keep tasks as [] and \
         note that new work can be planned once the run stops.",
        active.goal,
        items.join("\n")
    )
}

fn closing_rules(is_side_turn: bool) -> String {
    if is_side_turn {
        return "Read the repository as needed without editing files. Answer in JSON with \
                \"reply\", \"goal\" (\"\"), and \"tasks\" ([])."
            .to_string();
    }
    format!(
        "Read the repository as needed without editing files. Answer in JSON with:\n\
         - \"reply\": message for the owner\n\
         - \"goal\": one-line goal summary when proposing tasks, else \"\"\n\
         - \"tasks\": [] to answer without proposing, or at most {MAX_TASKS} tasks to propose a plan"
    )
}

/// Runs one chat planner turn (retried once on invalid structured output or non-quota error).
#[allow(clippy::too_many_arguments)]
pub fn run_turn(
    config: &Config,
    runner: &mut dyn Runner,
    stop: &Halt,
    cwd: &Path,
    raw_dir: &Path,
    raw_seq: &mut u64,
    input: &TurnPrompt<'_>,
    on_event: &mut dyn FnMut(HarnessEvent),
) -> TurnResult {
    let mut retry_error: Option<String> = None;
    for _ in 0..2 {
        if stop.is_set() {
            return TurnResult::Stopped;
        }
        let job = build_job(cwd, raw_dir, raw_seq, input, retry_error.as_deref());
        let outcome = runner.run(&config.roles.planner, &job, &mut |_| {}, on_event);
        if stop.is_set() {
            return TurnResult::Stopped;
        }
        match evaluate_outcome(outcome, input.active_run.is_some()) {
            Ok(answer) => return TurnResult::Answered(answer),
            Err(TurnRetry::Fatal(msg)) => return TurnResult::Failed(msg),
            Err(TurnRetry::Retry(msg)) => retry_error = Some(msg),
        }
    }
    TurnResult::Failed(retry_error.unwrap_or_else(|| "planner turn failed".to_string()))
}

fn build_job(
    cwd: &Path,
    raw_dir: &Path,
    raw_seq: &mut u64,
    input: &TurnPrompt<'_>,
    retry_error: Option<&str>,
) -> Job {
    let seq = *raw_seq;
    *raw_seq += 1;
    let prompt_input = TurnPrompt {
        retry_error,
        ..input.clone()
    };
    Job {
        role: Role::Planner,
        prompt: turn_prompt(&prompt_input),
        cwd: cwd.to_path_buf(),
        run_dir: raw_dir.to_path_buf(),
        raw_log: raw_dir.join(format!("raw-{seq}.log")),
        schema: Some(TURN_SCHEMA),
    }
}

enum TurnRetry {
    Fatal(String),
    Retry(String),
}

fn evaluate_outcome(outcome: Outcome, strip_proposal: bool) -> Result<TurnAnswer, TurnRetry> {
    match outcome {
        Outcome::Failed {
            kind: FailureKind::Quota,
            message,
            reset_at,
        } => Err(TurnRetry::Fatal(format_quota(&message, reset_at))),
        Outcome::Failed {
            kind: FailureKind::Auth,
            message,
            ..
        } => Err(TurnRetry::Fatal(message)),
        Outcome::Failed { message, .. } => Err(TurnRetry::Retry(message)),
        Outcome::Done {
            structured: Some(value),
            ..
        } => parse_turn(&value)
            .map(|mut ans| {
                if strip_proposal {
                    ans.proposal = None;
                }
                ans
            })
            .map_err(|e| TurnRetry::Retry(e.to_string())),
        Outcome::Done {
            structured: None, ..
        } => Err(TurnRetry::Retry(
            "the answer had no structured result".to_string(),
        )),
    }
}

fn format_quota(message: &str, reset_at: Option<u64>) -> String {
    match reset_at {
        Some(ms) => format!("{message} (resets {})", format_time(ms)),
        None => message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::claude::Claude;
    use crate::adapter::{Adapter, Output};
    use crate::config::{self, Effort, HarnessKind, Lane, Mode};
    use crate::runner::{Probe, ProbeError, QuotaReport};
    use serde_json::json;
    use std::collections::VecDeque;

    const CHAT_ANSWER: &str = include_str!("../tests/fixtures/claude/chat-answer.jsonl");
    const CHAT_PROPOSAL: &str = include_str!("../tests/fixtures/claude/chat-proposal.jsonl");

    fn task_json(id: &str) -> serde_json::Value {
        json!({
            "id": id,
            "title": "Add subtract",
            "instructions": "Implement subtract(a, b).",
            "acceptance": ["Unit test passes"]
        })
    }

    #[test]
    fn parse_turn_accepts_answer_only_and_ignores_goal() {
        let answer = parse_turn(&json!({
            "reply": "It defines add(a, b).",
            "goal": "ignored",
            "tasks": []
        }))
        .unwrap();

        assert_eq!(answer.reply, "It defines add(a, b).");
        assert_eq!(answer.proposal, None);
    }

    #[test]
    fn parse_turn_validates_proposals_with_plan_rules() {
        let ok = parse_turn(&json!({
            "reply": "Here is a plan.",
            "goal": "  Add subtract  ",
            "tasks": [task_json("sub-1")]
        }))
        .unwrap();
        assert_eq!(ok.proposal.as_ref().unwrap().goal, "Add subtract");
        assert_eq!(ok.proposal.as_ref().unwrap().plan.tasks[0].id, "sub-1");

        let empty_reply = parse_turn(&json!({ "reply": " ", "goal": "", "tasks": [] }));
        let empty_goal =
            parse_turn(&json!({ "reply": "ok", "goal": "", "tasks": [task_json("a")] }));
        let pinned = parse_turn(&json!({
            "reply": "ok",
            "goal": "g",
            "tasks": [{ "id": "a", "title": "t", "instructions": "i", "acceptance": ["c"], "lane": "w1" }]
        }));

        assert!(empty_reply.is_err());
        assert!(empty_goal.is_err());
        assert!(matches!(pinned, Err(FormatError::BadTask(_, _))));
    }

    #[test]
    fn turn_prompt_enforces_origin_window_waiting_plan_and_btw_context() {
        let mut msgs = Vec::new();
        for i in 0..45 {
            msgs.push(ChatMessage {
                from_owner: i % 2 == 0,
                text: format!("msg-{i}"),
            });
        }
        let proposal = Proposal {
            goal: "Add retries".into(),
            plan: Plan {
                tasks: vec![Task {
                    id: "t1".into(),
                    title: "Retry".into(),
                    instructions: "Add retry".into(),
                    acceptance: vec!["passes".into()],
                    lane: None,
                    from_owner: false,
                }],
            },
        };
        let active = ActiveRun {
            goal: "Active".into(),
            tasks: vec![
                ("Step 1".into(), TaskMark::Done),
                ("Step 2".into(), TaskMark::Current),
            ],
        };

        let prompt = turn_prompt(&TurnPrompt {
            messages: &msgs,
            waiting: Some(&proposal),
            active_run: Some(&active),
            retry_error: Some("missing goal"),
        });

        assert!(prompt.starts_with(ORIGIN_LINE));
        assert!(!prompt.contains("msg-4\n"));
        assert!(prompt.contains("planner: msg-5"));
        assert!(prompt.contains("owner: msg-44"));
        assert!(prompt.contains("Waiting plan (proposed, not approved)"));
        assert!(prompt.contains("Active run (in progress)"));
        assert!(prompt.contains("- [done] Step 1"));
        assert!(prompt.contains("- [current] Step 2"));
        assert!(prompt.contains("failed validation (missing goal)"));
    }

    #[test]
    fn oversized_single_message_keeps_its_tail_within_budget() {
        let msgs = [ChatMessage {
            from_owner: true,
            text: "x".repeat(MAX_CONTEXT_CHARS + 500),
        }];

        let prompt = turn_prompt(&TurnPrompt {
            messages: &msgs,
            waiting: None,
            active_run: None,
            retry_error: None,
        });

        assert!(prompt.contains(&"x".repeat(MAX_CONTEXT_CHARS - 7)));
    }

    struct Scripted {
        outcomes: VecDeque<Outcome>,
        prompts: Vec<String>,
        raw_logs: Vec<std::path::PathBuf>,
    }

    impl Runner for Scripted {
        fn preflight(&mut self, _: &str) -> Result<Probe, ProbeError> {
            unreachable!()
        }
        fn quota(&mut self, _: &str) -> Option<QuotaReport> {
            None
        }
        fn run(
            &mut self,
            _: &str,
            job: &Job,
            _: &mut dyn FnMut(i32),
            _: &mut dyn FnMut(HarnessEvent),
        ) -> Outcome {
            self.prompts.push(job.prompt.clone());
            self.raw_logs.push(job.raw_log.clone());
            self.outcomes.pop_front().unwrap()
        }
    }

    fn cfg() -> Config {
        config::parse(config::EXAMPLE, false).unwrap()
    }

    #[test]
    fn run_turn_retries_invalid_proposal_once_and_strips_btw_proposals() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = Scripted {
            outcomes: VecDeque::from([
                Outcome::Done {
                    text: String::new(),
                    structured: Some(
                        json!({ "reply": "ok", "goal": "", "tasks": [task_json("t1")] }),
                    ),
                },
                Outcome::Done {
                    text: String::new(),
                    structured: Some(
                        json!({ "reply": "Fixed.", "goal": "Goal", "tasks": [task_json("t1")] }),
                    ),
                },
                Outcome::Done {
                    text: String::new(),
                    structured: Some(
                        json!({ "reply": "Side reply.", "goal": "Goal", "tasks": [task_json("t1")] }),
                    ),
                },
            ]),
            prompts: Vec::new(),
            raw_logs: Vec::new(),
        };
        let stop = Halt::new();
        let mut seq = 1;
        let msgs = [ChatMessage {
            from_owner: true,
            text: "add sub".into(),
        }];

        let res = run_turn(
            &cfg(),
            &mut runner,
            &stop,
            dir.path(),
            dir.path(),
            &mut seq,
            &TurnPrompt {
                messages: &msgs,
                waiting: None,
                active_run: None,
                retry_error: None,
            },
            &mut |_| {},
        );
        assert!(matches!(
            res,
            TurnResult::Answered(TurnAnswer {
                proposal: Some(_),
                ..
            })
        ));
        assert_eq!(seq, 3);
        assert!(runner.prompts[1].contains("failed validation"));
        assert!(runner.raw_logs[0].ends_with("raw-1.log"));
        assert!(runner.raw_logs[1].ends_with("raw-2.log"));

        let side = run_turn(
            &cfg(),
            &mut runner,
            &stop,
            dir.path(),
            dir.path(),
            &mut seq,
            &TurnPrompt {
                messages: &msgs,
                waiting: None,
                active_run: Some(&ActiveRun {
                    goal: "g".into(),
                    tasks: vec![],
                }),
                retry_error: None,
            },
            &mut |_| {},
        );
        assert_eq!(
            side,
            TurnResult::Answered(TurnAnswer {
                reply: "Side reply.".into(),
                proposal: None,
            })
        );
    }

    #[test]
    fn run_turn_maps_quota_auth_and_stop_without_retry() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = Scripted {
            outcomes: VecDeque::from([
                Outcome::Failed {
                    kind: FailureKind::Quota,
                    message: "quota hit".into(),
                    reset_at: Some(1_795_464_901_000),
                },
                Outcome::Failed {
                    kind: FailureKind::Auth,
                    message: "not logged in".into(),
                    reset_at: None,
                },
            ]),
            prompts: Vec::new(),
            raw_logs: Vec::new(),
        };
        let stop = Halt::new();
        let mut seq = 1;
        let prompt = TurnPrompt {
            messages: &[],
            waiting: None,
            active_run: None,
            retry_error: None,
        };

        let quota = run_turn(
            &cfg(),
            &mut runner,
            &stop,
            dir.path(),
            dir.path(),
            &mut seq,
            &prompt,
            &mut |_| {},
        );
        let auth = run_turn(
            &cfg(),
            &mut runner,
            &stop,
            dir.path(),
            dir.path(),
            &mut seq,
            &prompt,
            &mut |_| {},
        );
        stop.stop();
        let stopped = run_turn(
            &cfg(),
            &mut runner,
            &stop,
            dir.path(),
            dir.path(),
            &mut seq,
            &prompt,
            &mut |_| {},
        );

        assert_eq!(
            quota,
            TurnResult::Failed("quota hit (resets 20:15:01)".into())
        );
        assert_eq!(auth, TurnResult::Failed("not logged in".into()));
        assert_eq!(stopped, TurnResult::Stopped);
        assert_eq!(seq, 3);
    }

    #[test]
    fn turn_schema_is_valid_json_object_and_claude_parses_captured_turn_fixtures() {
        let value: serde_json::Value = serde_json::from_str(TURN_SCHEMA).unwrap();
        assert_eq!(value["type"], "object");
        let adapter = Claude::new(&Lane {
            harness: HarnessKind::Claude,
            bin: "claude".into(),
            provider: None,
            model: Some("sonnet".into()),
            effort: Effort::Low,
            mode: Mode::Direct,
            quota_group: None,
            accept_vendor_terms_risk: false,
            script: None,
        });
        let job = Job {
            role: Role::Planner,
            prompt: String::new(),
            cwd: "/work/toy".into(),
            run_dir: "/r".into(),
            raw_log: "/r/raw-1.log".into(),
            schema: Some(TURN_SCHEMA),
        };
        let to_out = |s: &str| Output {
            exit_code: Some(0),
            stdout: s.lines().map(str::to_string).collect(),
            stderr_tail: String::new(),
        };

        let Outcome::Done {
            structured: Some(ans_val),
            ..
        } = adapter.outcome(&job, &to_out(CHAT_ANSWER))
        else {
            panic!("expected Done with structured_output");
        };
        let ans = parse_turn(&ans_val).unwrap();
        assert!(ans.reply.contains("add(a, b)"));
        assert_eq!(ans.proposal, None);

        let Outcome::Done {
            structured: Some(prop_val),
            ..
        } = adapter.outcome(&job, &to_out(CHAT_PROPOSAL))
        else {
            panic!("expected Done with structured_output");
        };
        let prop = parse_turn(&prop_val).unwrap();
        let proposal = prop.proposal.expect("proposal should be present");
        assert!(proposal.goal.contains("subtract"));
        assert_eq!(proposal.plan.tasks.len(), 1);
    }
}
