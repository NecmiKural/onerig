//! The subprocess driver. It runs one described call to its end and knows
//! nothing about harnesses or runs: it starts the child in its own process
//! group, streams its output, and kills the whole group when a limit is hit,
//! the stop flag is set, or the child is done.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::adapter::{Output, Spawn};

/// How long a child gets to leave after SIGTERM, and how long its pipes may
/// stay open after it has exited.
const GRACE: Duration = Duration::from_secs(2);
/// How often limits and the stop flag are checked.
const TICK: Duration = Duration::from_millis(20);
/// How long output is still collected after the process group is killed.
const DRAIN: Duration = Duration::from_millis(500);
const STDERR_TAIL: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Longest time without a new stdout line.
    pub idle: Duration,
    /// Longest time the job may run.
    pub total: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    Exited(Output),
    IdleTimeout(Output),
    JobTimeout(Output),
    /// The stop flag was set.
    Stopped(Output),
    SpawnFailed(String),
}

pub fn run(
    spawn: &Spawn,
    cwd: &Path,
    limits: Limits,
    stop: &AtomicBool,
    on_spawn: &mut dyn FnMut(i32),
    on_line: &mut dyn FnMut(&str),
) -> Ended {
    if let Err(message) = write_files(spawn, cwd) {
        return Ended::SpawnFailed(message);
    }
    let mut job = match Job::start(spawn, cwd) {
        Ok(job) => job,
        Err(message) => return Ended::SpawnFailed(message),
    };
    on_spawn(job.pgid);
    let reason = job.wait_for_end(limits, stop, on_line);
    finish(job, reason, on_line)
}

fn finish(mut job: Job, reason: Reason, on_line: &mut dyn FnMut(&str)) -> Ended {
    if reason == Reason::Exited {
        signal_group(job.pgid, libc::SIGKILL);
    } else {
        job.terminate(on_line);
    }
    job.drain(on_line);
    let output = job.output();
    match reason {
        Reason::Exited => Ended::Exited(output),
        Reason::Idle => Ended::IdleTimeout(output),
        Reason::Total => Ended::JobTimeout(output),
        Reason::Stopped => Ended::Stopped(output),
    }
}

/// Relative paths are taken from `cwd`.
fn write_files(spawn: &Spawn, cwd: &Path) -> Result<(), String> {
    for (path, text) in &spawn.files {
        let full = cwd.join(path);
        let write = || -> std::io::Result<()> {
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&full, text)
        };
        write().map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    Ok(())
}

/// An exact variable name, or a prefix followed by `*`.
fn is_removed(name: &OsStr, patterns: &[String]) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    patterns
        .iter()
        .any(|pattern| match pattern.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == pattern,
        })
}

