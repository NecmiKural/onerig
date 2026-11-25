//! The run state machine of design sections 5 and 6: plan, execute, review,
//! commit, pause and resume. It talks to harnesses only through `Runner`, so
//! it is tested with the scripted runner.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::halt::Halt;
use crate::plan::{
    FormatError, PLAN_SCHEMA, Plan, Task, VERDICT_SCHEMA, Verdict, VerdictKind, parse_plan,
    parse_verdict,
};
use crate::policy::{self, AfterFailure, LaneView, Selected, Selection};
use crate::prompt::{self, Previous, WorkerPrompt};
use crate::runner::{FailureKind, HarnessEvent, Job, Outcome, ProbeError, Role, Runner};
use crate::store::{
    LaneMark, LastEnd, Lock, LockError, LogEntry, LogRecord, PauseReason, Paused, Phase,
    PolicyEvent, RunDir, STATE_VERSION, State, StateEvent, StoreError,
};
use crate::worktree::{Committed, Git, GitError, worktree_path};

/// How many times a planner or reviewer job is tried before the run gives up.
const ASK_TRIES: usize = 2;

/// What a run needs from the world. The binary passes the real things; tests
/// pass a scripted runner, a fixed clock and a temporary data home.
pub struct Env<'a> {
    pub git: &'a Git,
    pub runner: &'a mut dyn Runner,
    /// Unix milliseconds.
    pub clock: &'a dyn Fn() -> u64,
    pub pid: u32,
    pub stop: &'a Halt,
    /// Receives every log record as it is written.
    pub sink: &'a mut dyn FnMut(&LogRecord),
    /// `$XDG_DATA_HOME` or its default; worktrees go below it.
    pub data_home: &'a Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Done,
    Failed,
    Paused,
}

#[derive(Debug)]
pub struct Finished {
    pub exit: Exit,
    pub run_id: String,
    pub state: State,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("lane {lane} cannot run: {message}")]
    Probe { lane: String, message: String },
    #[error("there is no run to resume in this repository")]
    NothingToResume,
    #[error("run {0} has already finished")]
    AlreadyFinished(String),
}

/// What a new run starts from.
#[derive(Debug, Clone)]
pub struct NewRun<'g> {
    pub goal: &'g str,
    /// A plan the owner already approved; the run then skips planning.
    pub plan: Option<Plan>,
    /// The ref the run's branch starts from; the main checkout's HEAD if `None`.
    pub base: Option<&'g str>,
}

/// Starts a new run for `goal` in the repository that contains `cwd`.
pub fn start<'a>(
    config: &'a Config,
    cwd: &Path,
    goal: &str,
    env: Env<'a>,
) -> Result<Finished, RunError> {
    let new = NewRun {
        goal,
        plan: None,
        base: None,
    };
    start_with(config, cwd, new, env)
}

/// Starts a new run as `new` describes.
pub fn start_with<'a>(
    config: &'a Config,
    cwd: &Path,
    new: NewRun<'_>,
    env: Env<'a>,
) -> Result<Finished, RunError> {
    let main = env.git.main_checkout(cwd)?;
    env.git.preflight(&main)?;
    let probes = probe_lanes(config, env.runner)?;
    let (dir, lock, state) = create_run(&env, main, new.goal, new.base)?;
    let mut machine = Machine::new(config, env, dir, lock, state, probes, None);
    machine.note_run();
    machine.begin(new.plan)?;
    machine.finish()
}

/// Creates the run's directory, lock, branch and worktree.
fn create_run(
    env: &Env<'_>,
    main: PathBuf,
    goal: &str,
    base: Option<&str>,
) -> Result<(RunDir, Lock, State), RunError> {
    let now = (env.clock)();
    let run_id = crate::store::run_id(now, env.pid);
    let dir = RunDir::create(&main, &run_id)?;
    let lock = Lock::acquire(&dir)?;
    env.git.ensure_excluded(&main)?;
    let worktree = worktree_path(env.data_home, &main, &run_id);
    let branch = format!("onerig/{run_id}");
    env.git
        .create_run_worktree(&main, &worktree, &branch, base.unwrap_or("HEAD"))?;
    let state = new_state(&run_id, goal, now, main, worktree, branch);
    Ok((dir, lock, state))
}

