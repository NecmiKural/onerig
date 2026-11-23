//! State machine tests with the scripted runner against real repositories:
//! one test per pause reason and per path through a task.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::config::{self, Config};
use crate::fake::FakeRunner;

const NOW: u64 = 1_795_464_900_000;

struct Fixture {
    _tmp: tempfile::TempDir,
    main: PathBuf,
    data: PathBuf,
    script: PathBuf,
    config: Config,
    git: Git,
    stop: Arc<AtomicBool>,
}

/// A repository with one commit, fake lanes plan, review, w1 and w2, a manual
/// agy lane gemini, and the given worker chain and script steps.
fn fixture(workers: &str, max_attempts: u32, steps: Vec<Value>) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let main = root.join("shop");
    std::fs::create_dir(&main).unwrap();
    let git = Git::isolated();
    git.must(&main, &["init", "-q", "-b", "main"]);
    git.must(&main, &["config", "user.name", "Test"]);
    git.must(&main, &["config", "user.email", "test@example.invalid"]);
    std::fs::write(main.join("README"), "hello\n").unwrap();
    git.must(&main, &["add", "README"]);
    git.must(&main, &["commit", "-q", "-m", "first"]);
    let script = root.join("script.json");
    std::fs::write(&script, json!({ "steps": steps }).to_string()).unwrap();
    let fake = |name: &str| {
        format!(
            "[lanes.{name}]\nharness = \"fake\"\nscript = \"{}\"\n",
            script.display()
        )
    };
    let text = format!(
        "[run]\nmax_attempts = {max_attempts}\n\n[roles]\nplanner = [\"plan\"]\nreviewer = [\"review\"]\n\
         worker = {workers}\n\n{}{}{}{}[lanes.gemini]\nharness = \"agy\"\nmodel = \"m\"\n",
        fake("plan"),
        fake("review"),
        fake("w1"),
        fake("w2"),
    );
    Fixture {
        config: config::parse(&text, true).unwrap(),
        data: root.join("data"),
        _tmp: tmp,
        main,
        script,
        git,
        stop: Arc::new(AtomicBool::new(false)),
    }
}

type Ran = (Result<Finished, RunError>, Vec<LogRecord>);

enum Call<'c> {
    Start,
    Resume(Option<&'c str>),
}

impl Fixture {
    fn go(&self, call: Call<'_>, cwd: &Path) -> Ran {
        let mut runner = FakeRunner::new(&self.script, Arc::clone(&self.stop)).unwrap();
        let mut records = Vec::new();
        let clock = || NOW;
        let env = Env {
            git: &self.git,
            runner: &mut runner,
            clock: &clock,
            pid: 7,
            stop: &self.stop,
            sink: &mut |record| records.push(record.clone()),
            data_home: &self.data,
        };
        let result = self.call(call, cwd, env);
        (result, records)
    }

    fn call<'a>(&'a self, call: Call<'_>, cwd: &Path, env: Env<'a>) -> Result<Finished, RunError> {
        match call {
            Call::Start => start(&self.config, cwd, "the goal", env),
            Call::Resume(id) => resume(&self.config, cwd, id, env),
        }
    }

    fn start(&self) -> Finished {
        self.go(Call::Start, &self.main).0.unwrap()
    }

    fn resume(&self) -> Finished {
        self.go(Call::Resume(None), &self.main).0.unwrap()
    }

    fn subjects(&self, branch: &str) -> Vec<String> {
        self.git
            .must(
                &self.main,
                &["log", "--format=%s", &format!("main..{branch}")],
            )
            .lines()
            .rev()
            .map(str::to_string)
            .collect()
    }

    fn handover(&self, finished: &Finished, task: usize, attempt: u32) -> String {
        let dir = RunDir::open(&self.main, &finished.run_id).unwrap();
        std::fs::read_to_string(dir.handover_path(task, attempt)).unwrap()
    }
}

