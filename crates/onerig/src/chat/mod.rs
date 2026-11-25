//! Interactive chat screen loop, session replay, background workers, and
//! `/model` config persistence (design sections 5, 6, 8 and 9).

pub mod format;
pub mod picker;
pub mod render;
pub mod state;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use onerig_core::config::{self, Config};
use onerig_core::halt::Halt;
use onerig_core::plan::owner_plan;
use onerig_core::run::{self, Env, NewRun};
use onerig_core::session::{SessionDir, TranscriptEntry, TranscriptRecord, messages, session_runs};
use onerig_core::store::{LogEntry, LogRecord, RunDir, StateEvent};
use onerig_core::turn::{ActiveRun, TurnPrompt, TurnResult, run_turn};
use onerig_core::worktree::Git;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::chat::format::LogFormatter;
use crate::chat::picker::fetch_agy_models;
use crate::chat::state::{Chat, CommandAction};
use crate::lanes::{Lanes, vendor_terms_warning};

const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub struct ChatContext {
    pub config: Config,
    pub config_path: PathBuf,
    pub main_repo: PathBuf,
    pub data_home: PathBuf,
    pub stop: Halt,
    pub session: SessionDir,
    pub resume: bool,
}

enum BgEvent {
    MainRecord(LogRecord),
    MainTurnDone(Lanes, TurnResult),
    MainRunDone(Lanes, Box<Result<run::Finished, run::RunError>>),
    MainPanicked(String),
    SideTurnDone(TurnResult),
    SidePanicked(String),
    AgyModels(Vec<(String, String)>),
}

pub fn run(ctx: ChatContext) -> anyhow::Result<()> {
    let mut app = App::new(ctx)?;
    let mut terminal = ratatui::init();
    let result = app.event_loop(&mut terminal);
    app.shutdown();
    ratatui::restore();
    result
}

struct App {
    config: Config,
    config_path: PathBuf,
    main_repo: PathBuf,
    data_home: PathBuf,
    main_halt: Halt,
    session: SessionDir,
    transcript: Vec<TranscriptRecord>,
    raw_seq: u64,
    chat: Chat,
    formatter: LogFormatter,
    runner: Option<Lanes>,
    main_thread: Option<JoinHandle<()>>,
    side_thread: Option<JoinHandle<()>>,
    record_next_run: Option<bool>,
    agy_fetched: bool,
    tx: Sender<BgEvent>,
    rx: Receiver<BgEvent>,
}

impl App {
    fn new(ctx: ChatContext) -> anyhow::Result<App> {
        let (tx, rx) = mpsc::channel();
        let runner = Lanes::build(&ctx.config, &ctx.stop, &ctx.main_repo, &ctx.data_home)?;
        let raw_seq = ctx.session.next_raw_log_seq();
        let mut app = App {
            chat: Chat::new(&ctx.config),
            config: ctx.config,
            config_path: ctx.config_path,
            main_repo: ctx.main_repo,
            data_home: ctx.data_home,
            main_halt: ctx.stop,
            session: ctx.session,
            transcript: Vec::new(),
            raw_seq,
            formatter: LogFormatter::default(),
            runner: Some(runner),
            main_thread: None,
            side_thread: None,
            record_next_run: None,
            agy_fetched: false,
            tx,
            rx,
        };
        if ctx.resume {
            app.replay_session()?;
        }
        if let Some(warning) = vendor_terms_warning(&app.config) {
            app.chat.push_system(&warning);
        }
        Ok(app)
    }

    fn replay_session(&mut self) -> anyhow::Result<()> {
        let records = self.session.read_transcript()?;
        for record in &records {
            self.chat.apply_transcript(&record.entry);
            if let TranscriptEntry::Run { run, .. } = &record.entry {
                self.replay_run(run);
            }
        }
        self.transcript = records;
        Ok(())
    }

    fn replay_run(&mut self, run_id: &str) {
        let Ok(dir) = RunDir::open(&self.main_repo, run_id) else {
            return;
        };
        self.formatter = LogFormatter::default();
        if let Ok(plan) = dir.read_plan() {
            self.formatter.load_plan(&plan);
        }
        if let Ok(logs) = dir.read_log() {
            for log in &logs {
                self.chat.apply_log(log, &mut self.formatter);
            }
        }
    }