/// Continues the given run, or the latest one that has not finished.
pub fn resume<'a>(
    config: &'a Config,
    cwd: &Path,
    run_id: Option<&str>,
    env: Env<'a>,
) -> Result<Finished, RunError> {
    let (dir, lock, mut state) = open_run(env.git, cwd, run_id)?;
    let probes = probe_lanes(config, env.runner)?;
    let plan = state.task_index.map(|_| dir.read_plan()).transpose()?;
    prepare_resume(&mut state);
    let mut machine = Machine::new(config, env, dir, lock, state, probes, plan);
    machine.note_run();
    machine.note_plan();
    machine.note(LogEntry::State(StateEvent::Phase {
        phase: machine.state.phase,
    }));
    machine.save()?;
    machine.finish()
}

/// Finds the run, takes its lock and reads its state.
fn open_run(
    git: &Git,
    cwd: &Path,
    run_id: Option<&str>,
) -> Result<(RunDir, Lock, State), RunError> {
    let main = git.main_checkout(cwd)?;
    let run_id = given_or_latest(&main, run_id)?;
    let dir = RunDir::open(&main, &run_id)?;
    let lock = Lock::acquire(&dir)?;
    let state = dir.read_state()?;
    if exit_of(state.phase).is_some_and(|exit| exit != Exit::Paused) {
        return Err(RunError::AlreadyFinished(run_id));
    }
    Ok((dir, lock, state))
}

fn given_or_latest(main: &Path, run_id: Option<&str>) -> Result<String, RunError> {
    match run_id {
        Some(id) => Ok(id.to_string()),
        None => crate::store::latest_resumable(main)?.ok_or(RunError::NothingToResume),
    }
}

fn new_state(
    run_id: &str,
    goal: &str,
    now: u64,
    repo: PathBuf,
    worktree: PathBuf,
    branch: String,
) -> State {
    State {
        v: STATE_VERSION,
        run_id: run_id.to_string(),
        goal: goal.to_string(),
        updated_at: now,
        repo,
        worktree,
        branch,
        phase: Phase::Planning,
        paused: None,
        task_index: None,
        task_base: None,
        attempt: None,
        attempts_used: 0,
        lane: None,
        unknown_streak: 0,
        skipped: Vec::new(),
        last_end: None,
        approved: false,
        issues: Vec::new(),
        last_output_tail: String::new(),
        job_seq: 0,
        lanes: BTreeMap::new(),
    }
}

/// A run found in a working phase had no live process: it was interrupted.
fn prepare_resume(state: &mut State) {
    let resume_phase = match state.paused.take() {
        Some(paused) if state.phase == Phase::Paused => paused.resume_phase,
        _ => {
            if state.phase == Phase::Executing {
                state.last_end = Some(LastEnd::Interrupted);
            }
            state.phase
        }
    };
    state.phase = resume_phase;
    state
        .lanes
        .values_mut()
        .for_each(|mark| *mark = LaneMark::default());
    state.skipped.clear();
    state.unknown_streak = 0;
    state.attempts_used = 0;
}

/// Planner and reviewer must be usable, or the run cannot start; a worker
/// lane that fails its probe is only unavailable. Manual and off lanes are
/// never probed.
fn probe_lanes(config: &Config, runner: &mut dyn Runner) -> Result<Probes, RunError> {
    let mut probes = Probes::default();
    let mut seen = HashSet::new();
    for lane in [&config.roles.planner, &config.roles.reviewer] {
        if !seen.insert(lane.clone()) {
            continue;
        }
        let result = runner.preflight(lane);
        if let Err(error) = &result {
            return Err(RunError::Probe {
                lane: lane.clone(),
                message: error.to_string(),
            });
        }
        probes.results.push((lane.clone(), result));
    }
    for name in &config.roles.worker {
        if !seen.insert(name.clone()) {
            continue;
        }
        let lane = &config.lanes[name];
        if lane.is_off() || lane.is_manual() {
            continue;
        }
        let result = runner.preflight(name);
        if let Err(error) = &result {
            probes.failed.insert(name.clone(), error.to_string());
        }
        probes.results.push((name.clone(), result));
    }
    Ok(probes)
}

