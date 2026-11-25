//! OpenAI's Codex CLI (`codex exec`) pointed at DeepSeek's Responses API, as a
//! worker (design section 9.3). The output shapes are those captured in
//! `tests/fixtures/codex/`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{
    Adapter, Output, Spawn, classify_error_text, exit_message, extract_json_object, json_lines,
    prepend_role,
};
use crate::config::{CodexProvider, Lane};
use crate::runner::{
    FailureKind, HarnessEvent, Job, Outcome, Probe, ProbeError, QuotaReport, Role,
};

/// DeepSeek documents its Codex integration for this version and later.
const MIN_VERSION: (u64, u64) = (0, 164);
const KEY_VARIABLE: &str = "DEEPSEEK_API_KEY";

const QUOTA_SIGNS: [&str; 4] = [
    "status 402",
    "insufficient balance",
    "usage_limit_exceeded",
    "usage limit",
];
const AUTH_SIGNS: [&str; 4] = ["status 401", "status 403", "unauthorized", "api key"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Codex {
    bin: String,
    provider: CodexProvider,
    model: String,
    /// The lane's own `CODEX_HOME` (DeepSeek only); `~/.codex` is used for OpenAI.
    home: PathBuf,
}

impl Codex {
    /// `data_home` is onerig's data directory; the lane's Codex home goes below it.
    pub fn new(name: &str, lane: &Lane, data_home: &Path) -> Codex {
        Codex {
            bin: lane.bin.clone(),
            provider: lane.provider.unwrap_or(CodexProvider::Deepseek),
            model: lane.model.clone().unwrap_or_default(),
            home: data_home.join("onerig").join("codex").join(name),
        }
    }

    fn command(&self, args: &[&str]) -> Spawn {
        let env_set = match self.provider {
            CodexProvider::Deepseek => {
                vec![("CODEX_HOME".to_string(), self.home.display().to_string())]
            }
            CodexProvider::Openai => Vec::new(),
        };
        Spawn {
            program: self.bin.clone().into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env_set,
            ..Spawn::default()
        }
    }

    /// Without a model catalog Codex uses fallback metadata, which works; a
    /// catalog entry would need its own long system prompt.
    fn config_toml(&self) -> String {
        format!(
            "model = \"{model}\"\n\
             model_provider = \"deepseek\"\n\n\
             [model_providers.deepseek]\n\
             name = \"deepseek\"\n\
             base_url = \"https://api.deepseek.com/\"\n\
             wire_api = \"responses\"\n\
             env_key = \"{KEY_VARIABLE}\"\n",
            model = self.model
        )
    }
}

impl Adapter for Codex {
    fn probe_spawns(&self) -> Vec<Spawn> {
        let auth = match self.provider {
            CodexProvider::Deepseek => Spawn {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), format!("[ -n \"${KEY_VARIABLE}\" ]")],
                ..Spawn::default()
            },
            CodexProvider::Openai => self.command(&["login", "status"]),
        };
        vec![
            self.command(&["--version"]),
            self.command(&["exec", "--help"]),
            auth,
        ]
    }

    fn probe_result(&self, outputs: &[Output]) -> Result<Probe, ProbeError> {
        let [version, help, key] = outputs else {
            return Err(ProbeError::Unsupported(
                "unexpected probe outputs".to_string(),
            ));
        };
        let line = version.stdout.first().cloned().unwrap_or_default();
        if parse_version(&line).is_none_or(|v| v < MIN_VERSION) {
            return Err(ProbeError::Unsupported(format!(
                "{} reports {line:?}; onerig needs Codex 0.164 or later",
                self.bin
            )));
        }
        if !help.stdout.iter().any(|l| l.contains("--json")) {
            return Err(ProbeError::Unsupported(format!(
                "`{} exec` has no --json option",
                self.bin
            )));
        }
        if key.exit_code != Some(0) {
            let msg = match self.provider {
                CodexProvider::Deepseek => {
                    format!("{KEY_VARIABLE} is not set; export it in the shell that runs onerig")
                }
                CodexProvider::Openai => {
                    format!("Codex is not logged in; run `{} login`", self.bin)
                }
            };
            return Err(ProbeError::NotLoggedIn(msg));
        }
        Ok(Probe { version: line })
    }

    fn quota_spawn(&self) -> Option<Spawn> {
        None
    }

    fn quota_result(&self, _output: &Output) -> Option<QuotaReport> {
        None
    }

    fn spawn(&self, job: &Job) -> Spawn {
        let sandbox = match job.role {
            Role::Worker => "workspace-write",
            Role::Planner | Role::Reviewer => "read-only",
        };
        let mut args = vec![
            "exec".to_string(),
            "--json".to_string(),
            "--skip-git-repo-check".to_string(),
        ];
        if self.provider == CodexProvider::Openai {
            args.extend([
                "--ignore-user-config".to_string(),
                "--ephemeral".to_string(),
            ]);
        }
        args.extend([
            "--sandbox".to_string(),
            sandbox.to_string(),
            "--model".to_string(),
            self.model.clone(),
        ]);
        let mut files = Vec::new();
        if self.provider == CodexProvider::Deepseek {
            files.push((self.home.join("config.toml"), self.config_toml()));
        }
        if let (CodexProvider::Openai, Some(schema)) = (self.provider, job.schema) {
            let path = schema_file_path(job);
            args.extend(["--output-schema".to_string(), path.display().to_string()]);
            files.push((path, schema.to_string()));
        }
        args.push("-".to_string());
        let mut spawn = self.command(&[]);
        spawn.args = args;
        spawn.stdin = Some(prompt_for_job(self.provider, job));
        spawn.files = files;
        spawn
    }

    fn parse(&self, line: &str) -> Vec<HarnessEvent> {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let event = match value["type"].as_str() {
            Some("thread.started") => Some(HarnessEvent::Started {
                model: Some(self.model.clone()),
            }),
            Some("item.completed") => item_event(&value["item"]),
            _ => None,
        };
        event.into_iter().collect()
    }

    fn outcome(&self, job: &Job, output: &Output) -> Outcome {
        let lines = json_lines(&output.stdout);
        if lines.iter().any(|v| v["type"] == "turn.completed") {
            let text = last_message(&lines);
            return structured_or_done(job, text);
        }
        let message = final_error(&lines)
            .unwrap_or_else(|| exit_message("codex ended without finishing its turn", output));
        classify_error_text(&QUOTA_SIGNS, &AUTH_SIGNS, message)
    }
}