    fn event_loop<B: ratatui::backend::Backend>(
        &mut self,
        terminal: &mut ratatui::Terminal<B>,
    ) -> anyhow::Result<()> {
        self.run_loop(terminal, read_terminal_event)
    }

    fn run_loop<B: ratatui::backend::Backend>(
        &mut self,
        terminal: &mut ratatui::Terminal<B>,
        mut next_event: impl FnMut() -> anyhow::Result<Option<Event>>,
    ) -> anyhow::Result<()> {
        let _ = terminal.draw(|frame| render::render(&self.chat, frame));
        while !self.quit_requested() {
            let ev = next_event()?;
            if !self.apply_frame(terminal, ev)? {
                break;
            }
        }
        Ok(())
    }

    fn quit_requested(&self) -> bool {
        self.main_halt
            .quit_flag()
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn apply_frame<B: ratatui::backend::Backend>(
        &mut self,
        terminal: &mut ratatui::Terminal<B>,
        ev: Option<Event>,
    ) -> anyhow::Result<bool> {
        let mut dirty = self.drain_bg();
        let quit = match ev.and_then(next_key_press) {
            Some(key) => {
                dirty = true;
                let action = self.chat.handle_key(key, &self.config);
                self.handle_action(action)?
            }
            None => false,
        };
        if dirty {
            let _ = terminal.draw(|frame| render::render(&self.chat, frame));
        }
        Ok(!quit && !self.quit_requested())
    }

    fn drain_bg(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            changed = true;
            self.on_bg(msg);
        }
        changed
    }

    fn on_bg(&mut self, msg: BgEvent) {
        match msg {
            BgEvent::MainRecord(record) => self.on_main_record(&record),
            BgEvent::MainTurnDone(runner, result) => self.on_main_turn_done(runner, result),
            BgEvent::MainRunDone(runner, result) => self.on_main_run_done(runner, *result),
            BgEvent::MainPanicked(err) => self.on_main_panic(&err),
            BgEvent::SideTurnDone(result) => self.on_side_turn_done(result),
            BgEvent::SidePanicked(err) => self.on_side_panic(&err),
            BgEvent::AgyModels(models) => self.chat.set_agy_models(models),
        }
    }

    fn on_main_record(&mut self, record: &LogRecord) {
        if let LogEntry::State(StateEvent::Run { run_id, .. }) = &record.entry
            && let Some(from_owner) = self.record_next_run.take()
        {
            self.append_record(TranscriptEntry::Run {
                run: run_id.clone(),
                from_owner,
            });
        }
        self.chat.apply_log(record, &mut self.formatter);
    }

    fn on_main_turn_done(&mut self, runner: Lanes, result: TurnResult) {
        self.runner = Some(runner);
        join_thread(&mut self.main_thread);
        self.chat.main_busy = None;
        self.finish_turn(result, false);
    }

    fn on_side_turn_done(&mut self, result: TurnResult) {
        join_thread(&mut self.side_thread);
        self.chat.side_busy = false;
        self.finish_turn(result, true);
    }

    fn finish_turn(&mut self, result: TurnResult, btw: bool) {
        match result {
            TurnResult::Answered(answer) => {
                self.append_record(TranscriptEntry::Planner {
                    text: answer.reply,
                    plan: answer.proposal,
                    btw,
                });
            }
            TurnResult::Failed(err) => {
                self.append_record(TranscriptEntry::System {
                    text: format!("planner turn failed: {err}"),
                });
            }
            TurnResult::Stopped => {
                self.append_record(TranscriptEntry::System {
                    text: "planner turn stopped".into(),
                });
            }
        }
    }

    fn on_main_run_done(&mut self, runner: Lanes, result: Result<run::Finished, run::RunError>) {
        self.runner = Some(runner);
        join_thread(&mut self.main_thread);
        self.chat.main_busy = None;
        if let Err(err) = result {
            self.append_record(TranscriptEntry::System {
                text: format!("run error: {err}"),
            });
        }
    }

    fn on_main_panic(&mut self, err: &str) {
        join_thread(&mut self.main_thread);
        self.chat.main_busy = None;
        self.rebuild_runner();
        self.append_record(TranscriptEntry::System {
            text: format!("worker panicked: {err}"),
        });
    }

