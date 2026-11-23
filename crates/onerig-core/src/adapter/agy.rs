//! Antigravity's `agy` CLI as a worker in `direct` mode (design section 9.2).
//! A lane in `handover` mode never reaches this adapter: onerig starts no
//! `agy` process for it.
//!
//! The quota report is observed (`tests/fixtures/agy/usage.json`); the task
//! event stream is not yet, so `parse` and `outcome` read it defensively.

use serde_json::{Value, json};

use super::{Adapter, Output, Spawn};
use crate::config::Lane;
use crate::runner::{FailureKind, HarnessEvent, Job, Outcome, Probe, ProbeError, QuotaReport};
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
const AUTH_SIGNS: [&str; 4] = [
    "unauthenticated",
    "permission_denied",
    "sign in",
    "not logged in",
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
        let listed = models
            .stdout
            .iter()
            .any(|l| l.split_whitespace().next() == Some(self.model.as_str()));
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
            reset_at: lowest.1.and_then(parse_utc_ms).filter(|&ms| ms > 0),
        })
    }

    fn spawn(&self, job: &Job) -> Spawn {
        let args = [
            "-p",
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--model",
            &self.model,
            "--mode",
            "accept-edits",
            "--sandbox",
            "--print-timeout",
            "30m",
        ];
        let message = json!({ "event": "user", "message": { "content": job.prompt } });
        Spawn {
            stdin: Some(format!("{message}\n")),
            ..self.command(&args)
        }
    }

    fn parse(&self, line: &str) -> Vec<HarnessEvent> {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        match event_kind(&value) {
            Some("init") => vec![HarnessEvent::Started {
                model: Some(self.model.clone()),
            }],
            _ => Vec::new(),
        }
    }

    fn outcome(&self, _job: &Job, output: &Output) -> Outcome {
        if let Some(error) = typed_error(&output.stderr_tail) {
            return error;
        }
        let lines: Vec<Value> = output
            .stdout
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        match lines.iter().rev().find(|v| event_kind(v) == Some("result")) {
            Some(result) => from_result(output.exit_code, result),
            None => {
                let exit = output
                    .exit_code
                    .map_or("a signal".to_string(), |c| format!("exit code {c}"));
                classify(format!(
                    "agy ended without a result ({exit}): {}",
                    output.stderr_tail.trim()
                ))
            }
        }
    }
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

