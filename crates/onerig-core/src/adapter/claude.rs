//! Claude Code (`claude -p`) as planner and reviewer (design section 9.1).
//! The output shapes are those captured in `tests/fixtures/claude/`.

use serde_json::Value;

use super::{Adapter, Output, Spawn};
use crate::config::{Effort, Lane};
use crate::prompt::{PLANNER_ROLE, REVIEWER_ROLE};
use crate::runner::{
    FailureKind, HarnessEvent, Job, Outcome, Probe, ProbeError, QuotaReport, Role,
};

/// Variables that would make a child act as part of another Claude Code
/// session, or send it to a custom endpoint instead of the user's login.
const REMOVED_ENV: [&str; 4] = [
    "CLAUDECODE",
    "AI_AGENT",
    "CLAUDE_CODE_ENTRYPOINT",
    "ANTHROPIC_*",
];

/// Tool input fields that name what a tool worked on, in order of preference.
const TOOL_TARGETS: [&str; 4] = ["file_path", "pattern", "command", "path"];

/// The tool through which Claude delivers a `--json-schema` answer.
const ANSWER_TOOL: &str = "StructuredOutput";

const AUTH_SIGNS: [&str; 4] = [
    "not logged in",
    "/login",
    "invalid api key",
    "oauth token has expired",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claude {
    bin: String,
    model: String,
    effort: Effort,
}

impl Claude {
    pub fn new(lane: &Lane) -> Claude {
        Claude {
            bin: lane.bin.clone(),
            model: lane.model.clone().unwrap_or_default(),
            effort: lane.effort,
        }
    }

    fn command(&self, args: &[&str]) -> Spawn {
        Spawn {
            program: self.bin.clone().into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env_remove: REMOVED_ENV.iter().map(|v| v.to_string()).collect(),
            ..Spawn::default()
        }
    }
}

fn effort_word(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
    }
}

fn role_prompt(role: Role) -> Option<&'static str> {
    match role {
        Role::Planner => Some(PLANNER_ROLE),
        Role::Reviewer => Some(REVIEWER_ROLE),
        Role::Worker => None,
    }
}

impl Adapter for Claude {
    fn probe_spawns(&self) -> Vec<Spawn> {
        vec![
            self.command(&["--version"]),
            self.command(&["auth", "status", "--json"]),
        ]
    }

    fn probe_result(&self, outputs: &[Output]) -> Result<Probe, ProbeError> {
        let [version, auth] = outputs else {
            return Err(ProbeError::Unsupported(
                "unexpected probe outputs".to_string(),
            ));
        };
        if version.exit_code != Some(0) {
            return Err(ProbeError::Unsupported(format!(
                "`{} --version` failed",
                self.bin
            )));
        }
        // The status also holds the account's email and organisation; only
        // the login flag is read, and nothing of it is kept.
        let logged_in = serde_json::from_str::<Value>(&auth.stdout.join("\n"))
            .is_ok_and(|status| status["loggedIn"] == Value::Bool(true));
        if !logged_in {
            return Err(ProbeError::NotLoggedIn(format!(
                "Claude Code is not logged in; run `{} auth login`",
                self.bin
            )));
        }
        Ok(Probe {
            version: version.stdout.first().cloned().unwrap_or_default(),
        })
    }

    fn quota_spawn(&self) -> Option<Spawn> {
        None
    }

    fn quota_result(&self, _output: &Output) -> Option<QuotaReport> {
        None
    }

    fn spawn(&self, job: &Job) -> Spawn {
        let mut args = vec![
            "-p",
            "--safe-mode",
            "--model",
            &self.model,
            "--effort",
            effort_word(self.effort),
            "--output-format",
            "stream-json",
            "--verbose",
        ];
        if let Some(schema) = job.schema {
            args.extend(["--json-schema", schema]);
        }
        args.extend(["--permission-mode", "dontAsk", "--no-session-persistence"]);
        if let Some(role) = role_prompt(job.role) {
            args.extend(["--append-system-prompt", role]);
        }
        args.extend([
            "--tools",
            "Read",
            "Glob",
            "Grep",
            "--disallowedTools",
            "AskUserQuestion",
        ]);
        Spawn {
            stdin: Some(job.prompt.clone()),
            ..self.command(&args)
        }
    }

    fn parse(&self, line: &str) -> Vec<HarnessEvent> {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        match (value["type"].as_str(), value["subtype"].as_str()) {
            (Some("system"), Some("init")) => vec![HarnessEvent::Started {
                model: value["model"].as_str().map(str::to_string),
            }],
            (Some("assistant"), _) => content_events(&value["message"]["content"]),
            _ => Vec::new(),
        }
    }

