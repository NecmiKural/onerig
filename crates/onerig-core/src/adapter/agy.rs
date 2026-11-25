//! Antigravity's `agy` CLI as a worker in `direct` mode (design section 9.2).
//! A lane in `handover` mode never reaches this adapter: onerig starts no
//! `agy` process for it.
//!
//! The quota report and a task stream are observed (`tests/fixtures/agy/`).
//! A task prints one `init`, `step_update` events and one `result`, each
//! named by an `event` field.

use serde_json::Value;

use super::{
    Adapter, Output, Spawn, classify_error_text, exit_message, extract_json_object, json_lines,
    prepend_role,
};
use crate::config::Lane;
use crate::runner::{
    FailureKind, HarnessEvent, Job, Outcome, Probe, ProbeError, QuotaReport, Role,
};
use crate::store::parse_utc_ms;

/// Exit code 3, typed error lines and `finished_reason` arrived in this version.
const MIN_VERSION: (u64, u64, u64) = (1, 5, 10);
/// `agy` updates itself on ordinary runs; the version must not change mid-run.
const NO_UPDATE: (&str, &str) = ("AGY_CLI_DISABLE_AUTO_UPDATE", "true");
const ERROR_PREFIX: &str = "AGY_ERROR: ";

const QUOTA_SIGNS: [&str; 4] = [
    "resource_exhausted",
    "quota reached",
    "exhausted your quota",
    "credits balance is too low",
];
/// Signing in again is the fix for all of these, eligibility included.
const AUTH_SIGNS: [&str; 8] = [
    "unauthenticated",
    "permission_denied",
    "authentication",
    "sign in",
    "sign-in",
    "not logged in",
    "no valid credentials",
    "eligibility check failed",
];
/// Tool parameters that name what a tool worked on; agy uses PascalCase.
const TOOL_TARGETS: [&str; 8] = [
    "CommandLine",
    "command",
    "AbsolutePath",
    "Path",
    "file_path",
    "path",
    "Query",
    "query",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agy {
    bin: String,
    model: String,
    quota_group: String,
}

impl Agy {
    pub fn new(lane: &Lane) -> Agy {
        Agy {
            bin: lane.bin.clone(),
            model: lane.model.clone().unwrap_or_default(),
            quota_group: lane.quota_group.clone().unwrap_or_default(),
        }
    }

    fn command(&self, args: &[&str]) -> Spawn {
        Spawn {
            program: self.bin.clone().into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env_set: vec![(NO_UPDATE.0.to_string(), NO_UPDATE.1.to_string())],
            ..Spawn::default()
        }
    }
}

impl Adapter for Agy {
    fn probe_spawns(&self) -> Vec<Spawn> {
        // `agy models` needs the sign-in and spends no quota.
        vec![self.command(&["--version"]), self.command(&["models"])]
    }

    fn probe_result(&self, outputs: &[Output]) -> Result<Probe, ProbeError> {
        let [version, models] = outputs else {
            return Err(ProbeError::Unsupported(
                "unexpected probe outputs".to_string(),
            ));
        };
        let line = version.stdout.first().cloned().unwrap_or_default();
        if parse_version(&line).is_none_or(|v| v < MIN_VERSION) {
            return Err(ProbeError::Unsupported(format!(
                "{} reports {line:?}; onerig needs agy 1.5.10 or later",
                self.bin
            )));
        }
        if models.exit_code != Some(0) {
            return Err(ProbeError::NotLoggedIn(format!(
                "`{} models` failed; start agy once to sign in",
                self.bin
            )));
        }
        let listed = parse_models(&models.stdout)
            .into_iter()
            .any(|(id, _)| id == self.model);
        if !listed {
            return Err(ProbeError::Unsupported(format!(
                "model {} is not offered to this account; see `{} models`",
                self.model, self.bin
            )));
        }
        Ok(Probe { version: line })
    }

    fn quota_spawn(&self) -> Option<Spawn> {
        Some(self.command(&[
            "-p",
            "/usage",
            "--output-format",
            "json",
            "--print-timeout",
            "60s",
        ]))
    }

    fn quota_result(&self, output: &Output) -> Option<QuotaReport> {
        let report: Value = serde_json::from_str(&output.stdout.join("\n")).ok()?;
        let group = report["command"]["data"]["groups"]
            .as_array()?
            .iter()
            .find(|g| g["name"] == self.quota_group.as_str())?;
        let lowest = group["buckets"]
            .as_array()?
            .iter()
            .filter_map(|b| Some((b["remaining_fraction"].as_f64()?, b["reset_time"].as_str())))
            .min_by(|a, b| a.0.total_cmp(&b.0))?;
        Some(QuotaReport {
            remaining_fraction: lowest.0,
            // An unused bucket reports the epoch, which is no reset time.
            reset_at: lowest.1.and_then(parse_utc_ms).filter(|&ms| ms != 0),
        })
    }

    /// Headless runs cannot answer approval prompts, so worker permissions are
    /// granted and `--sandbox` restricts what the terminal tool can reach.
    /// Planner and reviewer runs use `--mode plan --sandbox` without write permissions.
    fn spawn(&self, job: &Job) -> Spawn {
        let prompt = prepend_role(job.role, &job.prompt);
        let mut args = vec!["-p".to_string(), prompt];
        match job.role {
            Role::Worker => args.push("--dangerously-skip-permissions".to_string()),
            Role::Planner | Role::Reviewer => {
                args.extend(["--mode".to_string(), "plan".to_string()])
            }
        }
        args.extend([
            "--sandbox".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--model".to_string(),
            self.model.clone(),
            "--print-timeout".to_string(),
            "30m".to_string(),
        ]);
        if let (Role::Planner | Role::Reviewer, Some(schema)) = (job.role, job.schema) {
            args.extend(["--json-schema".to_string(), schema.to_string()]);
        }
        let mut spawn = self.command(&[]);
        spawn.args = args;
        spawn
    }

    fn parse(&self, line: &str) -> Vec<HarnessEvent> {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let event = match value["event"].as_str() {
            Some("init") => Some(HarnessEvent::Started {
                model: Some(self.model.clone()),
            }),
            Some("step_update") => finished_tool(&value["step_update"]),
            _ => None,
        };
        event.into_iter().collect()
    }

    fn outcome(&self, job: &Job, output: &Output) -> Outcome {
        if let Some(error) = typed_error(&output.stderr_tail) {
            return error;
        }
        let lines = json_lines(&output.stdout);
        match lines.iter().rev().find(|v| v["event"] == "result") {
            Some(event) => from_result(job, output.exit_code, &event["result"]),
            None => classify(exit_message("agy ended without a result", output)),
        }
    }
}

/// Parses `agy models` stdout lines into `(model_id, display_name)` pairs.
pub fn parse_models(stdout: &[String]) -> Vec<(String, String)> {
    stdout
        .iter()
        .filter_map(|raw| {
            let line = raw.rsplit('\r').next().unwrap_or(raw).trim();
            let mut parts = line.splitn(2, char::is_whitespace);
            let id = parts.next()?.trim();
            if !is_model_id(id) {
                return None;
            }
            let label = parts
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(id);
            Some((id.to_string(), label.to_string()))
        })
        .collect()
}

fn is_model_id(id: &str) -> bool {
    id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.' | b'_')
        })
}