    fn on_side_panic(&mut self, err: &str) {
        join_thread(&mut self.side_thread);
        self.chat.side_busy = false;
        self.append_record(TranscriptEntry::System {
            text: format!("side turn panicked: {err}"),
        });
    }

    fn handle_action(&mut self, action: CommandAction) -> anyhow::Result<bool> {
        match action {
            CommandAction::None => {}
            CommandAction::Quit => return Ok(true),
            CommandAction::StopMain => self.main_halt.stop(),
            CommandAction::OpenModelPicker => self.open_model_picker(),
            CommandAction::SaveConfig(new_cfg, summary) => {
                self.apply_new_config(new_cfg, &summary);
            }
            other => self.dispatch_work_action(other),
        }
        Ok(false)
    }

    fn dispatch_work_action(&mut self, action: CommandAction) {
        match action {
            CommandAction::SendPlanner(text) => self.start_planner_turn(text, false, None),
            CommandAction::Btw(text) => {
                let snapshot = self.chat.active_run_snapshot();
                self.start_planner_turn(text, true, snapshot);
            }
            CommandAction::Go => self.handle_go(),
            CommandAction::DirectLane { lane, text } => self.handle_direct_lane(&lane, &text),
            CommandAction::None
            | CommandAction::OpenModelPicker
            | CommandAction::SaveConfig(_, _)
            | CommandAction::StopMain
            | CommandAction::Quit => {}
        }
    }

    fn open_model_picker(&mut self) {
        self.chat.open_picker(&self.config);
        if !self.agy_fetched {
            self.agy_fetched = true;
            let bin = agy_bin_for(&self.config);
            let tx = self.tx.clone();
            thread::spawn(move || {
                let models = fetch_agy_models(&bin);
                if !models.is_empty() {
                    let _ = tx.send(BgEvent::AgyModels(models));
                }
            });
        }
    }

    fn apply_new_config(&mut self, new_cfg: Config, summary: &str) {
        if let Err(err) = config::save(&self.config_path, &new_cfg) {
            self.chat.push_system(&format!("cannot save config: {err}"));
            return;
        }
        match Lanes::build(&new_cfg, &self.main_halt, &self.main_repo, &self.data_home) {
            Ok(runner) => {
                self.config = new_cfg;
                self.runner = Some(runner);
                self.chat.refresh_config(&self.config);
                self.append_record(TranscriptEntry::System {
                    text: format!("saved config: {summary}"),
                });
            }
            Err(err) => self
                .chat
                .push_system(&format!("cannot rebuild lanes: {err}")),
        }
    }