fn plan(tasks: usize) -> Value {
    let tasks: Vec<Value> = (1..=tasks)
        .map(|i| {
            json!({ "id": format!("t{i}"), "title": format!("Task {i}"),
                    "instructions": "Do it.", "acceptance": ["It is done"] })
        })
        .collect();
    json!({ "lane": "plan", "outcome": { "type": "done", "text": "", "structured": { "tasks": tasks } } })
}

fn work(lane: &str, file: &str) -> Value {
    json!({ "lane": lane, "writes": [[file, "content\n"]],
            "events": [{ "type": "text", "text": format!("{lane} wrote {file}") }],
            "outcome": { "type": "done", "text": "done", "structured": null } })
}

fn fail(lane: &str, kind: &str) -> Value {
    json!({ "lane": lane, "outcome": { "type": "failed", "kind": kind,
            "message": format!("{kind} happened"), "reset_at": null } })
}

fn approve() -> Value {
    json!({ "lane": "review", "outcome": { "type": "done", "text": "", "structured":
            { "verdict": "approve", "summary": "Good." } } })
}

fn revise(note: &str) -> Value {
    json!({ "lane": "review", "outcome": { "type": "done", "text": "", "structured":
            { "verdict": "revise", "summary": "No.", "issues": [{ "file": "a.txt", "note": note }] } } })
}

fn paused(finished: &Finished) -> &Paused {
    assert_eq!(finished.exit, Exit::Paused);
    finished.state.paused.as_ref().unwrap()
}

#[test]
fn a_plan_of_two_tasks_becomes_two_commits_on_the_run_branch() {
    let f = fixture(
        r#"["w1"]"#,
        5,
        vec![
            plan(2),
            work("w1", "a.txt"),
            approve(),
            work("w1", "b.txt"),
            approve(),
        ],
    );

    let finished = f.start();

    assert_eq!(finished.exit, Exit::Done);
    assert_eq!(f.subjects(&finished.state.branch), ["Task 1", "Task 2"]);
    assert_eq!(
        f.git
            .must(&f.main, &["rev-parse", "--abbrev-ref", "HEAD"])
            .trim(),
        "main"
    );
    assert!(!f.git.is_dirty(&f.main).unwrap());
    assert!(!finished.state.worktree.join(".onerig/handover.md").exists());
}

#[test]
fn a_revise_gives_the_next_attempt_the_issues_on_the_same_lane() {
    let f = fixture(
        r#"["w1", "w2"]"#,
        5,
        vec![
            plan(1),
            work("w1", "a.txt"),
            revise("Use b."),
            work("w1", "b.txt"),
            approve(),
        ],
    );

    let finished = f.start();

    assert_eq!(finished.exit, Exit::Done);
    let second = f.handover(&finished, 1, 2);
    assert!(second.contains("(review asked for changes)"));
    assert!(second.contains("# Review issues to fix\n- a.txt: Use b."));
    assert!(second.contains("+content"));
}

#[test]
fn quota_on_the_first_worker_moves_the_task_to_the_next_lane() {
    let f = fixture(
        r#"["w1", "w2"]"#,
        5,
        vec![plan(1), fail("w1", "quota"), work("w2", "a.txt"), approve()],
    );

    let finished = f.start();

    assert_eq!(finished.exit, Exit::Done);
    assert!(f.handover(&finished, 1, 2).contains("(quota exhausted)"));
}

#[test]
fn a_timeout_skips_the_lane_for_the_task() {
    let f = fixture(
        r#"["w1", "w2"]"#,
        5,
        vec![
            plan(1),
            fail("w1", "timeout"),
            work("w2", "a.txt"),
            approve(),
        ],
    );

    let (result, records) = f.go(Call::Start, &f.main);

    assert_eq!(result.unwrap().exit, Exit::Done);
    assert!(records.iter().any(|r| matches!(&r.entry,
        LogEntry::Policy(PolicyEvent::LaneSkipped { lane }) if lane == "w1")));
}

#[test]
fn an_unknown_failure_is_retried_once_on_the_same_lane_then_skipped() {
    let f = fixture(
        r#"["w1", "w2"]"#,
        5,
        vec![
            plan(1),
            fail("w1", "unknown"),
            fail("w1", "unknown"),
            work("w2", "a.txt"),
            approve(),
        ],
    );

    assert_eq!(f.start().exit, Exit::Done);
}