#[derive(Default)]
struct Probes {
    results: Vec<(String, Result<crate::runner::Probe, ProbeError>)>,
    /// Worker lanes whose probe failed, with the message.
    failed: BTreeMap<String, String>,
}

/// Appends records to the run log and passes them to the sink. A write error
/// is kept and reported when the current step ends.
struct Log<'a> {
    dir: RunDir,
    sink: &'a mut dyn FnMut(&LogRecord),
    clock: &'a dyn Fn() -> u64,
    error: Option<StoreError>,
}

impl Log<'_> {
    fn record(
        &mut self,
        task: Option<usize>,
        attempt: Option<u32>,
        lane: Option<&str>,
        entry: LogEntry,
    ) {
        let record = LogRecord {
            ts: (self.clock)(),
            task,
            attempt,
            lane: lane.map(str::to_string),
            entry,
        };
        if let Err(error) = self.dir.append_log(&record) {
            self.error.get_or_insert(error);
        }
        (self.sink)(&record);
    }
}

/// The answer of a planner or reviewer job.
enum Asked<T> {
    Answer(T),
    Pause(PauseReason, String, Option<u64>),
    Failed(String),
    Stopped,
}

/// Why a planner or reviewer answer cannot be used.
enum Unusable {
    Pause(PauseReason, String, Option<u64>),
    Retry(String),
}

fn classify<T>(
    outcome: Outcome,
    parse: fn(&serde_json::Value) -> Result<T, FormatError>,
) -> Result<T, Unusable> {
    match outcome {
        Outcome::Failed {
            kind: FailureKind::Quota,
            message,
            reset_at,
        } => Err(Unusable::Pause(PauseReason::Quota, message, reset_at)),
        Outcome::Failed {
            kind: FailureKind::Auth,
            message,
            ..
        } => Err(Unusable::Pause(PauseReason::Auth, message, None)),
        Outcome::Failed { message, .. } => Err(Unusable::Retry(message)),
        Outcome::Done {
            structured: Some(value),
            ..
        } => parse(&value).map_err(|error| Unusable::Retry(error.to_string())),
        Outcome::Done {
            structured: None, ..
        } => Err(Unusable::Retry(
            "the answer had no structured result".to_string(),
        )),
    }
}

struct Machine<'a> {
    config: &'a Config,
    git: &'a Git,
    runner: &'a mut dyn Runner,
    stop: &'a Halt,
    clock: &'a dyn Fn() -> u64,
    log: Log<'a>,
    /// Held for the whole run; records the running harness's process group.
    lock: Lock,
    probes: Probes,
    plan: Option<Plan>,
    state: State,
}