    fn start_planner_turn(&mut self, text: String, btw: bool, active: Option<ActiveRun>) {
        if !btw && self.runner.is_none() {
            return;
        }
        self.append_record(TranscriptEntry::Owner { text, btw });
        let runs = session_runs(&self.main_repo, &self.transcript);
        let msgs = messages(&self.transcript);
        let waiting = if btw {
            None
        } else {
            self.chat.waiting_plan.clone()
        };
        let config = self.config.clone();
        let raw_dir = self.session.path().to_path_buf();
        let mut seq = self.raw_seq;
        self.raw_seq += 2;
        let tx = self.tx.clone();
        if btw {
            self.chat.side_busy = true;
            let side_halt = self.main_halt.with_shared_quit();
            let Ok(mut side_runner) =
                Lanes::build(&config, &side_halt, &self.main_repo, &self.data_home)
            else {
                self.chat.side_busy = false;
                self.chat.push_system("cannot build runner for /btw");
                return;
            };
            self.side_thread = Some(thread::spawn(move || {
                let prompt = TurnPrompt {
                    messages: &msgs,
                    waiting: None,
                    active_run: active.as_ref(),
                    retry_error: None,
                };
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_turn(
                        &config,
                        &mut side_runner,
                        &side_halt,
                        &runs.planner_cwd,
                        &raw_dir,
                        &mut seq,
                        &prompt,
                        &mut |_| {},
                    )
                }));
                let msg = match outcome {
                    Ok(res) => BgEvent::SideTurnDone(res),
                    Err(p) => BgEvent::SidePanicked(panic_text(p)),
                };
                let _ = tx.send(msg);
            }));
            return;
        }
        self.main_halt.clear_stop();
        let Some(mut runner) = self.runner.take() else {
            return;
        };
        self.chat.main_busy = Some("planner".into());
        let halt = self.main_halt.clone();
        self.main_thread = Some(thread::spawn(move || {
            let prompt = TurnPrompt {
                messages: &msgs,
                waiting: waiting.as_ref(),
                active_run: None,
                retry_error: None,
            };
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_turn(
                    &config,
                    &mut runner,
                    &halt,
                    &runs.planner_cwd,
                    &raw_dir,
                    &mut seq,
                    &prompt,
                    &mut |_| {},
                )
            }));
            let msg = match outcome {
                Ok(res) => BgEvent::MainTurnDone(runner, res),
                Err(p) => BgEvent::MainPanicked(panic_text(p)),
            };
            let _ = tx.send(msg);
        }));
    }

    fn handle_go(&mut self) {
        if let Some(proposal) = self.chat.waiting_plan.take() {
            self.start_run(proposal.goal, proposal.plan, false);
            return;
        }
        let runs = session_runs(&self.main_repo, &self.transcript);
        if let Some(run_id) = runs.latest_resumable {
            self.resume_run(run_id);
        } else {
            self.chat.push_system("no waiting plan or paused run");
        }
    }

    fn handle_direct_lane(&mut self, lane: &str, text: &str) {
        match owner_plan(lane, text) {
            Ok(plan) => {
                let goal = plan.tasks[0].instructions.clone();
                self.start_run(goal, plan, true);
            }
            Err(err) => self.chat.push_system(&format!("invalid task: {err}")),
        }
    }

    fn start_run(&mut self, goal: String, plan: onerig_core::plan::Plan, from_owner: bool) {
        self.main_halt.clear_stop();
        let Some(mut runner) = self.runner.take() else {
            return;
        };
        let runs = session_runs(&self.main_repo, &self.transcript);
        self.formatter = LogFormatter::default();
        self.formatter.load_plan(&plan);
        self.record_next_run = Some(from_owner);
        self.chat.main_busy = Some("run".into());
        let config = self.config.clone();
        let cwd = self.main_repo.clone();
        let data = self.data_home.clone();
        let halt = self.main_halt.clone();
        let tx = self.tx.clone();
        self.main_thread = Some(thread::spawn(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let git = Git::new();
                let mut sink = |record: &LogRecord| {
                    let _ = tx.send(BgEvent::MainRecord(record.clone()));
                };
                let env = Env {
                    git: &git,
                    runner: &mut runner,
                    clock: &now_ms,
                    pid: std::process::id(),
                    stop: &halt,
                    sink: &mut sink,
                    data_home: &data,
                };
                run::start_with(
                    &config,
                    &cwd,
                    NewRun {
                        goal: &goal,
                        plan: Some(plan),
                        base: runs.latest_done_branch.as_deref(),
                    },
                    env,
                )
            }));
            let msg = match res {
                Ok(out) => BgEvent::MainRunDone(runner, Box::new(out)),
                Err(p) => BgEvent::MainPanicked(panic_text(p)),
            };
            let _ = tx.send(msg);
        }));
    }

    fn resume_run(&mut self, run_id: String) {
        self.main_halt.clear_stop();
        let Some(mut runner) = self.runner.take() else {
            return;
        };
        self.formatter = LogFormatter::default();
        if let Ok(dir) = RunDir::open(&self.main_repo, &run_id)
            && let Ok(plan) = dir.read_plan()
        {
            self.formatter.load_plan(&plan);
        }
        self.record_next_run = None;
        self.chat.main_busy = Some("resuming run".into());
        let config = self.config.clone();
        let cwd = self.main_repo.clone();
        let data = self.data_home.clone();
        let halt = self.main_halt.clone();
        let tx = self.tx.clone();
        self.main_thread = Some(thread::spawn(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let git = Git::new();
                let mut sink = |record: &LogRecord| {
                    let _ = tx.send(BgEvent::MainRecord(record.clone()));
                };
                let env = Env {
                    git: &git,
                    runner: &mut runner,
                    clock: &now_ms,
                    pid: std::process::id(),
                    stop: &halt,
                    sink: &mut sink,
                    data_home: &data,
                };
                run::resume(&config, &cwd, Some(&run_id), env)
            }));
            let msg = match res {
                Ok(out) => BgEvent::MainRunDone(runner, Box::new(out)),
                Err(p) => BgEvent::MainPanicked(panic_text(p)),
            };
            let _ = tx.send(msg);
        }));
    }

    fn append_record(&mut self, entry: TranscriptEntry) {
        let rec = TranscriptRecord {
            ts: now_ms(),
            entry: entry.clone(),
        };
        if let Err(err) = self.session.append(&rec) {
            self.chat
                .push_system(&format!("transcript write error: {err}"));
        }
        self.transcript.push(rec);
        self.chat.apply_transcript(&entry);
    }

    fn rebuild_runner(&mut self) {
        match Lanes::build(
            &self.config,
            &self.main_halt,
            &self.main_repo,
            &self.data_home,
        ) {
            Ok(runner) => self.runner = Some(runner),
            Err(err) => self
                .chat
                .push_system(&format!("cannot rebuild runner: {err}")),
        }
    }

    fn shutdown(&mut self) {
        self.main_halt
            .quit_flag()
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.main_halt.stop();
        join_thread(&mut self.main_thread);
        join_thread(&mut self.side_thread);
    }
}