/// `agy --version` prints `1.5.14`.
fn parse_version(line: &str) -> Option<(u64, u64, u64)> {
    let mut parts = line
        .split_whitespace()
        .last()?
        .split('.')
        .map(|p| p.parse().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// A tool step is reported again on every update; it is shown once, when done.
fn finished_tool(step: &Value) -> Option<HarnessEvent> {
    if step["step_type"] != "tool" || step["state"] != "DONE" {
        return None;
    }
    let info = &step["tool_info"];
    let name = info["name"]
        .as_str()
        .or_else(|| step["tool_name"].as_str())
        .unwrap_or("tool");
    let detail = TOOL_TARGETS
        .iter()
        .find_map(|key| info["parameters"][*key].as_str())
        .unwrap_or_default();
    Some(HarnessEvent::Tool {
        name: name.to_string(),
        detail: detail.to_string(),
    })
}

/// A stderr line `AGY_ERROR: {...}` carries the canonical status.
fn typed_error(stderr: &str) -> Option<Outcome> {
    let line = stderr
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix(ERROR_PREFIX))?;
    let error: Value = serde_json::from_str(line).ok()?;
    let message = error["message"].as_str().unwrap_or(line).to_string();
    let status = error["status"].as_str().unwrap_or_default();
    let kind = if status == "RESOURCE_EXHAUSTED" || error["error_code"] == 429 {
        FailureKind::Quota
    } else if matches!(status, "UNAUTHENTICATED" | "PERMISSION_DENIED") {
        FailureKind::Auth
    } else {
        return Some(classify(message));
    };
    Some(Outcome::failed(kind, message))
}

/// A success also needs a clean exit and no refused or unfinished actions,
/// since agy can report success with tools denied.
fn from_result(job: &Job, exit_code: Option<i32>, result: &Value) -> Outcome {
    let denied = result["denied_actions"]
        .as_array()
        .is_some_and(|a| !a.is_empty());
    let unfinished = !result["finished_reason"].is_null();
    if exit_code == Some(0) && result["status"] == "SUCCESS" && !denied && !unfinished {
        let text = result["response"].as_str().unwrap_or_default().to_string();
        if job.schema.is_none() {
            return Outcome::Done {
                text,
                structured: None,
            };
        }
        let structured = result
            .get("structured_output")
            .filter(|v| v.is_object())
            .cloned()
            .or_else(|| extract_json_object(&text));
        return match structured {
            Some(value) => Outcome::Done {
                text,
                structured: Some(value),
            },
            None => Outcome::failed(
                FailureKind::Unknown,
                "agy gave no structured answer".to_string(),
            ),
        };
    }
    classify(failure_message(result, denied, unfinished))
}

/// `error` may be a string or an object with a type and a message.
fn failure_message(result: &Value, denied: bool, unfinished: bool) -> String {
    match &result["error"] {
        Value::String(text) => text.clone(),
        Value::Object(error) => ["type", "message"]
            .iter()
            .filter_map(|key| error.get(*key).and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ if denied => "agy was refused some actions".to_string(),
        _ if unfinished => format!("agy stopped early: {}", result["finished_reason"]),
        _ => format!("agy reported {}", result["status"]),
    }
}

fn classify(message: String) -> Outcome {
    classify_error_text(&QUOTA_SIGNS, &AUTH_SIGNS, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Effort, HarnessKind, Mode};
    use crate::runner::Role;

    const USAGE: &str = include_str!("../../tests/fixtures/agy/usage.json");
    const TASK: &str = include_str!("../../tests/fixtures/agy/task.jsonl");

    fn agy(group: &str) -> Agy {
        Agy::new(&Lane {
            harness: HarnessKind::Agy,
            bin: "agy".into(),
            provider: None,
            model: Some("gemini-4-hydrogen-medium".into()),
            effort: Effort::Medium,
            mode: Mode::Direct,
            quota_group: Some(group.into()),
            accept_vendor_terms_risk: true,
            script: None,
        })
    }

    fn job() -> Job {
        Job {
            role: Role::Worker,
            prompt: "do \"it\"".into(),
            cwd: "/w".into(),
            run_dir: "/r".into(),
            raw_log: "/r/raw-1.log".into(),
            schema: None,
        }
    }

    fn output(stdout: &str, stderr: &str, exit_code: Option<i32>) -> Output {
        Output {
            exit_code,
            stdout: stdout.lines().map(str::to_string).collect(),
            stderr_tail: stderr.into(),
        }
    }

    fn failed_with(outcome: &Outcome) -> (FailureKind, &str) {
        match outcome {
            Outcome::Failed { kind, message, .. } => (*kind, message.as_str()),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_task_runs_in_print_mode_in_the_sandbox_without_self_update() {
        let spawn = agy("Gemini Models").spawn(&job());

        assert_eq!(
            spawn.args,
            [
                "-p",
                "do \"it\"",
                "--dangerously-skip-permissions",
                "--sandbox",
                "--output-format",
                "stream-json",
                "--model",
                "gemini-4-hydrogen-medium",
                "--print-timeout",
                "30m"
            ]
        );
        assert_eq!(spawn.stdin, None);
        assert_eq!(
            spawn.env_set,
            [(
                "AGY_CLI_DISABLE_AUTO_UPDATE".to_string(),
                "true".to_string()
            )]
        );
    }

    #[test]
    fn the_captured_usage_reports_the_lowest_bucket_of_the_lanes_group() {
        let usage = output(USAGE, "", Some(0));

        let gemini = agy("Gemini Models").quota_result(&usage).unwrap();
        let others = agy("Claude and GPT models").quota_result(&usage).unwrap();

        assert_eq!(gemini.remaining_fraction, 1.0);
        assert_eq!(gemini.reset_at, parse_utc_ms("2026-11-30T20:24:10Z"));
        assert!((others.remaining_fraction - 0.077).abs() < 0.001);
        assert_eq!(others.reset_at, parse_utc_ms("2026-11-28T07:36:31Z"));
    }

    #[test]
    fn an_unknown_group_or_unreadable_usage_reports_nothing() {
        let usage = output(USAGE, "", Some(0));

        assert_eq!(agy("No such group").quota_result(&usage), None);
        assert_eq!(
            agy("Gemini Models").quota_result(&output("not json", "", Some(0))),
            None
        );
    }

    #[test]
    fn an_epoch_reset_time_means_none() {
        let usage = r#"{"command":{"data":{"groups":[{"name":"G","buckets":[
            {"remaining_fraction":0.5,"reset_time":"1970-01-01T00:00:00Z"}]}]}}}"#;

        let report = agy("G").quota_result(&output(usage, "", Some(0))).unwrap();

        assert_eq!(report.reset_at, None);
        assert_eq!(
            agy("G").quota_spawn().unwrap().args,
            [
                "-p",
                "/usage",
                "--output-format",
                "json",
                "--print-timeout",
                "60s"
            ]
        );
    }

    #[test]
    fn a_clean_success_is_done_and_a_soft_failure_is_not() {
        let ok = r#"{"event":"result","result":{"status":"SUCCESS","response":"Added subtract."}}"#;
        let denied =
            r#"{"event":"result","result":{"status":"SUCCESS","denied_actions":["run_command"]}}"#;
        let early =
            r#"{"event":"result","result":{"status":"SUCCESS","finished_reason":"max_steps"}}"#;

        assert_eq!(
            agy("G").outcome(&job(), &output(ok, "", Some(0))),
            Outcome::Done {
                text: "Added subtract.".into(),
                structured: None
            }
        );
        let refused = agy("G").outcome(&job(), &output(denied, "", Some(0)));
        let stopped = agy("G").outcome(&job(), &output(early, "", Some(0)));
        assert_eq!(failed_with(&refused).1, "agy was refused some actions");
        assert!(failed_with(&stopped).1.starts_with("agy stopped early"));
    }

    #[test]
    fn typed_errors_decide_quota_and_auth() {
        let quota = "AGY_ERROR: {\"status\":\"RESOURCE_EXHAUSTED\",\"error_code\":429,\"message\":\"Individual quota reached. Resets in 2h3m\"}\n";
        let suspended =
            "AGY_ERROR: {\"status\":\"PERMISSION_DENIED\",\"message\":\"account suspended\"}";
        let other = "AGY_ERROR: {\"status\":\"DEADLINE_EXCEEDED\",\"message\":\"print_timeout\"}";
        let code_only = "AGY_ERROR: {\"error_code\":429,\"message\":\"Too many requests\"}";

        let quota = agy("G").outcome(&job(), &output("", quota, Some(3)));
        let suspended = agy("G").outcome(&job(), &output("", suspended, Some(3)));
        let other = agy("G").outcome(&job(), &output("", other, Some(3)));
        let code_only = agy("G").outcome(&job(), &output("", code_only, Some(3)));

        assert_eq!(failed_with(&quota).0, FailureKind::Quota);
        assert_eq!(failed_with(&code_only).0, FailureKind::Quota);
        assert_eq!(failed_with(&suspended).0, FailureKind::Auth);
        assert_eq!(failed_with(&other), (FailureKind::Unknown, "print_timeout"));
    }

    #[test]
    fn an_error_result_is_classified_by_its_text_whatever_its_shape() {
        let as_text = r#"{"event":"result","result":{"status":"ERROR","error":"You have exhausted your quota"}}"#;
        let as_object = r#"{"event":"result","result":{"status":"ERROR","error":{"type":"auth","message":"authentication failed"}}}"#;
        let bare = r#"{"event":"result","result":{"status":"ERROR"}}"#;

        let quota = agy("G").outcome(&job(), &output(as_text, "", Some(1)));
        let auth = agy("G").outcome(&job(), &output(as_object, "", Some(1)));
        let unknown = agy("G").outcome(&job(), &output(bare, "", Some(1)));

        assert_eq!(failed_with(&quota).0, FailureKind::Quota);
        assert_eq!(
            failed_with(&auth),
            (FailureKind::Auth, "auth authentication failed")
        );
        assert_eq!(
            failed_with(&unknown),
            (FailureKind::Unknown, "agy reported \"ERROR\"")
        );
    }

    #[test]
    fn a_run_without_a_result_reports_its_exit() {
        let outcome = agy("G").outcome(&job(), &output("{\"event\":\"init\"}", "boom", None));

        assert_eq!(
            failed_with(&outcome),
            (
                FailureKind::Unknown,
                "agy ended without a result (a signal): boom"
            )
        );
    }

    #[test]
    fn init_starts_and_only_finished_tool_steps_are_shown() {
        let active = r#"{"event":"step_update","step_update":{"step_type":"tool","state":"ACTIVE","step_index":3,"tool_info":{"name":"run_command"}}}"#;
        let done = r#"{"event":"step_update","step_update":{"step_type":"tool","state":"DONE","step_index":3,
            "tool_info":{"name":"run_command","parameters":{"CommandLine":"python3 -m unittest"}}}}"#;
        let bare = r#"{"event":"step_update","step_update":{"step_type":"tool","state":"DONE","tool_name":"view_file"}}"#;
        let text = r#"{"event":"step_update","step_update":{"step_type":"agent_response","state":"DONE","text_delta":"hi"}}"#;
        let adapter = agy("G");

        assert_eq!(
            adapter.parse("{\"event\":\"init\"}"),
            [HarnessEvent::Started {
                model: Some("gemini-4-hydrogen-medium".into())
            }]
        );
        assert!(adapter.parse(active).is_empty());
        assert_eq!(
            adapter.parse(done),
            [HarnessEvent::Tool {
                name: "run_command".into(),
                detail: "python3 -m unittest".into()
            }]
        );
        assert_eq!(
            adapter.parse(bare),
            [HarnessEvent::Tool {
                name: "view_file".into(),
                detail: String::new()
            }]
        );
        assert!(adapter.parse(text).is_empty());
        assert!(adapter.parse("{\"type\":\"init\"}").is_empty());
        assert!(adapter.parse("garbage").is_empty());
    }

    fn probe(version: &str, models_exit: i32, models: &str) -> [Output; 2] {
        [
            output(version, "", Some(0)),
            output(models, "", Some(models_exit)),
        ]
    }

    #[test]
    fn the_probe_needs_a_recent_agy_a_sign_in_and_the_lanes_model() {
        let listed =
            "Fetching available models...\ngemini-4-hydrogen-medium\tGemini 4 Hydrogen (Medium)";
        let adapter = agy("G");

        assert_eq!(
            adapter.probe_result(&probe("1.5.14", 0, listed)),
            Ok(Probe {
                version: "1.5.14".into()
            })
        );
        assert!(matches!(
            adapter.probe_result(&probe("1.4.7", 0, listed)),
            Err(ProbeError::Unsupported(_))
        ));
        assert!(matches!(
            adapter.probe_result(&probe("1.5.14", 1, "")),
            Err(ProbeError::NotLoggedIn(_))
        ));
        assert!(matches!(
            adapter.probe_result(&probe("1.5.14", 0, "other-model\tOther")),
            Err(ProbeError::Unsupported(m)) if m.contains("gemini-4-hydrogen-medium")
        ));
        assert!(matches!(
            adapter.probe_result(&[]),
            Err(ProbeError::Unsupported(_))
        ));
        assert_eq!(adapter.probe_spawns()[1].args, ["models"]);
    }

    #[test]
    fn the_captured_task_shows_each_finished_tool_once_and_ends_done() {
        let adapter = agy("Gemini Models");

        let events: Vec<HarnessEvent> = TASK.lines().flat_map(|l| adapter.parse(l)).collect();
        let outcome = adapter.outcome(&job(), &output(TASK, "", Some(0)));

        let tools: Vec<(&str, &str)> = events[1..]
            .iter()
            .map(|e| match e {
                HarnessEvent::Tool { name, detail } => (name.as_str(), detail.as_str()),
                other => panic!("expected a tool, got {other:?}"),
            })
            .collect();
        assert!(matches!(events[0], HarnessEvent::Started { .. }));
        assert_eq!(
            tools,
            [
                ("read_file", "/work/calc.py"),
                ("read_file", "/work/test_calc.py"),
                ("edit_file", "/work/calc.py"),
                ("edit_file", "/work/test_calc.py"),
                ("run_command", "python -m unittest test_calc"),
            ]
        );
        assert!(
            matches!(outcome, Outcome::Done { ref text, .. } if text.starts_with("Added `subtract(a, b)`"))
        );
    }

    #[test]
    fn versions_need_three_numbers() {
        assert_eq!(parse_version("1.5.14"), Some((1, 5, 14)));
        assert_eq!(parse_version("agy 2.0.0"), Some((2, 0, 0)));
        assert_eq!(parse_version("1.5"), None);
    }

    #[test]
    fn parse_models_handles_spinner_prefixes_and_labels() {
        let lines = vec![
            "\r⠋ Fetching available models...\rgemini-4-carbon-high    Gemini 4 Carbon (High)"
                .into(),
            "claude-sonnet-5-5-medium\tClaude Sonnet 5.5 (Medium)".into(),
            "Fetching available models...".into(),
        ];

        assert_eq!(
            parse_models(&lines),
            [
                (
                    "gemini-4-carbon-high".into(),
                    "Gemini 4 Carbon (High)".into()
                ),
                (
                    "claude-sonnet-5-5-medium".into(),
                    "Claude Sonnet 5.5 (Medium)".into()
                ),
            ]
        );
    }

    #[test]
    fn planner_and_reviewer_use_plan_mode_and_extract_structured_output() {
        let planner_job = Job {
            role: Role::Planner,
            schema: Some(r#"{"type":"object"}"#),
            ..job()
        };

        let spawn = agy("Gemini Models").spawn(&planner_job);
        assert!(
            !spawn
                .args
                .contains(&"--dangerously-skip-permissions".to_string())
        );
        assert!(spawn.args.contains(&"--mode".to_string()));
        assert!(spawn.args.contains(&"plan".to_string()));
        assert!(spawn.args.contains(&"--json-schema".to_string()));
        assert!(spawn.args[1].starts_with("You are the planner"));

        let from_field = r#"{"event":"result","result":{"status":"SUCCESS","response":"ok","structured_output":{"tasks":[]}}}"#;
        let from_text =
            r#"{"event":"result","result":{"status":"SUCCESS","response":"{\"tasks\":[]}"}}"#;
        let missing = r#"{"event":"result","result":{"status":"SUCCESS","response":"plain text"}}"#;

        assert!(matches!(
            agy("G").outcome(&planner_job, &output(from_field, "", Some(0))),
            Outcome::Done {
                structured: Some(_),
                ..
            }
        ));
        assert!(matches!(
            agy("G").outcome(&planner_job, &output(from_text, "", Some(0))),
            Outcome::Done {
                structured: Some(_),
                ..
            }
        ));
        assert!(matches!(
            agy("G").outcome(&planner_job, &output(missing, "", Some(0))),
            Outcome::Failed {
                kind: FailureKind::Unknown,
                ..
            }
        ));
    }
}
