//! A scripted `Runner` for tests (design section 16). It plays back a JSON
//! script instead of starting a harness, and keeps its position in a file so
//! a later `onerig resume` process continues where an earlier one stopped.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde::Deserialize;

use crate::runner::{
    FailureKind, HarnessEvent, Job, Outcome, Probe, ProbeError, QuotaReport, Runner,
};

const HANG_SLICE: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FakeError {
    #[error("cannot read the fake script {}: {message}", path.display())]
    Read { path: PathBuf, message: String },
    #[error("the fake script {} is not valid: {message}", path.display())]
    Script { path: PathBuf, message: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Script {
    #[serde(default)]
    preflight: BTreeMap<String, PreflightAnswer>,
    #[serde(default)]
    quota: BTreeMap<String, QuotaReport>,
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightAnswer {
    version: Option<String>,
    error: Option<ProbeError>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    lane: String,
    #[serde(default)]
    writes: Vec<(String, String)>,
    #[serde(default)]
    events: Vec<HarnessEvent>,
    #[serde(default)]
    hang: bool,
    outcome: Option<Outcome>,
}

#[derive(Debug)]
pub struct FakeRunner {
    script: Script,
    position_file: PathBuf,
    position: usize,
    stop: Arc<AtomicBool>,
}

impl FakeRunner {
    pub fn new(script_path: &Path, stop: Arc<AtomicBool>) -> Result<FakeRunner, FakeError> {
        let read_error = |path: &Path, e: std::io::Error| FakeError::Read {
            path: path.to_path_buf(),
            message: e.to_string(),
        };
        let text = std::fs::read_to_string(script_path).map_err(|e| read_error(script_path, e))?;
        let script = parse_script(&text).map_err(|message| FakeError::Script {
            path: script_path.to_path_buf(),
            message,
        })?;
        let position_file = position_file(script_path);
        let position = match std::fs::read_to_string(&position_file) {
            Ok(text) => text.trim().parse().unwrap_or(0),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(read_error(&position_file, e)),
        };
        Ok(FakeRunner {
            script,
            position_file,
            position,
            stop,
        })
    }

    fn consume(&mut self) {
        self.position += 1;
        // A position that cannot be stored shows up as a replayed step in the
        // test that uses it.
        let _ = std::fs::write(&self.position_file, self.position.to_string());
    }

    fn hang(&self) -> Outcome {
        while !self.stop.load(Ordering::Relaxed) {
            thread::sleep(HANG_SLICE);
        }
        unknown("stopped".to_string())
    }
}

fn parse_script(text: &str) -> Result<Script, String> {
    let script: Script = serde_json::from_str(text).map_err(|e| e.to_string())?;
    match script
        .steps
        .iter()
        .position(|s| !s.hang && s.outcome.is_none())
    {
        Some(index) => Err(format!(
            "step {} needs an outcome or \"hang\": true",
            index + 1
        )),
        None => Ok(script),
    }
}

/// `<script>.pos`, next to the script.
fn position_file(script: &Path) -> PathBuf {
    let mut name = OsString::from(script.as_os_str());
    name.push(".pos");
    PathBuf::from(name)
}

fn unknown(message: String) -> Outcome {
    Outcome::failed(FailureKind::Unknown, message)
}

/// Writes go inside the worktree only.
fn apply_writes(cwd: &Path, writes: &[(String, String)]) -> Result<(), String> {
    for (relative, text) in writes {
        let path = Path::new(relative);
        let inside = path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        if !inside {
            return Err(format!("refuses to write outside the worktree: {relative}"));
        }
        let full = cwd.join(path);
        let write = || -> std::io::Result<()> {
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&full, text)
        };
        write().map_err(|e| format!("cannot write {relative}: {e}"))?;
    }
    Ok(())
}

impl Runner for FakeRunner {
    fn preflight(&mut self, lane: &str) -> Result<Probe, ProbeError> {
        match self.script.preflight.get(lane) {
            Some(PreflightAnswer {
                error: Some(error), ..
            }) => Err(error.clone()),
            Some(PreflightAnswer {
                version: Some(version),
                ..
            }) => Ok(Probe {
                version: version.clone(),
            }),
            _ => Ok(Probe {
                version: "fake".to_string(),
            }),
        }
    }

    fn quota(&mut self, lane: &str) -> Option<QuotaReport> {
        self.script.quota.get(lane).copied()
    }

    fn run(
        &mut self,
        lane: &str,
        job: &Job,
        _on_spawn: &mut dyn FnMut(i32),
        on_event: &mut dyn FnMut(HarnessEvent),
    ) -> Outcome {
        let number = self.position + 1;
        let Some(step) = self.script.steps.get(self.position) else {
            return unknown(format!(
                "the fake script has no step {number} (it has {})",
                self.script.steps.len()
            ));
        };
        if step.lane != lane {
            return unknown(format!(
                "fake step {number} is for lane {}, but lane {lane} was called",
                step.lane
            ));
        }
        let (writes, events, hang, outcome) = (
            step.writes.clone(),
            step.events.clone(),
            step.hang,
            step.outcome.clone(),
        );
        self.consume();
        if let Err(message) = apply_writes(&job.cwd, &writes) {
            return unknown(format!("fake step {number} {message}"));
        }
        events.into_iter().for_each(&mut *on_event);
        match outcome {
            Some(outcome) if !hang => outcome,
            _ => self.hang(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::Role;
    use serde_json::json;

    struct Setup {
        dir: tempfile::TempDir,
        script: PathBuf,
    }

    impl Setup {
        fn runner(&self) -> FakeRunner {
            FakeRunner::new(&self.script, Arc::new(AtomicBool::new(false))).unwrap()
        }

        fn job(&self) -> Job {
            Job {
                role: Role::Worker,
                prompt: "p".into(),
                cwd: self.dir.path().join("wt"),
                run_dir: self.dir.path().join("run"),
                raw_log: self.dir.path().join("run/raw-1.log"),
                schema: None,
            }
        }
    }

    fn setup(script: serde_json::Value) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("wt")).unwrap();
        let path = dir.path().join("script.json");
        std::fs::write(&path, script.to_string()).unwrap();
        Setup { dir, script: path }
    }

    fn done(text: &str) -> serde_json::Value {
        json!({ "type": "done", "text": text, "structured": null })
    }

    fn run(runner: &mut FakeRunner, lane: &str, job: &Job) -> (Outcome, Vec<HarnessEvent>) {
        let mut events = Vec::new();
        let outcome = runner.run(lane, job, &mut |_| {}, &mut |event| events.push(event));
        (outcome, events)
    }

    fn failure_message(outcome: Outcome) -> String {
        match outcome {
            Outcome::Failed {
                kind: FailureKind::Unknown,
                message,
                ..
            } => message,
            other => panic!("expected an Unknown failure, got {other:?}"),
        }
    }

    #[test]
    fn a_lane_missing_from_preflight_passes_and_listed_answers_are_given() {
        let setup = setup(json!({
            "preflight": {
                "old": { "version": "1.2" },
                "gone": { "error": { "type": "missing", "message": "codex not found" } }
            },
            "steps": []
        }));
        let mut runner = setup.runner();

        assert_eq!(
            runner.preflight("any"),
            Ok(Probe {
                version: "fake".into()
            })
        );
        assert_eq!(
            runner.preflight("old"),
            Ok(Probe {
                version: "1.2".into()
            })
        );
        assert_eq!(
            runner.preflight("gone"),
            Err(ProbeError::Missing("codex not found".into()))
        );
    }

    #[test]
    fn quota_is_reported_only_for_listed_lanes() {
        let setup = setup(json!({
            "quota": { "gemini": { "remaining_fraction": 0.5, "reset_at": 9 } },
            "steps": []
        }));
        let mut runner = setup.runner();

        assert_eq!(runner.quota("deepseek"), None);
        assert_eq!(
            runner.quota("gemini"),
            Some(QuotaReport {
                remaining_fraction: 0.5,
                reset_at: Some(9)
            })
        );
    }

    #[test]
    fn steps_play_in_order_with_their_events_and_writes() {
        let setup = setup(json!({ "steps": [
            { "lane": "plan", "outcome": done("planned") },
            { "lane": "deepseek", "writes": [["src/a.txt", "hello"]],
              "events": [{ "type": "text", "text": "working" }],
              "outcome": { "type": "failed", "kind": "quota", "message": "empty", "reset_at": null } }
        ]}));
        let mut runner = setup.runner();
        let job = setup.job();

        let (first, _) = run(&mut runner, "plan", &job);
        let (second, events) = run(&mut runner, "deepseek", &job);

        assert_eq!(
            first,
            Outcome::Done {
                text: "planned".into(),
                structured: None
            }
        );
        assert!(matches!(
            second,
            Outcome::Failed {
                kind: FailureKind::Quota,
                ..
            }
        ));
        assert_eq!(
            events,
            [HarnessEvent::Text {
                text: "working".into()
            }]
        );
        assert_eq!(
            std::fs::read_to_string(job.cwd.join("src/a.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn writes_outside_the_worktree_are_refused() {
        for path in ["../escape.txt", "/tmp/escape.txt", "a/../../escape.txt"] {
            let setup = setup(json!({ "steps": [
                { "lane": "w", "writes": [[path, "x"]], "outcome": done("") }
            ]}));
            let mut runner = setup.runner();

            let (outcome, _) = run(&mut runner, "w", &setup.job());

            assert!(
                failure_message(outcome).contains("outside the worktree"),
                "{path}"
            );
        }
    }

    #[test]
    fn a_step_for_another_lane_fails_and_is_not_consumed() {
        let setup = setup(json!({ "steps": [{ "lane": "plan", "outcome": done("ok") }] }));
        let mut runner = setup.runner();
        let job = setup.job();

        let (wrong, _) = run(&mut runner, "review", &job);
        let (right, _) = run(&mut runner, "plan", &job);

        assert_eq!(
            failure_message(wrong),
            "fake step 1 is for lane plan, but lane review was called"
        );
        assert!(matches!(right, Outcome::Done { .. }));
    }

    #[test]
    fn a_call_after_the_last_step_fails() {
        let setup = setup(json!({ "steps": [] }));
        let mut runner = setup.runner();

        let (outcome, _) = run(&mut runner, "plan", &setup.job());

        assert_eq!(
            failure_message(outcome),
            "the fake script has no step 1 (it has 0)"
        );
    }

    #[test]
    fn a_new_runner_continues_at_the_stored_position() {
        let setup = setup(json!({ "steps": [
            { "lane": "plan", "outcome": done("one") },
            { "lane": "plan", "outcome": done("two") }
        ]}));
        let job = setup.job();
        run(&mut setup.runner(), "plan", &job);

        let (outcome, _) = run(&mut setup.runner(), "plan", &job);

        assert_eq!(
            outcome,
            Outcome::Done {
                text: "two".into(),
                structured: None
            }
        );
    }

    #[test]
    fn a_hanging_step_waits_for_the_stop_flag_after_being_consumed() {
        let setup = setup(json!({ "steps": [
            { "lane": "w", "hang": true, "writes": [["partial.txt", "x"]] },
            { "lane": "w", "outcome": done("after") }
        ]}));
        let stop = Arc::new(AtomicBool::new(false));
        let mut runner = FakeRunner::new(&setup.script, Arc::clone(&stop)).unwrap();
        let job = setup.job();

        let outcome = thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(100));
                stop.store(true, Ordering::Relaxed);
            });
            run(&mut runner, "w", &job).0
        });

        assert_eq!(failure_message(outcome), "stopped");
        assert!(job.cwd.join("partial.txt").exists());
        let (next, _) = run(&mut setup.runner(), "w", &job);
        assert_eq!(
            next,
            Outcome::Done {
                text: "after".into(),
                structured: None
            }
        );
    }

    #[test]
    fn a_step_without_an_outcome_must_hang() {
        let setup = setup(json!({ "steps": [{ "lane": "w" }] }));

        let error = FakeRunner::new(&setup.script, Arc::new(AtomicBool::new(false))).unwrap_err();

        assert!(matches!(error, FakeError::Script { message, .. } if message.contains("step 1")));
    }

    #[test]
    fn a_missing_script_is_a_read_error() {
        let error = FakeRunner::new(Path::new("/nonexistent/s.json"), Arc::default()).unwrap_err();

        assert!(matches!(error, FakeError::Read { .. }));
    }
}