fn join_thread(slot: &mut Option<JoinHandle<()>>) {
    if let Some(handle) = slot.take() {
        let _ = handle.join();
    }
}

fn read_terminal_event() -> anyhow::Result<Option<Event>> {
    event::poll(POLL_INTERVAL)?
        .then(read_next_event)
        .transpose()
}

fn read_next_event() -> anyhow::Result<Event> {
    Ok(event::read()?)
}

fn next_key_press(ev: Event) -> Option<ratatui::crossterm::event::KeyEvent> {
    match ev {
        Event::Key(key) if key.kind == KeyEventKind::Press => Some(key),
        _ => None,
    }
}

fn agy_bin_for(config: &Config) -> String {
    config
        .lanes
        .values()
        .find(|l| l.harness == onerig_core::config::HarnessKind::Agy)
        .map_or_else(|| "agy".to_string(), |l| l.bin.clone())
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "worker thread panicked".to_string()
    }
}

pub fn new_session_id(pid: u32) -> String {
    onerig_core::store::run_id(now_ms(), pid)
}

pub fn ensure_session_repo(cwd: &Path) -> anyhow::Result<PathBuf> {
    let git = Git::new();
    let main = git.main_checkout(cwd)?;
    git.preflight(&main)?;
    git.ensure_excluded(&main)?;
    Ok(main)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serde_json::json;
    use std::process::Command;

    struct Harness {
        _tmp: tempfile::TempDir,
        app: App,
    }

    fn make_harness(steps: Vec<serde_json::Value>, resume: bool, session_id: &str) -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        let data = root.join("data");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "user.email", "t@example.invalid"]);
        std::fs::write(repo.join("README"), "hi\n").unwrap();
        git(&["add", "README"]);
        git(&["commit", "-q", "-m", "init"]);

        let script = root.join("script.json");
        std::fs::write(&script, json!({ "steps": steps }).to_string()).unwrap();
        let cfg_path = root.join("config.toml");
        let cfg_text = format!(
            "[roles]\nplanner = [\"plan\"]\nreviewer = [\"review\"]\nworker = [\"w1\"]\n\n\
             [lanes.plan]\nharness = \"fake\"\nscript = \"{}\"\n\n\
             [lanes.review]\nharness = \"fake\"\nscript = \"{}\"\n\n\
             [lanes.w1]\nharness = \"fake\"\nscript = \"{}\"\n",
            script.display(),
            script.display(),
            script.display()
        );
        std::fs::write(&cfg_path, cfg_text).unwrap();
        let config = config::load(&cfg_path, true).unwrap();
        let session = if resume {
            SessionDir::open(&repo, session_id).unwrap()
        } else {
            SessionDir::create(&repo, session_id).unwrap()
        };
        let app = App::new(ChatContext {
            config,
            config_path: cfg_path,
            main_repo: repo,
            data_home: data,
            stop: Halt::new(),
            session,
            resume,
        })
        .unwrap();
        Harness { _tmp: tmp, app }
    }

    fn wait_idle(app: &mut App) {
        let start = std::time::Instant::now();
        while (app.chat.main_busy.is_some() || app.chat.side_busy)
            && start.elapsed() < Duration::from_secs(10)
        {
            app.drain_bg();
            thread::sleep(Duration::from_millis(10));
        }
        app.drain_bg();
        assert!(app.chat.main_busy.is_none(), "main action never finished");
        assert!(!app.chat.side_busy, "side action never finished");
    }

    fn wait_for_file(dir: &Path, name: &str) {
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            let found = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .any(|e| e.path().join(name).exists());
            if found {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("{name} never appeared");
    }

    #[test]
    fn scripted_session_runs_proposal_concurrent_btw_and_continues_cleanly() {
        let answer = json!({
            "lane": "plan",
            "outcome": { "type": "done", "text": "", "structured": {
                "reply": "It has README.", "goal": "", "tasks": []
            } }
        });
        let proposal = json!({
            "lane": "plan",
            "outcome": { "type": "done", "text": "", "structured": {
                "reply": "One task.",
                "goal": "Add a file",
                "tasks": [{ "id": "t1", "title": "Add a.txt", "instructions": "write a", "acceptance": ["exists"] }]
            } }
        });
        let hang_work = json!({ "lane": "w1", "hang": true, "writes": [["a.txt", "x\n"]] });
        let btw_answer = json!({
            "lane": "plan",
            "outcome": { "type": "done", "text": "", "structured": {
                "reply": "Still running t1.", "goal": "", "tasks": []
            } }
        });
        let finish_work = json!({
            "lane": "w1",
            "writes": [["a.txt", "done\n"]],
            "outcome": { "type": "done", "text": "ok", "structured": null }
        });
        let approve = json!({
            "lane": "review",
            "outcome": { "type": "done", "text": "", "structured": {
                "verdict": "approve", "summary": "Good."
            } }
        });

        let mut h = make_harness(
            vec![
                answer,
                proposal,
                hang_work,
                btw_answer,
                finish_work,
                approve,
            ],
            false,
            "s1",
        );

        h.app
            .handle_action(CommandAction::SendPlanner("what is here?".into()))
            .unwrap();
        wait_idle(&mut h.app);
        assert_eq!(h.app.chat.waiting_plan, None);

        h.app
            .handle_action(CommandAction::SendPlanner("add a.txt".into()))
            .unwrap();
        wait_idle(&mut h.app);
        assert!(h.app.chat.waiting_plan.is_some());

        // Start /go -> worker hangs after writing a.txt; send /btw mid-run, then /stop + /go resume.
        h.app.handle_action(CommandAction::Go).unwrap();
        let wt_parent = h.app.data_home.join("onerig/worktrees/repo");
        wait_for_file(&wt_parent, "a.txt");
        h.app.drain_bg();
        assert!(h.app.chat.active_run.is_some());

        h.app
            .handle_action(CommandAction::Btw("progress?".into()))
            .unwrap();
        let btw_start = std::time::Instant::now();
        while h.app.chat.side_busy && btw_start.elapsed() < Duration::from_secs(5) {
            h.app.drain_bg();
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!h.app.chat.side_busy);
        assert!(
            h.app
                .chat
                .lines
                .iter()
                .any(|l| l.contains("Still running t1.")),
            "lines: {:?}",
            h.app.chat.lines
        );
        // Main run is still active until stopped!
        assert!(h.app.chat.main_busy.is_some());

        h.app.handle_action(CommandAction::StopMain).unwrap();
        wait_idle(&mut h.app);

        // Resume paused run with /go -> finishes with commit!
        h.app.handle_action(CommandAction::Go).unwrap();
        wait_idle(&mut h.app);
        assert!(h.app.chat.lines.iter().any(|l| l.contains("commit")));
        assert!(
            h.app
                .chat
                .lines
                .iter()
                .any(|l| l.contains("done: branch onerig/"))
        );

        // Render with TestBackend.
        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| render::render(&h.app.chat, f)).unwrap();

        // Reopen with --continue (replay) and confirm transcript + run logs restore.
        let repo = h.app.main_repo.clone();
        let cfg = h.app.config.clone();
        let cfg_path = h.app.config_path.clone();
        let data = h.app.data_home.clone();
        h.app.shutdown();
        let replayed = App::new(ChatContext {
            config: cfg,
            config_path: cfg_path,
            main_repo: repo.clone(),
            data_home: data,
            stop: Halt::new(),
            session: SessionDir::open(&repo, "s1").unwrap(),
            resume: true,
        })
        .unwrap();
        assert!(
            replayed
                .chat
                .lines
                .iter()
                .any(|l| l.contains("Still running t1."))
        );
        assert!(
            replayed
                .chat
                .lines
                .iter()
                .any(|l| l.contains("done: branch onerig/"))
        );
    }

    #[test]
    fn model_picker_saves_config_and_refreshes_runner_and_panel() {
        let mut h = make_harness(vec![], false, "s2");
        let mut updated = h.app.config.clone();
        updated.run.max_attempts = 7;

        h.app
            .handle_action(CommandAction::SaveConfig(updated, "max_attempts=7".into()))
            .unwrap();

        let disk = config::load(&h.app.config_path, true).unwrap();
        assert_eq!(disk.run.max_attempts, 7);
        assert!(
            h.app
                .chat
                .lines
                .last()
                .unwrap()
                .contains("saved config: max_attempts=7")
        );
    }

    #[test]
    fn actions_picker_direct_lane_panic_and_frame_loop_work() {
        let approve = json!({
            "lane": "review",
            "outcome": { "type": "done", "text": "", "structured": {
                "verdict": "approve", "summary": "ok"
            } }
        });
        let work = json!({
            "lane": "w1",
            "writes": [["direct.txt", "ok\n"]],
            "outcome": { "type": "done", "text": "done", "structured": null }
        });
        let mut h = make_harness(vec![work, approve], false, "s3");

        // OpenModelPicker opens picker (skipping live agy spawn when already flagged).
        h.app.agy_fetched = true;
        assert!(!h.app.handle_action(CommandAction::OpenModelPicker).unwrap());
        assert!(h.app.chat.picker.is_some());
        h.app.chat.picker = None;

        // DirectLane runs owner-directed task.
        h.app
            .handle_action(CommandAction::DirectLane {
                lane: "w1".into(),
                text: "   ".into(),
            })
            .unwrap();
        assert!(h.app.chat.lines.last().unwrap().contains("invalid task"));

        h.app
            .handle_action(CommandAction::DirectLane {
                lane: "w1".into(),
                text: "write direct.txt".into(),
            })
            .unwrap();
        wait_idle(&mut h.app);
        assert!(h.app.chat.lines.iter().any(|l| l.contains("done: branch")));

        // Frame loop & key press handling with TestBackend + panic/rebuild helpers.
        let mut term = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let quit_key = ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Char('d'),
            ratatui::crossterm::event::KeyModifiers::CONTROL,
        );
        assert!(
            !h.app
                .apply_frame(&mut term, Some(Event::Key(quit_key)))
                .unwrap()
        );
        assert!(h.app.handle_action(CommandAction::Quit).unwrap());
        assert!(!h.app.handle_action(CommandAction::StopMain).unwrap());

        h.app
            .main_halt
            .quit_flag()
            .store(true, std::sync::atomic::Ordering::Relaxed);
        h.app.event_loop(&mut term).unwrap();
        h.app
            .main_halt
            .quit_flag()
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let mut events = std::collections::VecDeque::from([
            Some(Event::FocusGained),
            Some(Event::Key(quit_key)),
        ]);
        h.app
            .run_loop(&mut term, || Ok(events.pop_front().flatten()))
            .unwrap();
        h.app.on_main_panic("boom");
        h.app.on_side_panic("side boom");
        assert!(panic_text(Box::new("str err")).contains("str err"));
        assert!(panic_text(Box::new(String::from("owned err"))).contains("owned err"));
        assert_eq!(panic_text(Box::new(42usize)), "worker thread panicked");
        assert!(next_key_press(Event::FocusGained).is_none());
    }
}