#[test]
fn a_worker_whose_probe_failed_is_not_used() {
    let f = fixture(
        r#"["w1", "w2"]"#,
        5,
        vec![plan(1), work("w2", "a.txt"), approve()],
    );
    let mut script: Value =
        serde_json::from_str(&std::fs::read_to_string(&f.script).unwrap()).unwrap();
    script["preflight"] = json!({ "w1": { "error": { "type": "missing", "message": "gone" } } });
    std::fs::write(&f.script, script.to_string()).unwrap();

    assert_eq!(f.start().exit, Exit::Done);
}

#[test]
fn no_available_worker_pauses_with_every_reason_and_a_hand_over() {
    let f = fixture(r#"["w1"]"#, 5, vec![plan(1), fail("w1", "quota")]);

    let finished = f.start();

    let paused = paused(&finished);
    assert_eq!(paused.reason, PauseReason::NoLane);
    assert_eq!(paused.resume_phase, Phase::Executing);
    assert!(paused.message.starts_with("w1: quota exhausted"));
    assert!(paused.message.contains("By hand: cd "));
    let note =
        std::fs::read_to_string(finished.state.worktree.join(".onerig/handover.md")).unwrap();
    assert!(note.contains("tell the user to run: onerig resume"));
}

#[test]
fn a_manual_lane_pauses_for_a_person_and_stays_after_a_revise() {
    let f = fixture(
        r#"["w1", "gemini"]"#,
        5,
        vec![plan(1), fail("w1", "quota"), revise("Again."), approve()],
    );

    let first = f.start();
    assert_eq!(paused(&first).reason, PauseReason::Handover);
    assert_eq!(paused(&first).resume_phase, Phase::Reviewing);
    assert!(
        paused(&first)
            .message
            .contains("&& agy\nFirst message: Read .onerig/handover.md")
    );
    std::fs::write(first.state.worktree.join("by-hand.txt"), "x\n").unwrap();

    let second = f.resume();
    assert_eq!(paused(&second).reason, PauseReason::Handover);
    assert!(f.handover(&second, 1, 3).contains("- a.txt: Again."));

    let third = f.resume();
    assert_eq!(third.exit, Exit::Done);
    assert_eq!(f.subjects(&third.state.branch), ["Task 1"]);
}

#[test]
fn planner_quota_pauses_to_plan_again() {
    let f = fixture(r#"["w1"]"#, 5, vec![fail("plan", "quota")]);

    let finished = f.start();

    assert_eq!(paused(&finished).reason, PauseReason::Quota);
    assert_eq!(paused(&finished).resume_phase, Phase::Planning);
}

#[test]
fn a_worker_that_is_not_logged_in_pauses_the_run() {
    let f = fixture(r#"["w1", "w2"]"#, 5, vec![plan(1), fail("w1", "auth")]);

    let finished = f.start();

    assert_eq!(paused(&finished).reason, PauseReason::Auth);
    assert_eq!(paused(&finished).resume_phase, Phase::Executing);
}

#[test]
fn the_attempt_budget_pauses_a_task_that_is_never_approved() {
    let f = fixture(
        r#"["w1"]"#,
        2,
        vec![
            plan(1),
            work("w1", "a.txt"),
            revise("1"),
            work("w1", "b.txt"),
            revise("2"),
        ],
    );

    let finished = f.start();

    assert_eq!(paused(&finished).reason, PauseReason::AttemptsExhausted);
    assert_eq!(finished.state.attempts_used, 2);
}

#[test]
fn a_reviewer_that_fails_twice_pauses_without_using_an_attempt() {
    let f = fixture(
        r#"["w1"]"#,
        5,
        vec![
            plan(1),
            work("w1", "a.txt"),
            fail("review", "unknown"),
            fail("review", "unknown"),
        ],
    );

    let finished = f.start();

    assert_eq!(paused(&finished).reason, PauseReason::JobFailed);
    assert_eq!(paused(&finished).resume_phase, Phase::Reviewing);
    assert_eq!(finished.state.attempts_used, 1);
}

#[test]
fn a_failed_commit_keeps_the_approval_and_resume_only_commits() {
    let f = fixture(
        r#"["w1"]"#,
        5,
        vec![plan(1), work("w1", "a.txt"), approve()],
    );
    let hook = f.main.join(".git/hooks/pre-commit");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "#!/bin/sh\necho refused >&2\nexit 1\n").unwrap();
    std::process::Command::new("chmod")
        .args(["+x"])
        .arg(&hook)
        .status()
        .unwrap();

    let first = f.start();
    assert_eq!(paused(&first).reason, PauseReason::GitError);
    assert!(first.state.approved);
    std::fs::remove_file(&hook).unwrap();

    let second = f.resume();
    assert_eq!(second.exit, Exit::Done);
    assert_eq!(f.subjects(&second.state.branch), ["Task 1"]);
}

#[test]
fn a_stop_during_a_worker_pauses_as_interrupted_and_resume_starts_a_new_attempt() {
    let f = fixture(
        r#"["w1"]"#,
        5,
        vec![
            plan(1),
            json!({ "lane": "w1", "hang": true, "writes": [["half.txt", "x\n"]] }),
            work("w1", "a.txt"),
            approve(),
        ],
    );
    // Stop only once the hanging step has written its file, so the stop lands
    // inside the worker job and not during planning.
    let half = f
        .data
        .join("onerig/worktrees/shop")
        .join(crate::store::run_id(NOW, 7))
        .join("half.txt");
    let stop = Arc::clone(&f.stop);
    let stopper = thread::spawn(move || {
        while !half.exists() {
            thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
    });

    let first = f.start();
    stopper.join().unwrap();
    assert_eq!(paused(&first).reason, PauseReason::Interrupted);
    assert_eq!(first.state.last_end, Some(LastEnd::Interrupted));
    assert!(f.handover(&first, 1, 2).contains("(interrupted)"));
    f.stop.store(false, Ordering::Relaxed);

    let second = f.resume();
    assert_eq!(second.exit, Exit::Done);
}

#[test]
fn a_planner_that_fails_twice_fails_the_run() {
    let f = fixture(
        r#"["w1"]"#,
        5,
        vec![fail("plan", "unknown"), fail("plan", "timeout")],
    );

    let finished = f.start();

    assert_eq!(finished.exit, Exit::Failed);
}

#[test]
fn a_planner_probe_failure_stops_before_anything_is_created() {
    let f = fixture(r#"["w1"]"#, 5, vec![]);
    std::fs::write(
        &f.script,
        json!({ "preflight": { "plan": { "error":
        { "type": "not_logged_in", "message": "log in" } } }, "steps": [] })
        .to_string(),
    )
    .unwrap();

    let (result, _) = f.go(Call::Start, &f.main);

    assert!(matches!(result, Err(RunError::Probe { lane, .. }) if lane == "plan"));
    assert!(!f.main.join(".onerig").exists());
}

#[test]
fn resume_refuses_a_finished_run_and_a_repository_without_runs() {
    let f = fixture(
        r#"["w1"]"#,
        5,
        vec![plan(1), work("w1", "a.txt"), approve()],
    );
    let (nothing, _) = f.go(Call::Resume(None), &f.main);
    let done = f.start();

    let (again, _) = f.go(Call::Resume(Some(&done.run_id)), &f.main);

    assert!(matches!(nothing, Err(RunError::NothingToResume)));
    assert!(matches!(again, Err(RunError::AlreadyFinished(_))));
}

#[test]
fn resume_works_from_inside_the_runs_worktree() {
    let f = fixture(
        r#"["w1"]"#,
        5,
        vec![plan(1), fail("w1", "quota"), work("w1", "a.txt"), approve()],
    );
    let first = f.start();

    let (second, _) = f.go(Call::Resume(None), &first.state.worktree);

    assert_eq!(second.unwrap().exit, Exit::Done);
}
