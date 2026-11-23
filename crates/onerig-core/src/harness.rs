//! The real `Runner`: one adapter per lane, run through the process driver.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::adapter::{Adapter, Output, Spawn};
use crate::process::{self, Ended, Limits};
use crate::runner::{
    FailureKind, HarnessEvent, Job, Outcome, Probe, ProbeError, QuotaReport, Runner,
};

/// Probes and quota calls must answer within this time.
const PROBE_LIMIT: Duration = Duration::from_secs(30);

pub struct HarnessRunner {
    adapters: BTreeMap<String, Box<dyn Adapter>>,
    limits: Limits,
    stop: Arc<AtomicBool>,
    /// Working directory of probe and quota calls: the main checkout.
    probe_cwd: PathBuf,
}

impl HarnessRunner {
    pub fn new(
        adapters: BTreeMap<String, Box<dyn Adapter>>,
        limits: Limits,
        stop: Arc<AtomicBool>,
        probe_cwd: PathBuf,
    ) -> HarnessRunner {
        HarnessRunner {
            adapters,
            limits,
            stop,
            probe_cwd,
        }
    }

    /// Runs a probe or quota call; `Err` when it gives no output to judge.
    fn quick(&self, spawn: &Spawn) -> Result<Output, String> {
        let limits = Limits {
            idle: PROBE_LIMIT,
            total: PROBE_LIMIT,
        };
        match process::run(
            spawn,
            &self.probe_cwd,
            limits,
            &self.stop,
            &mut |_| {},
            &mut |_| {},
        ) {
            Ended::Exited(output) => Ok(output),
            Ended::SpawnFailed(message) => Err(message),
            Ended::IdleTimeout(_) | Ended::JobTimeout(_) => Err(format!(
                "`{}` did not answer within {} s",
                spawn.program.display(),
                PROBE_LIMIT.as_secs()
            )),
            Ended::Stopped(_) => Err("stopped".to_string()),
        }
    }
}

impl Runner for HarnessRunner {
    fn preflight(&mut self, lane: &str) -> Result<Probe, ProbeError> {
        let adapter = self
            .adapters
            .get(lane)
            .ok_or_else(|| ProbeError::Missing(format!("lane {lane} has no adapter")))?;
        let outputs = adapter
            .probe_spawns()
            .iter()
            .map(|spawn| self.quick(spawn))
            .collect::<Result<Vec<_>, _>>()
            .map_err(ProbeError::Missing)?;
        adapter.probe_result(&outputs)
    }

    fn quota(&mut self, lane: &str) -> Option<QuotaReport> {
        let adapter = self.adapters.get(lane)?;
        let output = self.quick(&adapter.quota_spawn()?).ok()?;
        adapter.quota_result(&output)
    }

    fn run(
        &mut self,
        lane: &str,
        job: &Job,
        on_spawn: &mut dyn FnMut(i32),
        on_event: &mut dyn FnMut(HarnessEvent),
    ) -> Outcome {
        let Some(adapter) = self.adapters.get(lane) else {
            return Outcome::failed(FailureKind::Unknown, format!("lane {lane} has no adapter"));
        };
        let spawn = adapter.spawn(job);
        let ended = process::run(
            &spawn,
            &job.cwd,
            self.limits,
            &self.stop,
            on_spawn,
            &mut |line| {
                adapter.parse(line).into_iter().for_each(&mut *on_event);
            },
        );
        let (outcome, output) = judge(adapter.as_ref(), job, ended, self.limits);
        if let (Outcome::Failed { .. }, Some(output)) = (&outcome, &output) {
            // The raw output only helps a person diagnose the failure; not
            // being able to keep it changes nothing about the run.
            let _ = write_raw_log(&job.raw_log, output);
        }
        outcome
    }
}

/// The outcome of an ended job, with its output when there is one.
fn judge(
    adapter: &dyn Adapter,
    job: &Job,
    ended: Ended,
    limits: Limits,
) -> (Outcome, Option<Output>) {
    match ended {
        Ended::Exited(output) => (adapter.outcome(job, &output), Some(output)),
        Ended::IdleTimeout(output) => (
            Outcome::failed(
                FailureKind::Timeout,
                format!("no output for {}", minutes(limits.idle)),
            ),
            Some(output),
        ),
        Ended::JobTimeout(output) => (
            Outcome::failed(
                FailureKind::Timeout,
                format!("still running after {}", minutes(limits.total)),
            ),
            Some(output),
        ),
        Ended::Stopped(output) => (
            Outcome::failed(FailureKind::Unknown, "stopped".to_string()),
            Some(output),
        ),
        Ended::SpawnFailed(message) => (Outcome::failed(FailureKind::Unknown, message), None),
    }
}