fn event_kind(value: &Value) -> Option<&str> {
    value["event"].as_str().or_else(|| value["type"].as_str())
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
fn from_result(exit_code: Option<i32>, result: &Value) -> Outcome {
    let denied = result["denied_actions"]
        .as_array()
        .is_some_and(|a| !a.is_empty());
    let unfinished = !result["finished_reason"].is_null();
    if exit_code == Some(0) && result["status"] == "SUCCESS" && !denied && !unfinished {
        return Outcome::Done {
            text: result["response"].as_str().unwrap_or_default().to_string(),
            structured: None,
        };
    }
    classify(failure_message(result, denied, unfinished))
}

/// `error` may be a string or an object with a message.
fn failure_message(result: &Value, denied: bool, unfinished: bool) -> String {
    match &result["error"] {
        Value::String(text) => text.clone(),
        Value::Object(error) => error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ if denied => "agy was refused some actions".to_string(),
        _ if unfinished => format!("agy stopped early: {}", result["finished_reason"]),
        _ => format!("agy reported {}", result["status"]),
    }
}

fn classify(message: String) -> Outcome {
    let lower = message.to_lowercase();
    let kind = if QUOTA_SIGNS.iter().any(|sign| lower.contains(sign)) {
        FailureKind::Quota
    } else if AUTH_SIGNS.iter().any(|sign| lower.contains(sign)) {
        FailureKind::Auth
    } else {
        FailureKind::Unknown
    };
    Outcome::failed(kind, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Effort, HarnessKind, Mode};
    use crate::runner::Role;

    const USAGE: &str = include_str!("../../tests/fixtures/agy/usage.json");

    fn agy(group: &str) -> Agy {
        Agy::new(&Lane {
            harness: HarnessKind::Agy,
            bin: "agy".into(),
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

    #[test]
    fn a_task_runs_in_print_mode_with_edits_accepted_in_the_sandbox_and_no_self_update() {
        let spawn = agy("Gemini Models").spawn(&job());

        assert_eq!(
            spawn.args.join(" "),
            "-p --output-format stream-json --input-format stream-json --model gemini-4-hydrogen-medium \
             --mode accept-edits --sandbox --print-timeout 30m"
        );
        assert_eq!(
            spawn.stdin.as_deref(),
            Some("{\"event\":\"user\",\"message\":{\"content\":\"do \\\"it\\\"\"}}\n")
        );
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
        assert_eq!(
            agy("No such group").quota_result(&output(USAGE, "", Some(0))),
            None
        );
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
        let ok = r#"{"event":"result","status":"SUCCESS","response":"Added subtract."}"#;
        let denied = r#"{"event":"result","status":"SUCCESS","denied_actions":["run_command"]}"#;
        let early = r#"{"event":"result","status":"SUCCESS","finished_reason":"max_steps"}"#;

        assert_eq!(
            agy("G").outcome(&job(), &output(ok, "", Some(0))),
            Outcome::Done {
                text: "Added subtract.".into(),
                structured: None
            }
        );
        assert!(
            matches!(agy("G").outcome(&job(), &output(denied, "", Some(0))), Outcome::Failed { message, .. } if message == "agy was refused some actions")
        );
        assert!(
            matches!(agy("G").outcome(&job(), &output(early, "", Some(0))), Outcome::Failed { message, .. } if message.starts_with("agy stopped early"))
        );
    }

    #[test]
    fn typed_errors_decide_quota_and_auth() {
        let quota = "AGY_ERROR: {\"status\":\"RESOURCE_EXHAUSTED\",\"error_code\":429,\"message\":\"Individual quota reached. Resets in 2h3m\"}\n";
        let suspended =
            "AGY_ERROR: {\"status\":\"PERMISSION_DENIED\",\"message\":\"account suspended\"}";
        let other = "AGY_ERROR: {\"status\":\"DEADLINE_EXCEEDED\",\"message\":\"print_timeout\"}";

        assert!(matches!(
            agy("G").outcome(&job(), &output("", quota, Some(3))),
            Outcome::Failed {
                kind: FailureKind::Quota,
                ..
            }
        ));
        assert!(matches!(
            agy("G").outcome(&job(), &output("", suspended, Some(3))),
            Outcome::Failed {
                kind: FailureKind::Auth,
                ..
            }
        ));
        assert!(
            matches!(agy("G").outcome(&job(), &output("", other, Some(3))), Outcome::Failed { kind: FailureKind::Unknown, message, .. } if message == "print_timeout")
        );
    }

    #[test]
    fn an_error_result_is_classified_by_its_text_whatever_its_shape() {
        let as_text =
            r#"{"event":"result","status":"ERROR","error":"You have exhausted your quota"}"#;
        let as_object =
            r#"{"type":"result","status":"ERROR","error":{"message":"Please sign in again"}}"#;
        let bare = r#"{"event":"result","status":"ERROR"}"#;

        assert!(matches!(
            agy("G").outcome(&job(), &output(as_text, "", Some(1))),
            Outcome::Failed {
                kind: FailureKind::Quota,
                ..
            }
        ));
        assert!(matches!(
            agy("G").outcome(&job(), &output(as_object, "", Some(1))),
            Outcome::Failed {
                kind: FailureKind::Auth,
                ..
            }
        ));
        assert!(
            matches!(agy("G").outcome(&job(), &output(bare, "", Some(1))), Outcome::Failed { kind: FailureKind::Unknown, message, .. } if message == "agy reported \"ERROR\"")
        );
    }

    #[test]
    fn a_run_without_a_result_reports_its_exit() {
        let outcome = agy("G").outcome(&job(), &output("{\"event\":\"init\"}", "boom", None));

        assert!(
            matches!(outcome, Outcome::Failed { kind: FailureKind::Unknown, message, .. }
            if message == "agy ended without a result (a signal): boom")
        );
        assert_eq!(
            agy("G").parse("{\"event\":\"init\"}"),
            [HarnessEvent::Started {
                model: Some("gemini-4-hydrogen-medium".into())
            }]
        );
        assert!(agy("G").parse("{\"event\":\"step_update\"}").is_empty());
        assert!(agy("G").parse("garbage").is_empty());
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
        assert!(
            matches!(adapter.probe_result(&probe("1.5.14", 0, "other-model\tOther")), Err(ProbeError::Unsupported(m)) if m.contains("gemini-4-hydrogen-medium"))
        );
        assert!(matches!(
            adapter.probe_result(&[]),
            Err(ProbeError::Unsupported(_))
        ));
        assert_eq!(adapter.probe_spawns()[1].args, ["models"]);
    }

    #[test]
    fn versions_need_three_numbers() {
        assert_eq!(parse_version("1.5.14"), Some((1, 5, 14)));
        assert_eq!(parse_version("agy 2.0.0"), Some((2, 0, 0)));
        assert_eq!(parse_version("1.5"), None);
    }
}