    fn outcome(&self, job: &Job, output: &Output) -> Outcome {
        let lines: Vec<Value> = output
            .stdout
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if let Some(outcome) = rejected_limit(&lines) {
            return outcome;
        }
        match lines.iter().rev().find(|v| v["type"] == "result") {
            Some(result) => from_result(job, result),
            None => without_result(output),
        }
    }
}

fn content_events(content: &Value) -> Vec<HarnessEvent> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| match block["type"].as_str() {
            Some("text") => Some(HarnessEvent::Text {
                text: block["text"].as_str()?.to_string(),
            }),
            Some("tool_use") if block["name"] != ANSWER_TOOL => Some(HarnessEvent::Tool {
                name: block["name"].as_str()?.to_string(),
                detail: tool_target(&block["input"]),
            }),
            _ => None,
        })
        .collect()
}

fn tool_target(input: &Value) -> String {
    TOOL_TARGETS
        .iter()
        .find_map(|key| input[*key].as_str())
        .unwrap_or_default()
        .to_string()
}

/// A rate-limit event with status `rejected` means the plan's limit is used up.
fn rejected_limit(lines: &[Value]) -> Option<Outcome> {
    let info = lines
        .iter()
        .filter(|v| v["type"] == "rate_limit_event")
        .map(|v| &v["rate_limit_info"])
        .find(|info| info["status"] == "rejected")?;
    let window = info["rateLimitType"].as_str().unwrap_or("usage");
    Some(Outcome::Failed {
        kind: FailureKind::Quota,
        message: format!("Claude {window} limit reached"),
        reset_at: info["resetsAt"].as_u64().map(|seconds| seconds * 1000),
    })
}

fn from_result(job: &Job, result: &Value) -> Outcome {
    let text = result["result"].as_str().unwrap_or_default().to_string();
    if result["subtype"] == "success" && result["is_error"] == Value::Bool(false) {
        let structured = Some(result["structured_output"].clone()).filter(|v| !v.is_null());
        if job.schema.is_some() && structured.is_none() {
            return Outcome::failed(
                FailureKind::Unknown,
                "Claude gave no structured answer".to_string(),
            );
        }
        return Outcome::Done { text, structured };
    }
    let message = if text.is_empty() {
        format!(
            "Claude stopped: {}",
            result["subtype"].as_str().unwrap_or("error")
        )
    } else {
        text
    };
    if result["api_error_status"] == 429 {
        return Outcome::failed(FailureKind::Quota, message);
    }
    classify_message(message)
}

fn without_result(output: &Output) -> Outcome {
    let tail = output.stderr_tail.trim();
    let exit = output
        .exit_code
        .map_or("a signal".to_string(), |code| format!("exit code {code}"));
    let message = if tail.is_empty() {
        format!("claude ended without a result ({exit})")
    } else {
        format!("claude ended without a result ({exit}): {tail}")
    };
    classify_message(message)
}