fn schema_file_path(job: &Job) -> PathBuf {
    let stem = job
        .raw_log
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("job");
    job.run_dir.join(format!("{stem}.schema.json"))
}

fn prompt_for_job(provider: CodexProvider, job: &Job) -> String {
    let mut prompt = prepend_role(job.role, &job.prompt);
    if let (CodexProvider::Deepseek, Some(schema)) = (provider, job.schema) {
        prompt.push_str("\n\nAnswer with ONLY a JSON object matching this JSON Schema:\n");
        prompt.push_str(schema);
    }
    prompt
}

fn structured_or_done(job: &Job, text: String) -> Outcome {
    if job.schema.is_none() {
        return Outcome::Done {
            text,
            structured: None,
        };
    }
    match extract_json_object(&text) {
        Some(value) => Outcome::Done {
            text,
            structured: Some(value),
        },
        None => Outcome::failed(
            FailureKind::Unknown,
            "codex gave no structured answer".to_string(),
        ),
    }
}

/// `codex-cli 0.171.0` to `(0, 171)`.
fn parse_version(line: &str) -> Option<(u64, u64)> {
    let version = line.split_whitespace().last()?;
    let mut parts = version.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??))
}

/// Error items are warnings (such as the missing model catalog) and are left out.
fn item_event(item: &Value) -> Option<HarnessEvent> {
    match item["type"].as_str()? {
        "agent_message" => Some(HarnessEvent::Text {
            text: item["text"].as_str()?.to_string(),
        }),
        "command_execution" => Some(HarnessEvent::Tool {
            name: "shell".to_string(),
            detail: item["command"].as_str()?.to_string(),
        }),
        "file_change" => {
            let paths: Vec<&str> = item["changes"]
                .as_array()?
                .iter()
                .filter_map(|change| change["path"].as_str())
                .collect();
            Some(HarnessEvent::Tool {
                name: "edit".to_string(),
                detail: paths.join(", "),
            })
        }
        _ => None,
    }
}