impl<'a> Machine<'a> {
    fn new(
        config: &'a Config,
        env: Env<'a>,
        dir: RunDir,
        lock: Lock,
        state: State,
        probes: Probes,
        plan: Option<Plan>,
    ) -> Machine<'a> {
        let mut machine = Machine {
            config,
            git: env.git,
            runner: env.runner,
            stop: env.stop,
            clock: env.clock,
            log: Log {
                dir,
                sink: env.sink,
                clock: env.clock,
                error: None,
            },
            lock,
            probes,
            plan,
            state,
        };
        machine.log_probes();
        machine
    }

    fn log_probes(&mut self) {
        let results = std::mem::take(&mut self.probes.results);
        for (lane, result) in results {
            let (ok, message) = match result {
                Ok(probe) => (true, probe.version),
                Err(error) => (false, error.to_string()),
            };
            self.note(LogEntry::Policy(PolicyEvent::Probe { lane, ok, message }));
        }
    }

    fn finish(mut self) -> Result<Finished, RunError> {
        let exit = self.drive()?;
        Ok(Finished {
            exit,
            run_id: self.state.run_id.clone(),
            state: self.state,
        })
    }

    fn drive(&mut self) -> Result<Exit, RunError> {
        loop {
            if let Some(exit) = exit_of(self.state.phase) {
                return Ok(exit);
            }
            if self.stop.is_set() {
                self.pause_interrupted()?;
                continue;
            }
            self.step()?;
            if let Some(error) = self.log.error.take() {
                return Err(error.into());
            }
        }
    }

    fn step(&mut self) -> Result<(), RunError> {
        match self.state.phase {
            Phase::Planning => self.plan(),
            Phase::Executing => self.execute(),
            Phase::Reviewing => self.review(),
            Phase::Paused | Phase::Done | Phase::Failed => Ok(()),
        }
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn note(&mut self, entry: LogEntry) {
        let lane = self.state.lane.clone();
        self.log.record(
            self.state.task_index,
            self.state.attempt,
            lane.as_deref(),
            entry,
        );
    }

    fn note_run(&mut self) {
        let state = &self.state;
        let entry = LogEntry::State(StateEvent::Run {
            run_id: state.run_id.clone(),
            goal: state.goal.clone(),
            branch: state.branch.clone(),
        });
        self.note(entry);
    }

    /// Nothing before the plan exists.
    fn note_plan(&mut self) {
        let Some(plan) = &self.plan else {
            return;
        };
        let titles: Vec<String> = plan.tasks.iter().map(|task| task.title.clone()).collect();
        self.note(LogEntry::State(StateEvent::Plan {
            tasks: titles.len(),
            titles,
        }));
    }

    fn set_phase(&mut self, phase: Phase) {
        self.state.phase = phase;
        self.note(LogEntry::State(StateEvent::Phase { phase }));
    }

    fn save(&mut self) -> Result<(), RunError> {
        self.state.updated_at = self.now();
        self.log.dir.write_state(&self.state)?;
        Ok(())
    }

    fn task(&self) -> Option<&Task> {
        let index = self.state.task_index?;
        self.plan.as_ref()?.tasks.get(index.checked_sub(1)?)
    }

    // ---- planning

    fn plan(&mut self) -> Result<(), RunError> {
        let lane = self.config.roles.planner.clone();
        let prompt = prompt::planner_prompt(&self.state.goal);
        match self.ask(&lane, Role::Planner, &prompt, PLAN_SCHEMA, parse_plan)? {
            Asked::Answer(plan) => self.accept_plan(plan),
            Asked::Pause(reason, message, until) => {
                self.pause(reason, Phase::Planning, message, until)
            }
            Asked::Failed(problem) => self.fail(format!("the planner failed twice: {problem}")),
            Asked::Stopped => Ok(()),
        }
    }

    /// A new run plans first, or starts from a plan the owner approved.
    fn begin(&mut self, plan: Option<Plan>) -> Result<(), RunError> {
        let Some(plan) = plan else {
            self.note(LogEntry::State(StateEvent::Phase {
                phase: Phase::Planning,
            }));
            return self.save();
        };
        self.accept_plan(plan)
    }

    fn accept_plan(&mut self, plan: Plan) -> Result<(), RunError> {
        self.log.dir.write_plan(&plan)?;
        self.plan = Some(plan);
        self.note_plan();
        self.start_task(1)
    }

    fn start_task(&mut self, index: usize) -> Result<(), RunError> {
        let state = &mut self.state;
        state.task_index = Some(index);
        state.task_base = None;
        state.attempt = None;
        state.attempts_used = 0;
        // Clearing the lane also restarts the Unknown streak at the next attempt.
        state.lane = None;
        state.skipped.clear();
        state.issues.clear();
        state.last_output_tail.clear();
        state.last_end = None;
        state.approved = false;
        self.set_phase(Phase::Executing);
        self.save()
    }

    /// Runs a planner or reviewer job, retried once, and parses its answer.
    fn ask<T>(
        &mut self,
        lane: &str,
        role: Role,
        prompt: &str,
        schema: &'static str,
        parse: fn(&serde_json::Value) -> Result<T, FormatError>,
    ) -> Result<Asked<T>, RunError> {
        let mut problem = String::new();
        for _ in 0..ASK_TRIES {
            let Some((outcome, _)) = self.job(lane, role, prompt.to_string(), Some(schema))? else {
                return Ok(Asked::Stopped);
            };
            match classify(outcome, parse) {
                Ok(answer) => return Ok(Asked::Answer(answer)),
                Err(Unusable::Pause(reason, message, until)) => {
                    return Ok(Asked::Pause(reason, message, until));
                }
                Err(Unusable::Retry(message)) => problem = message,
            }
        }
        Ok(Asked::Failed(problem))
    }

    /// Runs one job. `None` means the stop flag was set: the outcome is
    /// discarded and the caller returns so the loop can pause.
    fn job(
        &mut self,
        lane: &str,
        role: Role,
        prompt: String,
        schema: Option<&'static str>,
    ) -> Result<Option<(Outcome, String)>, RunError> {
        self.state.job_seq += 1;
        self.save()?;
        let job = Job {
            role,
            prompt,
            cwd: self.state.worktree.clone(),
            run_dir: self.log.dir.path().to_path_buf(),
            raw_log: self.log.dir.raw_log_path(self.state.job_seq),
            schema,
        };
        let (task, attempt) = (self.state.task_index, self.state.attempt);
        let mut texts: Vec<String> = Vec::new();
        let log = &mut self.log;
        let lock = &mut self.lock;
        // A lock that cannot be updated only weakens the orphan check of the
        // next start, so the job goes on.
        let outcome = self.runner.run(
            lane,
            &job,
            &mut |pgid| {
                let _ = lock.set_pgid(Some(pgid));
            },
            &mut |event| {
                if let HarnessEvent::Text { text } = &event {
                    texts.push(text.clone());
                }
                log.record(task, attempt, Some(lane), LogEntry::Harness(event));
            },
        );
        let _ = self.lock.set_pgid(None);
        if self.stop.is_set() {
            return Ok(None);
        }
        self.log.record(
            task,
            attempt,
            Some(lane),
            LogEntry::Outcome(outcome.clone()),
        );
        Ok(Some((outcome, texts.join("\n"))))
    }

    // ---- ending

    fn fail(&mut self, message: String) -> Result<(), RunError> {
        self.set_phase(Phase::Failed);
        self.note(LogEntry::State(StateEvent::Failed { message }));
        self.save()
    }

    fn pause_interrupted(&mut self) -> Result<(), RunError> {
        let phase = self.state.phase;
        if phase == Phase::Executing {
            self.state.last_end = Some(LastEnd::Interrupted);
        }
        let message = "stopped by the user or a signal".to_string();
        self.pause(PauseReason::Interrupted, phase, message, None)
    }

    /// When the next step is a worker attempt, the hand-over it would get is
    /// written first, so a person can continue by hand.
    fn pause(
        &mut self,
        reason: PauseReason,
        resume_phase: Phase,
        mut message: String,
        until: Option<u64>,
    ) -> Result<(), RunError> {
        if resume_phase == Phase::Executing && self.write_pause_handover()? {
            message.push_str(&format!(
                "\nBy hand: cd {}, then tell any agent: Read .onerig/handover.md and follow its \
                 Instruction section.\nAfterwards: onerig resume {}",
                self.state.worktree.display(),
                self.state.run_id
            ));
        }
        self.state.phase = Phase::Paused;
        self.state.paused = Some(Paused {
            reason,
            resume_phase,
            message: message.clone(),
            until,
        });
        self.note(LogEntry::State(StateEvent::Paused { reason, message }));
        self.save()
    }

    /// False when there is no task yet, or git cannot give the diff.
    fn write_pause_handover(&mut self) -> Result<bool, RunError> {
        let next = self.state.attempt.unwrap_or(0) + 1;
        match self.worker_prompt(next, true) {
            Ok(prompt) => self.write_handover(next, &prompt).map(|()| true),
            Err(_) => Ok(false),
        }
    }
}

/// The phases a run stops in; the others are working phases.
fn exit_of(phase: Phase) -> Option<Exit> {
    match phase {
        Phase::Paused => Some(Exit::Paused),
        Phase::Done => Some(Exit::Done),
        Phase::Failed => Some(Exit::Failed),
        Phase::Planning | Phase::Executing | Phase::Reviewing => None,
    }
}

fn handover_note(worktree: &Path) -> PathBuf {
    worktree.join(".onerig").join("handover.md")
}

fn remove_if_present(path: &Path) -> Result<(), StoreError> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(StoreError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        }),
        _ => Ok(()),
    }
}

mod execute;
mod review;
#[cfg(test)]
mod tests;