fn classify_message(message: String) -> Outcome {
    let lower = message.to_lowercase();
    if AUTH_SIGNS.iter().any(|sign| lower.contains(sign)) {
        return Outcome::failed(
            FailureKind::Auth,
            format!(
                "{message} (if Claude Code was just updated, print mode may no longer use the \
                 subscription login)"
            ),
        );
    }
    Outcome::failed(FailureKind::Unknown, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HarnessKind, Mode};
    use crate::plan::{PLAN_SCHEMA, VERDICT_SCHEMA, VerdictKind, parse_plan, parse_verdict};
    use serde_json::json;

    const PLAN: &str = include_str!("../../tests/fixtures/claude/plan.jsonl");
    const REVIEW: &str = include_str!("../../tests/fixtures/claude/review.jsonl");

    fn claude() -> Claude {
        Claude::new(&Lane {
            harness: HarnessKind::Claude,
            bin: "claude".into(),
            model: Some("sonnet".into()),
            effort: Effort::Low,
            mode: Mode::Direct,
            quota_group: None,
            accept_vendor_terms_risk: false,
            script: None,
        })
    }

    fn job(role: Role, schema: Option<&'static str>) -> Job {
        Job {
            role,
            prompt: "the prompt".into(),
            cwd: "/w".into(),
            run_dir: "/r".into(),
            raw_log: "/r/raw-1.log".into(),
            schema,
        }
    }

    fn output(lines: &str) -> Output {
        Output {
            exit_code: Some(0),
            stdout: lines.lines().map(str::to_string).collect(),
            stderr_tail: String::new(),
        }
    }

    fn line(value: Value) -> String {
        value.to_string()
    }

    #[test]
    fn a_planner_call_is_read_only_structured_and_free_of_the_callers_session() {
        let spawn = claude().spawn(&job(Role::Planner, Some(PLAN_SCHEMA)));

        let args = spawn.args.join(" ");
        assert_eq!(spawn.program, std::path::PathBuf::from("claude"));
        assert!(args.starts_with("-p --safe-mode --model sonnet --effort low --output-format stream-json --verbose --json-schema "));
        assert!(args.contains("--permission-mode dontAsk --no-session-persistence --append-system-prompt You are the planner"));
        assert!(args.ends_with("--tools Read Glob Grep --disallowedTools AskUserQuestion"));
        assert_eq!(spawn.stdin.as_deref(), Some("the prompt"));
        assert_eq!(
            spawn.env_remove,
            [
                "CLAUDECODE",
                "AI_AGENT",
                "CLAUDE_CODE_ENTRYPOINT",
                "ANTHROPIC_*"
            ]
        );
    }

    #[test]
    fn a_call_without_a_schema_or_a_role_leaves_those_flags_out() {
        let spawn = claude().spawn(&job(Role::Worker, None));

        assert!(
            !spawn
                .args
                .iter()
                .any(|a| a == "--json-schema" || a == "--append-system-prompt")
        );
    }

    #[test]
    fn the_captured_plan_streams_the_model_and_the_tools_but_not_the_answer_tool() {
        let adapter = claude();

        let events: Vec<HarnessEvent> = PLAN.lines().flat_map(|l| adapter.parse(l)).collect();

        assert_eq!(
            events,
            [
                HarnessEvent::Started {
                    model: Some("claude-sonnet-5-5".into())
                },
                HarnessEvent::Tool {
                    name: "Glob".into(),
                    detail: "**/*.py".into()
                },
                HarnessEvent::Tool {
                    name: "Read".into(),
                    detail: "/work/toy/calc.py".into()
                },
                HarnessEvent::Tool {
                    name: "Read".into(),
                    detail: "/work/toy/test_calc.py".into()
                },
            ]
        );
    }

    #[test]
    fn the_captured_plan_and_review_end_with_usable_answers() {
        let adapter = claude();

        let plan = adapter.outcome(&job(Role::Planner, Some(PLAN_SCHEMA)), &output(PLAN));
        let review = adapter.outcome(&job(Role::Reviewer, Some(VERDICT_SCHEMA)), &output(REVIEW));

        let Outcome::Done {
            structured: Some(plan),
            ..
        } = plan
        else {
            panic!("{plan:?}")
        };
        let Outcome::Done {
            structured: Some(review),
            ..
        } = review
        else {
            panic!("{review:?}")
        };
        assert_eq!(parse_plan(&plan).unwrap().tasks[0].id, "add-subtract");
        assert_eq!(
            parse_verdict(&review).unwrap().verdict,
            VerdictKind::Approve
        );
    }

    #[test]
    fn a_rejected_rate_limit_is_quota_with_its_reset_time() {
        let text = [
            line(json!({ "type": "rate_limit_event", "rate_limit_info":
                { "status": "rejected", "resetsAt": 1_795_899_600u64, "rateLimitType": "seven_day" } })),
            line(json!({ "type": "result", "subtype": "error_during_execution", "is_error": true })),
        ]
        .join("\n");

        let outcome = claude().outcome(&job(Role::Planner, None), &output(&text));

        assert_eq!(
            outcome,
            Outcome::Failed {
                kind: FailureKind::Quota,
                message: "Claude seven_day limit reached".into(),
                reset_at: Some(1_795_899_600_000),
            }
        );
    }

    #[test]
    fn an_allowed_warning_is_not_a_failure() {
        let text = [
            line(json!({ "type": "rate_limit_event", "rate_limit_info": { "status": "allowed_warning" } })),
            line(json!({ "type": "result", "subtype": "success", "is_error": false, "result": "ok" })),
        ]
        .join("\n");

        let outcome = claude().outcome(&job(Role::Worker, None), &output(&text));

        assert_eq!(
            outcome,
            Outcome::Done {
                text: "ok".into(),
                structured: None
            }
        );
    }

    #[test]
    fn an_api_429_is_quota_and_a_login_problem_is_auth() {
        let limited = line(
            json!({ "type": "result", "subtype": "success", "is_error": true,
                                    "api_error_status": 429, "result": "API Error: 429" }),
        );
        let logged_out = line(
            json!({ "type": "result", "subtype": "success", "is_error": true,
                                       "result": "Not logged in · Please run /login" }),
        );

        let limited = claude().outcome(&job(Role::Planner, None), &output(&limited));
        let logged_out = claude().outcome(&job(Role::Planner, None), &output(&logged_out));

        assert!(matches!(
            limited,
            Outcome::Failed {
                kind: FailureKind::Quota,
                ..
            }
        ));
        assert!(
            matches!(&logged_out, Outcome::Failed { kind: FailureKind::Auth, message, .. }
            if message.starts_with("Not logged in") && message.contains("print mode"))
        );
    }

    #[test]
    fn a_success_without_the_requested_structured_answer_is_unusable() {
        let text = line(
            json!({ "type": "result", "subtype": "success", "is_error": false, "result": "prose" }),
        );

        let outcome = claude().outcome(&job(Role::Reviewer, Some(VERDICT_SCHEMA)), &output(&text));

        assert!(matches!(
            outcome,
            Outcome::Failed {
                kind: FailureKind::Unknown,
                ..
            }
        ));
    }

    #[test]
    fn an_error_result_without_text_names_its_subtype() {
        let text =
            line(json!({ "type": "result", "subtype": "error_max_turns", "is_error": true }));

        let outcome = claude().outcome(&job(Role::Planner, None), &output(&text));

        assert!(
            matches!(outcome, Outcome::Failed { kind: FailureKind::Unknown, message, .. }
            if message == "Claude stopped: error_max_turns")
        );
    }

    #[test]
    fn a_run_without_a_result_reports_its_exit_and_stderr() {
        let mut ended = output("not json\n");
        ended.exit_code = Some(1);
        ended.stderr_tail = "Error: Invalid API key\n".into();
        let silent = Output {
            exit_code: None,
            stdout: vec![],
            stderr_tail: String::new(),
        };

        let with_stderr = claude().outcome(&job(Role::Planner, None), &ended);
        let killed = claude().outcome(&job(Role::Planner, None), &silent);

        assert!(matches!(
            with_stderr,
            Outcome::Failed {
                kind: FailureKind::Auth,
                ..
            }
        ));
        assert!(
            matches!(killed, Outcome::Failed { kind: FailureKind::Unknown, message, .. }
            if message == "claude ended without a result (a signal)")
        );
    }

    #[test]
    fn other_lines_and_blocks_produce_no_events() {
        let adapter = claude();
        let thinking = line(
            json!({ "type": "assistant", "message": { "content": [{ "type": "thinking" }] } }),
        );
        let text = line(
            json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": "hi" }] } }),
        );

        assert!(adapter.parse("garbage").is_empty());
        assert!(adapter.parse(&line(json!({ "type": "user" }))).is_empty());
        assert!(adapter.parse(&thinking).is_empty());
        assert_eq!(
            adapter.parse(&text),
            [HarnessEvent::Text { text: "hi".into() }]
        );
    }

    fn probe_outputs(version_exit: i32, auth: &str) -> [Output; 2] {
        [
            Output {
                exit_code: Some(version_exit),
                stdout: vec!["2.1.330 (Claude Code)".into()],
                stderr_tail: String::new(),
            },
            Output {
                exit_code: Some(0),
                stdout: vec![auth.into()],
                stderr_tail: String::new(),
            },
        ]
    }

    #[test]
    fn the_probe_needs_a_version_and_a_login_and_keeps_nothing_else() {
        let adapter = claude();
        let account = r#"{"loggedIn": true, "email": "someone@example.invalid"}"#;

        assert_eq!(
            adapter.probe_result(&probe_outputs(0, account)),
            Ok(Probe {
                version: "2.1.330 (Claude Code)".into()
            })
        );
        assert!(matches!(
            adapter.probe_result(&probe_outputs(0, r#"{"loggedIn": false}"#)),
            Err(ProbeError::NotLoggedIn(_))
        ));
        assert!(matches!(
            adapter.probe_result(&probe_outputs(1, account)),
            Err(ProbeError::Unsupported(_))
        ));
        assert!(matches!(
            adapter.probe_result(&[]),
            Err(ProbeError::Unsupported(_))
        ));
        assert_eq!(adapter.probe_spawns()[1].args, ["auth", "status", "--json"]);
        assert_eq!(adapter.quota_spawn(), None);
    }
}
