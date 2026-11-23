//! End-to-end tests of the `onerig` binary with scripted lanes in a
//! temporary repository (design build step 1).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
}

/// A repository with one commit, a fake configuration and the given script.
fn world(steps: Vec<Value>) -> World {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let repo = root.join("shop");
    std::fs::create_dir_all(&repo).unwrap();
    let world = World {
        _tmp: tmp,
        root,
        repo,
    };
    world.git(&["init", "-q", "-b", "main"]);
    world.git(&["config", "user.name", "Test"]);
    world.git(&["config", "user.email", "test@example.invalid"]);
    std::fs::write(world.repo.join("README"), "hello\n").unwrap();
    world.git(&["add", "README"]);
    world.git(&["commit", "-q", "-m", "first"]);
    let script = world.root.join("script.json");
    std::fs::write(&script, json!({ "steps": steps }).to_string()).unwrap();
    let lane = |name: &str| {
        format!(
            "[lanes.{name}]\nharness = \"fake\"\nscript = \"{}\"\n",
            script.display()
        )
    };
    let config = format!(
        "[roles]\nplanner = [\"plan\"]\nreviewer = [\"review\"]\nworker = [\"w1\"]\n\n{}{}{}",
        lane("plan"),
        lane("review"),
        lane("w1")
    );
    std::fs::write(world.root.join("config.toml"), config).unwrap();
    world
}

impl World {
    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.repo)
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("ONERIG_TEST", "1");
        command
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self.command("git").args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn onerig(&self, args: &[&str]) -> Command {
        let mut command = self.command(env!("CARGO_BIN_EXE_onerig"));
        let config = self.root.join("config.toml");
        command
            .args(args)
            .arg("--plain")
            .arg("--config")
            .arg(config);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.onerig(args).output().unwrap()
    }

    fn spawn(&self, args: &[&str]) -> Child {
        self.onerig(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// The single run branch's commit subjects, oldest first.
    fn run_commits(&self) -> Vec<String> {
        let branches = self.git(&["branch", "--list", "onerig/*", "--format=%(refname:short)"]);
        let branch = branches.trim();
        let log = self.git(&["log", "--format=%s", &format!("main..{branch}")]);
        log.lines().rev().map(str::to_string).collect()
    }

    /// Waits until a file appears in the run's worktree.
    fn wait_for_worktree_file(&self, name: &str) {
        let worktrees = self.root.join("data/onerig/worktrees/shop");
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(20) {
            let found = std::fs::read_dir(&worktrees)
                .map(|entries| entries.flatten().any(|e| e.path().join(name).exists()))
                .unwrap_or(false);
            if found {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("{name} never appeared in a worktree");
    }
}

fn plan(tasks: usize) -> Value {
    let tasks: Vec<Value> = (1..=tasks)
        .map(|i| {
            json!({ "id": format!("t{i}"), "title": format!("Task {i}"),
                         "instructions": "Do it.", "acceptance": ["Done"] })
        })
        .collect();
    json!({ "lane": "plan", "outcome": { "type": "done", "text": "", "structured": { "tasks": tasks } } })
}

fn work(file: &str) -> Value {
    json!({ "lane": "w1", "writes": [[file, "x\n"]],
            "outcome": { "type": "done", "text": "ok", "structured": null } })
}

fn hang() -> Value {
    json!({ "lane": "w1", "hang": true, "writes": [["half.txt", "x\n"]] })
}

fn approve() -> Value {
    json!({ "lane": "review", "outcome": { "type": "done", "text": "",
            "structured": { "verdict": "approve", "summary": "ok" } } })
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn last_line(text: &str) -> &str {
    text.lines().last().unwrap_or("")
}

fn signal(child: &Child, name: &str) {
    let status = Command::new("kill")
        .arg(format!("-{name}"))
        .arg(child.id().to_string())
        .status();
    assert!(status.unwrap().success());
}

fn is_clean(world: &World, path: &Path) -> bool {
    let output = world
        .command("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    output.stdout.is_empty()
}

#[test]
fn a_run_commits_one_task_after_another_and_leaves_the_checkout_alone() {
    let world = world(vec![
        plan(2),
        work("a.txt"),
        approve(),
        work("b.txt"),
        approve(),
    ]);

    let output = world.run(&["run", "add two files"]);

    let text = stdout(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        last_line(&text).contains(" done      branch onerig/"),
        "{text}"
    );
    assert!(text.contains(" plan      2 tasks"));
    assert_eq!(world.run_commits(), ["Task 1", "Task 2"]);
    assert_eq!(
        world.git(&["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "main"
    );
    assert!(is_clean(&world, &world.repo));
}

#[test]
fn a_run_killed_during_a_worker_is_finished_by_resume() {
    let world = world(vec![plan(1), hang(), work("a.txt"), approve()]);
    let mut child = world.spawn(&["run", "goal"]);
    world.wait_for_worktree_file("half.txt");

    signal(&child, "KILL");
    child.wait().unwrap();
    let resumed = world.run(&["resume"]);

    let text = stdout(&resumed);
    assert_eq!(
        resumed.status.code(),
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(world.run_commits(), ["Task 1"]);
}

#[test]
fn sigterm_during_a_worker_pauses_the_run_as_interrupted() {
    let world = world(vec![plan(1), hang(), work("a.txt"), approve()]);
    let child = world.spawn(&["run", "goal"]);
    world.wait_for_worktree_file("half.txt");

    signal(&child, "TERM");
    let output = child.wait_with_output().unwrap();

    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(3), "{text}");
    assert!(
        text.contains(" paused    interrupted: stopped by the user or a signal"),
        "{text}"
    );
    assert!(text.contains("Afterwards: onerig resume "), "{text}");
}

#[test]
fn a_missing_configuration_prints_an_example_and_exits_2() {
    let world = world(vec![]);
    let output = world
        .command(env!("CARGO_BIN_EXE_onerig"))
        .args(["run", "goal", "--config"])
        .arg(world.root.join("nowhere.toml"))
        .output()
        .unwrap();

    let error = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2));
    assert!(error.contains("no configuration file at"), "{error}");
    assert!(error.contains("[lanes.plan]"), "{error}");
}

#[test]
fn resume_without_a_run_explains_and_exits_2() {
    let world = world(vec![]);

    let output = world.run(&["resume"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no run to resume"));
}