fn minutes(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 && seconds.is_multiple_of(60) {
        format!("{} min", seconds / 60)
    } else {
        format!("{seconds} s")
    }
}

fn write_raw_log(path: &Path, output: &Output) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let exit = output.exit_code.map_or_else(
        || "none (killed by a signal)".to_string(),
        |code| code.to_string(),
    );
    let text = format!(
        "exit code: {exit}\n--- stdout ---\n{}\n--- stderr (tail) ---\n{}",
        output.stdout.join("\n"),
        output.stderr_tail
    );
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::Role;
    use std::sync::Mutex;

    /// Runs `sh -c <script>`; each stdout line becomes a text event, and the
    /// outcome is Done for exit 0.
    struct Shell {
        script: &'static str,
        probes: Vec<&'static str>,
        quota: Option<&'static str>,
    }

    fn sh(script: &str) -> Spawn {
        Spawn {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            ..Spawn::default()
        }
    }

    impl Adapter for Shell {
        fn probe_spawns(&self) -> Vec<Spawn> {
            self.probes.iter().map(|script| sh(script)).collect()
        }

        fn probe_result(&self, outputs: &[Output]) -> Result<Probe, ProbeError> {
            Ok(Probe {
                version: outputs
                    .iter()
                    .flat_map(|o| o.stdout.clone())
                    .collect::<Vec<_>>()
                    .join("+"),
            })
        }

        fn quota_spawn(&self) -> Option<Spawn> {
            self.quota.map(sh)
        }

        fn quota_result(&self, output: &Output) -> Option<QuotaReport> {
            Some(QuotaReport {
                remaining_fraction: output.stdout.first()?.parse().ok()?,
                reset_at: None,
            })
        }

        fn spawn(&self, _job: &Job) -> Spawn {
            sh(self.script)
        }

        fn parse(&self, line: &str) -> Vec<HarnessEvent> {
            vec![HarnessEvent::Text {
                text: line.to_string(),
            }]
        }

        fn outcome(&self, _job: &Job, output: &Output) -> Outcome {
            match output.exit_code {
                Some(0) => Outcome::Done {
                    text: output.stdout.join("\n"),
                    structured: None,
                },
                _ => Outcome::failed(FailureKind::Unknown, "exit".to_string()),
            }
        }
    }

    fn runner(adapter: Shell, limits: Limits) -> HarnessRunner {
        let adapters: BTreeMap<String, Box<dyn Adapter>> =
            BTreeMap::from([("lane".to_string(), Box::new(adapter) as Box<dyn Adapter>)]);
        HarnessRunner::new(adapters, limits, Arc::default(), std::env::temp_dir())
    }

    fn shell(script: &'static str) -> Shell {
        Shell {
            script,
            probes: vec![],
            quota: None,
        }
    }

    const ROOMY: Limits = Limits {
        idle: Duration::from_secs(10),
        total: Duration::from_secs(10),
    };

    fn job(dir: &Path) -> Job {
        Job {
            role: Role::Worker,
            prompt: String::new(),
            cwd: dir.to_path_buf(),
            run_dir: dir.to_path_buf(),
            raw_log: dir.join("raw-1.log"),
            schema: None,
        }
    }

    #[test]
    fn a_job_streams_its_events_reports_its_group_and_ends_with_the_adapters_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let events = Mutex::new(Vec::new());
        let mut groups = Vec::new();

        let outcome = runner(shell("echo one; echo two"), ROOMY).run(
            "lane",
            &job(dir.path()),
            &mut |pgid| groups.push(pgid),
            &mut |event| events.lock().unwrap().push(event),
        );

        assert_eq!(
            outcome,
            Outcome::Done {
                text: "one\ntwo".into(),
                structured: None
            }
        );
        assert_eq!(events.lock().unwrap().len(), 2);
        assert_eq!(groups.len(), 1);
        assert!(!dir.path().join("raw-1.log").exists());
    }

    #[test]
    fn a_failed_job_keeps_its_raw_output() {
        let dir = tempfile::tempdir().unwrap();

        let outcome = runner(shell("echo out; echo err >&2; exit 4"), ROOMY).run(
            "lane",
            &job(dir.path()),
            &mut |_| {},
            &mut |_| {},
        );

        assert!(matches!(
            outcome,
            Outcome::Failed {
                kind: FailureKind::Unknown,
                ..
            }
        ));
        let raw = std::fs::read_to_string(dir.path().join("raw-1.log")).unwrap();
        assert_eq!(
            raw,
            "exit code: 4\n--- stdout ---\nout\n--- stderr (tail) ---\nerr\n"
        );
    }

    #[test]
    fn a_silent_job_times_out_without_asking_the_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let limits = Limits {
            idle: Duration::from_millis(200),
            total: Duration::from_secs(10),
        };

        let outcome = runner(shell("sleep 5"), limits).run(
            "lane",
            &job(dir.path()),
            &mut |_| {},
            &mut |_| {},
        );

        assert_eq!(
            outcome,
            Outcome::failed(FailureKind::Timeout, "no output for 0 s".to_string())
        );
        assert!(dir.path().join("raw-1.log").exists());
    }

    #[test]
    fn a_lane_without_an_adapter_fails_its_probe_and_its_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = runner(shell("true"), ROOMY);

        let probe = runner.preflight("other");
        let outcome = runner.run("other", &job(dir.path()), &mut |_| {}, &mut |_| {});

        assert!(matches!(probe, Err(ProbeError::Missing(_))));
        assert!(matches!(
            outcome,
            Outcome::Failed {
                kind: FailureKind::Unknown,
                ..
            }
        ));
        assert_eq!(runner.quota("other"), None);
    }

    #[test]
    fn probes_pass_every_output_to_the_adapter() {
        let adapter = Shell {
            probes: vec!["echo 1.2", "echo ok"],
            ..shell("true")
        };

        let probe = runner(adapter, ROOMY).preflight("lane");

        assert_eq!(
            probe,
            Ok(Probe {
                version: "1.2+ok".into()
            })
        );
    }

    #[test]
    fn a_probe_program_that_cannot_start_means_the_harness_is_missing() {
        let adapter = Shell {
            probes: vec![],
            ..shell("true")
        };
        let mut runner = runner(adapter, ROOMY);
        runner
            .adapters
            .insert("missing".into(), Box::new(Missing) as Box<dyn Adapter>);

        assert!(
            matches!(runner.preflight("missing"), Err(ProbeError::Missing(m)) if m.contains("onerig-no-such-harness"))
        );
    }

    #[test]
    fn quota_comes_from_the_adapters_quota_call() {
        let adapter = Shell {
            quota: Some("echo 0.25"),
            ..shell("true")
        };
        let mut runner = runner(adapter, ROOMY);

        assert_eq!(
            runner.quota("lane"),
            Some(QuotaReport {
                remaining_fraction: 0.25,
                reset_at: None
            })
        );
    }

    #[test]
    fn durations_read_in_minutes_when_they_are_whole_minutes() {
        assert_eq!(minutes(Duration::from_secs(300)), "5 min");
        assert_eq!(minutes(Duration::from_secs(90)), "90 s");
    }

    struct Missing;

    impl Adapter for Missing {
        fn probe_spawns(&self) -> Vec<Spawn> {
            vec![Spawn {
                program: "/nonexistent/onerig-no-such-harness".into(),
                ..Spawn::default()
            }]
        }
        fn probe_result(&self, _: &[Output]) -> Result<Probe, ProbeError> {
            unreachable!("a spawn failure never reaches the adapter")
        }
        fn quota_spawn(&self) -> Option<Spawn> {
            None
        }
        fn quota_result(&self, _: &Output) -> Option<QuotaReport> {
            None
        }
        fn spawn(&self, _: &Job) -> Spawn {
            Spawn::default()
        }
        fn parse(&self, _: &str) -> Vec<HarnessEvent> {
            vec![]
        }
        fn outcome(&self, _: &Job, _: &Output) -> Outcome {
            Outcome::failed(FailureKind::Unknown, String::new())
        }
    }
}