fn last_message(lines: &[Value]) -> String {
    lines
        .iter()
        .rev()
        .find(|v| v["type"] == "item.completed" && v["item"]["type"] == "agent_message")
        .and_then(|v| v["item"]["text"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// The failed turn's error, or the last error that is not a retry notice.
fn final_error(lines: &[Value]) -> Option<String> {
    let failed_turn = lines
        .iter()
        .rev()
        .find(|v| v["type"] == "turn.failed")
        .and_then(|v| v["error"]["message"].as_str());
    let last_error = || {
        lines
            .iter()
            .rev()
            .filter(|v| v["type"] == "error")
            .filter_map(|v| v["message"].as_str())
            .find(|m| !m.starts_with("Reconnecting"))
    };
    failed_turn.or_else(last_error).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Effort, HarnessKind, Mode};
    use crate::runner::Role;
    use serde_json::json;

    const TASK: &str = include_str!("../../tests/fixtures/codex/deepseek-task.jsonl");
    const AUTH: &str = include_str!("../../tests/fixtures/codex/deepseek-auth-failure.jsonl");

    fn codex_with(provider: CodexProvider, model: &str) -> Codex {
        let lane = Lane {
            harness: HarnessKind::Codex,
            bin: "codex".into(),
            provider: Some(provider),
            model: Some(model.into()),
            effort: Effort::Medium,
            mode: Mode::Direct,
            quota_group: None,
            accept_vendor_terms_risk: false,
            script: None,
        };
        Codex::new("deepseek", &lane, Path::new("/data"))
    }

    fn codex() -> Codex {
        codex_with(CodexProvider::Deepseek, "deepseek-flash")
    }

    fn job() -> Job {
        Job {
            role: Role::Worker,
            prompt: "the task".into(),
            cwd: "/w".into(),
            run_dir: "/r".into(),
            raw_log: "/r/raw-1.log".into(),
            schema: None,
        }
    }

    fn output(text: &str, exit_code: Option<i32>) -> Output {
        Output {
            exit_code,
            stdout: text.lines().map(str::to_string).collect(),
            stderr_tail: String::new(),
        }
    }

    #[test]
    fn a_task_runs_in_its_own_codex_home_pointed_at_deepseek() {
        let spawn = codex().spawn(&job());

        assert_eq!(spawn.program, PathBuf::from("codex"));
        assert_eq!(
            spawn.args.join(" "),
            "exec --json --skip-git-repo-check --sandbox workspace-write --model deepseek-flash -"
        );
        assert_eq!(spawn.stdin.as_deref(), Some("the task"));
        assert_eq!(
            spawn.env_set,
            [(
                "CODEX_HOME".to_string(),
                "/data/onerig/codex/deepseek".to_string()
            )]
        );
        let (path, config) = &spawn.files[0];
        assert_eq!(
            path,
            &PathBuf::from("/data/onerig/codex/deepseek/config.toml")
        );
        assert!(config.starts_with("model = \"deepseek-flash\"\nmodel_provider = \"deepseek\"\n"));
        assert!(config.contains("base_url = \"https://api.deepseek.com/\"\nwire_api = \"responses\"\nenv_key = \"DEEPSEEK_API_KEY\"\n"));
        assert!(!config.contains("sk-"));
    }

    #[test]
    fn the_captured_task_streams_messages_commands_and_edits_and_ends_done() {
        let adapter = codex();

        let events: Vec<HarnessEvent> = TASK.lines().flat_map(|l| adapter.parse(l)).collect();
        let outcome = adapter.outcome(&job(), &output(TASK, Some(0)));

        assert_eq!(
            events[0],
            HarnessEvent::Started {
                model: Some("deepseek-flash".into())
            }
        );
        assert!(events.contains(&HarnessEvent::Tool {
            name: "shell".into(),
            detail: "/bin/zsh -c 'cat calc.py'".into()
        }));
        assert!(events.contains(&HarnessEvent::Tool {
            name: "edit".into(),
            detail: "/work/toy/calc.py, /work/toy/test_calc.py".into()
        }));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, HarnessEvent::Text { text } if text.contains("metadata")))
        );
        assert!(
            matches!(outcome, Outcome::Done { text, structured: None } if text.starts_with("Implemented `subtract(a, b)`"))
        );
    }

    #[test]
    fn the_captured_invalid_key_is_an_auth_failure_after_the_retries() {
        let outcome = codex().outcome(&job(), &output(AUTH, Some(1)));

        assert!(
            matches!(outcome, Outcome::Failed { kind: FailureKind::Auth, message, .. }
            if message.starts_with("unexpected status 401 Unauthorized"))
        );
    }

    #[test]
    fn an_empty_balance_is_quota_and_other_errors_are_unknown() {
        let balance = json!({ "type": "turn.failed", "error": { "message":
            "unexpected status 402 Payment Required: Insufficient Balance" } })
        .to_string();
        let overloaded =
            json!({ "type": "error", "message": "unexpected status 503 Service Unavailable" })
                .to_string();

        let quota = codex().outcome(&job(), &output(&balance, Some(1)));
        let unknown = codex().outcome(&job(), &output(&overloaded, Some(1)));

        assert!(matches!(
            quota,
            Outcome::Failed {
                kind: FailureKind::Quota,
                ..
            }
        ));
        assert!(
            matches!(unknown, Outcome::Failed { kind: FailureKind::Unknown, message, .. }
            if message.contains("503"))
        );
    }

    #[test]
    fn a_run_that_never_finished_its_turn_reports_its_exit_and_stderr() {
        let mut ended = output("{\"type\":\"turn.started\"}", Some(2));
        ended.stderr_tail = "panic\n".into();

        let outcome = codex().outcome(&job(), &ended);
        let killed = codex().outcome(&job(), &output("", None));

        assert!(
            matches!(outcome, Outcome::Failed { kind: FailureKind::Unknown, message, .. }
            if message == "codex ended without finishing its turn (exit code 2): panic")
        );
        assert!(matches!(killed, Outcome::Failed { message, .. }
            if message == "codex ended without finishing its turn (a signal)"));
    }

    fn probe(version: &str, help: &str, key_exit: i32) -> [Output; 3] {
        let out = |line: &str, exit| Output {
            exit_code: Some(exit),
            stdout: vec![line.into()],
            stderr_tail: String::new(),
        };
        [out(version, 0), out(help, 0), out("", key_exit)]
    }

    #[test]
    fn the_probe_needs_a_recent_codex_with_json_output_and_the_key() {
        let adapter = codex();

        assert_eq!(
            adapter.probe_result(&probe("codex-cli 0.171.0", "      --json", 0)),
            Ok(Probe {
                version: "codex-cli 0.171.0".into()
            })
        );
        assert!(matches!(
            adapter.probe_result(&probe("codex-cli 0.163.9", "--json", 0)),
            Err(ProbeError::Unsupported(_))
        ));
        assert!(matches!(
            adapter.probe_result(&probe("garbage", "--json", 0)),
            Err(ProbeError::Unsupported(_))
        ));
        assert!(matches!(
            adapter.probe_result(&probe("codex-cli 0.171.0", "no flags", 0)),
            Err(ProbeError::Unsupported(_))
        ));
        assert!(
            matches!(adapter.probe_result(&probe("codex-cli 0.171.0", "--json", 1)), Err(ProbeError::NotLoggedIn(m)) if m.starts_with("DEEPSEEK_API_KEY"))
        );
        assert!(matches!(
            adapter.probe_result(&[]),
            Err(ProbeError::Unsupported(_))
        ));
    }

    #[test]
    fn the_key_check_reads_the_variable_without_printing_it() {
        let spawns = codex().probe_spawns();

        assert_eq!(spawns[2].args, ["-c", "[ -n \"$DEEPSEEK_API_KEY\" ]"]);
        assert_eq!(codex().quota_spawn(), None);
    }

    #[test]
    fn versions_are_read_from_the_last_word() {
        assert_eq!(parse_version("codex-cli 0.171.0"), Some((0, 171)));
        assert_eq!(parse_version("1.2"), Some((1, 2)));
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("codex x.y"), None);
    }

    #[test]
    fn openai_provider_uses_default_codex_home_and_login_status() {
        let adapter = codex_with(CodexProvider::Openai, "gpt-5.4");
        let probes = adapter.probe_spawns();

        assert_eq!(probes[2].program, PathBuf::from("codex"));
        assert_eq!(probes[2].args, ["login", "status"]);
        assert!(probes[2].env_set.is_empty());
        assert!(matches!(
            adapter.probe_result(&probe("codex-cli 0.171.0", "--json", 1)),
            Err(ProbeError::NotLoggedIn(m)) if m.contains("codex login")
        ));
    }

    #[test]
    fn planner_role_uses_read_only_sandbox_and_schema_per_provider() {
        let schema = r#"{"type":"object"}"#;
        let planner_job = Job {
            role: Role::Planner,
            schema: Some(schema),
            ..job()
        };

        let openai_spawn = codex_with(CodexProvider::Openai, "gpt-5.4").spawn(&planner_job);
        let deepseek_spawn = codex().spawn(&planner_job);

        assert!(openai_spawn.env_set.is_empty());
        assert!(
            openai_spawn
                .args
                .contains(&"--ignore-user-config".to_string())
        );
        assert!(openai_spawn.args.contains(&"--ephemeral".to_string()));
        assert!(openai_spawn.args.contains(&"read-only".to_string()));
        assert!(openai_spawn.args.contains(&"--output-schema".to_string()));
        assert_eq!(
            openai_spawn.files[0].0,
            PathBuf::from("/r/raw-1.schema.json")
        );
        assert_eq!(openai_spawn.files[0].1, schema);
        assert!(
            openai_spawn
                .stdin
                .as_deref()
                .unwrap()
                .starts_with("You are the planner")
        );

        assert!(deepseek_spawn.args.contains(&"read-only".to_string()));
        assert!(
            deepseek_spawn
                .stdin
                .as_deref()
                .unwrap()
                .contains("JSON Schema")
        );
    }

    #[test]
    fn schema_outcome_extracts_json_or_fails_unknown() {
        let schema_job = Job {
            role: Role::Planner,
            schema: Some(r#"{"type":"object"}"#),
            ..job()
        };
        let valid = [
            json!({"type":"item.completed","item":{"type":"agent_message","text":"```json\n{\"tasks\":[]}\n```"}}).to_string(),
            json!({"type":"item.completed","item":{"type":"command_execution","command":"ls"}}).to_string(),
            json!({"type":"turn.completed"}).to_string(),
        ]
        .join("\n");
        let invalid = [
            json!({"type":"item.completed","item":{"type":"agent_message","text":"no json"}})
                .to_string(),
            json!({"type":"turn.completed"}).to_string(),
        ]
        .join("\n");

        let ok = codex().outcome(&schema_job, &output(&valid, Some(0)));
        let bad = codex().outcome(&schema_job, &output(&invalid, Some(0)));

        assert_eq!(
            ok,
            Outcome::Done {
                text: "```json\n{\"tasks\":[]}\n```".into(),
                structured: Some(json!({"tasks": []})),
            }
        );
        assert!(matches!(
            bad,
            Outcome::Failed {
                kind: FailureKind::Unknown,
                ..
            }
        ));
    }
}