fn signal_group(pgid: i32, signal: i32) {
    // SAFETY: killpg only sends a signal; a group that is gone gives ESRCH,
    // which needs no handling.
    unsafe {
        libc::killpg(pgid, signal);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    Exited,
    Idle,
    Total,
    Stopped,
}

enum Piece {
    Line(String),
    Stderr(Vec<u8>),
    Closed,
}

struct Job {
    child: Child,
    pgid: i32,
    pieces: Receiver<Piece>,
    open_pipes: u8,
    stdout: Vec<String>,
    stderr: Vec<u8>,
    status: Option<ExitStatus>,
    started: Instant,
    last_line: Instant,
    exited_at: Option<Instant>,
}

impl Job {
    fn start(spawn: &Spawn, cwd: &Path) -> Result<Job, String> {
        let mut command = Command::new(&spawn.program);
        command
            .args(&spawn.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        for (name, _) in std::env::vars_os() {
            if is_removed(&name, &spawn.env_remove) {
                command.env_remove(&name);
            }
        }
        command.envs(spawn.env_set.iter().map(|(k, v)| (k, v)));
        let mut child = command
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", spawn.program.display()))?;
        feed_stdin(&mut child, spawn.stdin.clone());
        let pieces = read_pipes(&mut child);
        let now = Instant::now();
        Ok(Job {
            pgid: child.id() as i32,
            child,
            pieces,
            open_pipes: 2,
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: None,
            started: now,
            last_line: now,
            exited_at: None,
        })
    }

    fn wait_for_end(
        &mut self,
        limits: Limits,
        stop: &AtomicBool,
        on_line: &mut dyn FnMut(&str),
    ) -> Reason {
        loop {
            self.step(on_line);
            if let Some(reason) = self.reason(limits, stop) {
                return reason;
            }
        }
    }

    fn step(&mut self, on_line: &mut dyn FnMut(&str)) {
        match self.pieces.recv_timeout(TICK) {
            Ok(piece) => self.take(piece, on_line),
            Err(RecvTimeoutError::Disconnected) => thread::sleep(TICK),
            Err(RecvTimeoutError::Timeout) => {}
        }
        self.poll_exit();
    }

    fn take(&mut self, piece: Piece, on_line: &mut dyn FnMut(&str)) {
        match piece {
            Piece::Line(line) => {
                on_line(&line);
                self.stdout.push(line);
                self.last_line = Instant::now();
            }
            Piece::Stderr(bytes) => {
                self.stderr.extend(bytes);
                let excess = self.stderr.len().saturating_sub(STDERR_TAIL);
                self.stderr.drain(..excess);
            }
            Piece::Closed => self.open_pipes = self.open_pipes.saturating_sub(1),
        }
    }

    fn poll_exit(&mut self) {
        if self.status.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.status = Some(status);
            self.exited_at = Some(Instant::now());
        }
    }

    fn reason(&self, limits: Limits, stop: &AtomicBool) -> Option<Reason> {
        let done = |at: Instant| self.open_pipes == 0 || at.elapsed() >= GRACE;
        if self.exited_at.is_some_and(done) {
            Some(Reason::Exited)
        } else if stop.load(Ordering::Relaxed) {
            Some(Reason::Stopped)
        } else if self.last_line.elapsed() >= limits.idle {
            Some(Reason::Idle)
        } else if self.started.elapsed() >= limits.total {
            Some(Reason::Total)
        } else {
            None
        }
    }

    /// SIGTERM to the group, SIGKILL after the grace period; the group is
    /// killed in any case so no descendant outlives the job.
    fn terminate(&mut self, on_line: &mut dyn FnMut(&str)) {
        signal_group(self.pgid, libc::SIGTERM);
        let deadline = Instant::now() + GRACE;
        while self.status.is_none() && Instant::now() < deadline {
            self.step(on_line);
        }
        signal_group(self.pgid, libc::SIGKILL);
        if self.status.is_none() {
            self.status = self.child.wait().ok();
        }
    }

    fn drain(&mut self, on_line: &mut dyn FnMut(&str)) {
        let deadline = Instant::now() + DRAIN;
        while self.open_pipes > 0 && Instant::now() < deadline {
            match self.pieces.recv_timeout(TICK) {
                Ok(piece) => self.take(piece, on_line),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn output(self) -> Output {
        Output {
            exit_code: self.status.and_then(|status| status.code()),
            stdout: self.stdout,
            stderr_tail: String::from_utf8_lossy(&self.stderr).into_owned(),
        }
    }
}

/// Writes stdin from its own thread, so a child that never reads it cannot
/// block the job, and closes it.
fn feed_stdin(child: &mut Child, text: Option<String>) {
    let Some(mut stdin) = child.stdin.take() else {
        return;
    };
    if let Some(text) = text {
        // A child that exits without reading makes this fail with a broken
        // pipe, which is not an error of the job.
        thread::spawn(move || stdin.write_all(text.as_bytes()));
    }
}

fn read_pipes(child: &mut Child) -> Receiver<Piece> {
    let (sender, receiver) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        let sender = sender.clone();
        thread::spawn(move || read_lines(stdout, &sender));
    }
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || read_chunks(stderr, &sender));
    }
    receiver
}

fn read_lines(pipe: impl Read, sender: &Sender<Piece>) {
    let mut reader = BufReader::new(pipe);
    let mut buffer = Vec::new();
    while matches!(reader.read_until(b'\n', &mut buffer), Ok(n) if n > 0) {
        let line = buffer.strip_suffix(b"\n").unwrap_or(&buffer);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let _ = sender.send(Piece::Line(String::from_utf8_lossy(line).into_owned()));
        buffer.clear();
    }
    let _ = sender.send(Piece::Closed);
}

fn read_chunks(mut pipe: impl Read, sender: &Sender<Piece>) {
    let mut chunk = [0u8; 4096];
    while let Ok(n @ 1..) = pipe.read(&mut chunk) {
        let _ = sender.send(Piece::Stderr(chunk[..n].to_vec()));
    }
    let _ = sender.send(Piece::Closed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::Instant;

    /// Limits no passing test comes near; a regression fails instead of hanging.
    const ROOMY: Limits = Limits {
        idle: Duration::from_secs(10),
        total: Duration::from_secs(10),
    };

    struct Seen {
        ended: Ended,
        pgids: Vec<i32>,
        lines: Vec<String>,
        took: Duration,
    }

    fn sh(script: &str) -> Spawn {
        Spawn {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            ..Spawn::default()
        }
    }

    fn drive(spawn: &Spawn, cwd: &Path, limits: Limits, stop: &AtomicBool) -> Seen {
        let (mut pgids, mut lines) = (Vec::new(), Vec::new());
        let started = Instant::now();
        let ended = run(
            spawn,
            cwd,
            limits,
            stop,
            &mut |pgid| pgids.push(pgid),
            &mut |line| lines.push(line.to_owned()),
        );
        Seen {
            ended,
            pgids,
            lines,
            took: started.elapsed(),
        }
    }

    fn drive_here(spawn: &Spawn, limits: Limits) -> Seen {
        drive(spawn, Path::new("."), limits, &AtomicBool::new(false))
    }

    fn exited(ended: Ended) -> Output {
        match ended {
            Ended::Exited(output) => output,
            other => panic!("expected an exit, got {other:?}"),
        }
    }

    /// Whether a process disappears within a second.
    fn is_gone(pid: &str) -> bool {
        (0..100).any(|_| {
            thread::sleep(Duration::from_millis(10));
            !Command::new("/bin/kill")
                .args(["-0", pid])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        })
    }

    #[test]
    fn a_child_that_exits_reports_its_lines_its_exit_code_and_its_stderr() {
        let spawn = sh("echo one; echo two; echo oops >&2; exit 3");

        let seen = drive_here(&spawn, ROOMY);

        assert_eq!(
            seen.ended,
            Ended::Exited(Output {
                exit_code: Some(3),
                stdout: vec!["one".into(), "two".into()],
                stderr_tail: "oops\n".into(),
            })
        );
        assert!(seen.took < GRACE, "took {:?}", seen.took);
    }

    #[test]
    fn every_stdout_line_reaches_on_line_without_its_newline() {
        let spawn = sh("echo first; printf 'no newline at the end'");

        let seen = drive_here(&spawn, ROOMY);

        assert_eq!(seen.lines, ["first", "no newline at the end"]);
    }

    #[test]
    fn stdin_is_delivered_to_the_child_and_closed() {
        let spawn = Spawn {
            stdin: Some("alpha\nbeta\n".into()),
            ..sh("cat; echo end")
        };

        let seen = drive_here(&spawn, ROOMY);

        assert_eq!(exited(seen.ended).stdout, ["alpha", "beta", "end"]);
    }

    #[test]
    fn without_stdin_the_child_reads_end_of_file() {
        let spawn = sh("cat; echo end");

        let seen = drive_here(&spawn, ROOMY);

        assert_eq!(exited(seen.ended).stdout, ["end"]);
    }

    #[test]
    fn stdin_the_child_never_reads_does_not_block_the_job() {
        let spawn = Spawn {
            stdin: Some("x".repeat(1 << 20)),
            ..sh("exit 0")
        };

        let seen = drive_here(&spawn, ROOMY);

        assert_eq!(exited(seen.ended).exit_code, Some(0));
    }

    #[test]
    fn env_remove_drops_a_prefix_from_the_inherited_environment_and_env_set_is_applied_last() {
        assert!(
            std::env::var_os("CARGO_PKG_NAME").is_some(),
            "this test needs the variables cargo sets for a test run"
        );
        let spawn = Spawn {
            env_remove: vec!["CARGO_PKG_*".into()],
            env_set: vec![("CARGO_PKG_VERSION".into(), "set".into())],
            ..sh(
                r#"echo "${CARGO_PKG_NAME-removed} ${CARGO_PKG_VERSION-removed} ${CARGO_MANIFEST_DIR:+kept}""#,
            )
        };

        let seen = drive_here(&spawn, ROOMY);

        assert_eq!(seen.lines, ["removed set kept"]);
    }

    #[test]
    fn env_remove_matches_an_exact_name_or_the_prefix_before_a_star() {
        let patterns = ["CLAUDE_*".to_owned(), "ANTHROPIC_API_KEY".to_owned()];

        let removed = |name: &str| is_removed(OsStr::new(name), &patterns);

        assert!(removed("CLAUDE_CODE_ENTRYPOINT"));
        assert!(removed("ANTHROPIC_API_KEY"));
        assert!(!removed("ANTHROPIC_API_KEY_FILE"));
        assert!(!removed("MY_CLAUDE_HOME"));
        assert!(!removed("PATH"));
    }

    #[test]
    fn files_are_written_with_their_parent_directories_before_the_child_starts() {
        let dir = tempfile::tempdir().unwrap();
        let spawn = Spawn {
            files: vec![
                (dir.path().join("a/b/absolute.txt"), "absolute".into()),
                ("c/relative.txt".into(), "relative".into()),
            ],
            ..sh("cat a/b/absolute.txt; echo; cat c/relative.txt")
        };

        let seen = drive(&spawn, dir.path(), ROOMY, &AtomicBool::new(false));

        assert_eq!(seen.lines, ["absolute", "relative"]);
    }

    #[test]
    fn a_file_that_cannot_be_written_fails_the_spawn_before_anything_starts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blocker"), "").unwrap();
        let spawn = Spawn {
            files: vec![("blocker/prompt.txt".into(), "x".into())],
            ..sh("exit 0")
        };

        let seen = drive(&spawn, dir.path(), ROOMY, &AtomicBool::new(false));

        assert!(
            matches!(&seen.ended, Ended::SpawnFailed(message) if message.contains("blocker/prompt.txt")),
            "{:?}",
            seen.ended
        );
        assert!(seen.pgids.is_empty());
    }

    #[test]
    fn a_missing_program_is_a_spawn_failure_that_names_the_program() {
        let spawn = Spawn {
            program: "/nonexistent/onerig-harness".into(),
            ..Spawn::default()
        };

        let seen = drive_here(&spawn, ROOMY);

        assert!(
            matches!(&seen.ended, Ended::SpawnFailed(message) if message.contains("/nonexistent/onerig-harness")),
            "{:?}",
            seen.ended
        );
        assert!(seen.pgids.is_empty());
    }

    #[test]
    fn on_spawn_receives_the_process_group_the_child_leads() {
        let spawn = sh("ps -o pgid= -p $$");

        let seen = drive_here(&spawn, ROOMY);

        assert_eq!(seen.pgids.len(), 1);
        assert!(seen.pgids[0] > 0);
        assert_eq!(seen.lines[0].trim(), seen.pgids[0].to_string());
    }

    #[test]
    fn a_child_with_no_new_stdout_line_ends_as_an_idle_timeout() {
        let limits = Limits {
            idle: Duration::from_millis(300),
            ..ROOMY
        };
        let spawn = sh("echo one; while :; do echo noise >&2; sleep 0.05; done");

        let seen = drive_here(&spawn, limits);

        let Ended::IdleTimeout(output) = seen.ended else {
            panic!("expected an idle timeout, got {:?}", seen.ended);
        };
        assert_eq!(output.stdout, ["one"]);
        assert_eq!(output.exit_code, None);
        assert!(output.stderr_tail.contains("noise"));
        assert!(
            seen.took < GRACE,
            "SIGTERM alone should end it: {:?}",
            seen.took
        );
    }

    #[test]
    fn a_child_that_keeps_talking_past_the_total_limit_ends_as_a_job_timeout() {
        let limits = Limits {
            idle: Duration::from_millis(500),
            total: Duration::from_secs(1),
        };
        let spawn = sh("while :; do echo tick; sleep 0.05; done");

        let seen = drive_here(&spawn, limits);

        let Ended::JobTimeout(output) = seen.ended else {
            panic!("expected a job timeout, got {:?}", seen.ended);
        };
        assert!(output.stdout.len() > 1);
        assert!(output.stdout.iter().all(|line| line == "tick"));
        assert!(seen.took >= limits.total);
    }

    #[test]
    fn a_child_that_ignores_sigterm_is_killed_with_its_descendants_after_the_grace_period() {
        let limits = Limits {
            idle: Duration::from_millis(300),
            ..ROOMY
        };
        let spawn = sh("trap '' TERM; sleep 30 & echo $!; wait");

        let seen = drive_here(&spawn, limits);

        let Ended::IdleTimeout(output) = seen.ended else {
            panic!("expected an idle timeout, got {:?}", seen.ended);
        };
        assert_eq!(output.exit_code, None);
        assert!(seen.took >= GRACE, "took {:?}", seen.took);
        assert!(seen.took < 2 * GRACE, "took {:?}", seen.took);
        assert!(is_gone(&output.stdout[0]));
    }

    #[test]
    fn a_descendant_that_keeps_stdout_open_does_not_hang_the_job_and_is_killed() {
        let spawn = sh("sleep 30 & echo $!");

        let seen = drive_here(&spawn, ROOMY);

        let output = exited(seen.ended);
        assert_eq!(output.exit_code, Some(0));
        assert!(seen.took >= GRACE, "took {:?}", seen.took);
        assert!(seen.took < 2 * GRACE, "took {:?}", seen.took);
        assert!(is_gone(&output.stdout[0]));
    }

    #[test]
    fn the_stop_flag_set_from_another_thread_ends_the_job_as_stopped() {
        let stop = AtomicBool::new(false);
        let spawn = sh("echo started; sleep 30");

        let seen = thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(300));
                stop.store(true, Ordering::Relaxed);
            });
            drive(&spawn, Path::new("."), ROOMY, &stop)
        });

        assert!(
            matches!(&seen.ended, Ended::Stopped(output) if output.stdout == ["started"]),
            "{:?}",
            seen.ended
        );
        assert!(seen.took < GRACE, "took {:?}", seen.took);
    }

    #[test]
    fn output_that_is_not_utf8_is_decoded_lossily() {
        let spawn = sh(r"printf '\377 out\n'; printf '\377 err' >&2");

        let seen = drive_here(&spawn, ROOMY);

        let output = exited(seen.ended);
        assert_eq!(output.stdout, ["\u{fffd} out"]);
        assert_eq!(output.stderr_tail, "\u{fffd} err");
    }

    #[test]
    fn only_the_last_16_kib_of_stderr_are_kept() {
        let spawn = sh("yes 0123456789 | head -n 3000 >&2; echo last >&2");

        let seen = drive_here(&spawn, ROOMY);

        let tail = exited(seen.ended).stderr_tail;
        assert_eq!(tail.len(), 16 * 1024);
        assert!(tail.ends_with("0123456789\nlast\n"));
    }

    #[test]
    fn a_child_killed_by_a_signal_has_no_exit_code() {
        let spawn = sh("kill -KILL $$");

        let seen = drive_here(&spawn, ROOMY);

        assert_eq!(exited(seen.ended).exit_code, None);
    }

    #[test]
    fn a_child_that_closes_its_pipes_and_runs_on_still_reports_its_exit_code() {
        let spawn = sh("exec >&- 2>&-; sleep 0.4; exit 7");

        let seen = drive_here(&spawn, ROOMY);

        assert_eq!(exited(seen.ended).exit_code, Some(7));
    }
}
