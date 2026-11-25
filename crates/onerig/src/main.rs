//! The `onerig` command: no subcommand opens the interactive chat screen,
//! `run` starts a one-shot run, `resume` continues one, `doctor` checks lanes.

mod chat;
mod doctor;
mod lanes;
mod plain;
mod tui;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use onerig_core::config::{self, Config, ConfigError};
use onerig_core::halt::Halt;
use onerig_core::run::{self, Exit, Finished};
use onerig_core::session::SessionDir;
use onerig_core::store::LogRecord;
use onerig_core::worktree::{Git, data_home};

/// One rig for every agent harness: Claude Code plans and reviews, cheaper
/// harnesses do the work.
#[derive(Parser)]
#[command(name = "onerig", version)]
struct Cli {
    /// Reopen the latest chat session in the current repository.
    #[arg(long = "continue")]
    resume_session: bool,
    /// Configuration file; default $XDG_CONFIG_HOME/onerig/config.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Plan, execute and review a goal in the current repository.
    Run {
        goal: String,
        #[command(flatten)]
        options: Options,
    },
    /// Continue a paused or interrupted run (the latest one by default).
    Resume {
        run_id: Option<String>,
        #[command(flatten)]
        options: Options,
    },
    /// Check that every lane can run. Makes no model call.
    Doctor,
}

#[derive(clap::Args)]
struct Options {
    /// Print one line per event instead of the terminal UI.
    #[arg(long)]
    plain: bool,
}

fn main() -> ExitCode {
    match run_cli(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("onerig: {error:#}");
            ExitCode::from(2)
        }
    }
}

/// What a `run` or `resume` needs besides the runner.
struct Session<'s> {
    command: &'s Command,
    config: &'s Config,
    cwd: &'s Path,
    data: &'s Path,
    stop: &'s Halt,
}

fn run_cli(cli: Cli) -> anyhow::Result<ExitCode> {
    if cli.resume_session && cli.command.is_some() {
        bail!("--continue can only be used without a subcommand");
    }
    let cfg_path = config_path(cli.config.as_deref())?;
    let Some(config) = load_config(&cfg_path)? else {
        return Ok(ExitCode::from(2));
    };
    let stop = watch_signals()?;
    let data = data_dir()?;
    let cwd = std::env::current_dir().context("cannot read the current directory")?;

    let Some(command) = cli.command else {
        return run_chat(config, cfg_path, &cwd, data, stop, cli.resume_session);
    };
    dispatch_command(&command, &config, &cwd, &data, &stop)
}

fn dispatch_command(
    command: &Command,
    config: &Config,
    cwd: &Path,
    data: &Path,
    stop: &Halt,
) -> anyhow::Result<ExitCode> {
    let mut runner = lanes::Lanes::build(config, stop, cwd, data)?;
    if matches!(command, Command::Doctor) {
        return Ok(run_doctor(config, &mut runner));
    }
    let plain_wanted = match command {
        Command::Run { options, .. } | Command::Resume { options, .. } => options.plain,
        Command::Doctor => true,
    };
    let session = Session {
        command,
        config,
        cwd,
        data,
        stop,
    };
    run_session(&session, &mut runner, plain_wanted)
}

fn run_chat(
    config: Config,
    config_path: PathBuf,
    cwd: &Path,
    data_home: PathBuf,
    stop: Halt,
    resume: bool,
) -> anyhow::Result<ExitCode> {
    let main_repo = chat::ensure_session_repo(cwd)?;
    let session = open_chat_session(&main_repo, resume)?;
    ensure_interactive_terminal()?;
    let _lock = session.lock()?;
    chat::run(chat::ChatContext {
        config,
        config_path,
        main_repo,
        data_home,
        stop,
        session,
        resume,
    })?;
    Ok(ExitCode::SUCCESS)
}

fn ensure_interactive_terminal() -> anyhow::Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("interactive chat requires a terminal; use `onerig run \"<goal>\" --plain` instead");
    }
    Ok(())
}

fn open_chat_session(main_repo: &Path, resume: bool) -> anyhow::Result<SessionDir> {
    if !resume {
        let id = chat::new_session_id(std::process::id());
        return Ok(SessionDir::create(main_repo, &id)?);
    }
    let id = SessionDir::latest(main_repo)?
        .context("there is no session to continue in this repository")?;
    Ok(SessionDir::open(main_repo, &id)?)
}

fn run_doctor(config: &Config, runner: &mut lanes::Lanes) -> ExitCode {
    let (lines, ready) = doctor::report(config, runner);
    lines.iter().for_each(|line| println!("{line}"));
    if ready {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn run_session(
    session: &Session<'_>,
    runner: &mut lanes::Lanes,
    plain_wanted: bool,
) -> anyhow::Result<ExitCode> {
    if let Command::Run { .. } = session.command {
        warn_if_dirty(&Git::new(), session.cwd);
    }
    if let Some(warning) = lanes::vendor_terms_warning(session.config) {
        eprintln!("{warning}");
    }
    if plain_wanted || !std::io::stdout().is_terminal() {
        execute(session, runner, &mut |record| {
            if let Some(line) = plain::line(record) {
                println!("{line}");
            }
        })
    } else {
        execute_on_screen(session, runner)
    }
}

fn execute(
    session: &Session<'_>,
    runner: &mut lanes::Lanes,
    sink: &mut dyn FnMut(&LogRecord),
) -> anyhow::Result<ExitCode> {
    let git = Git::new();
    let env = onerig_core::run::Env {
        git: &git,
        runner,
        clock: &now_ms,
        pid: std::process::id(),
        stop: session.stop,
        sink,
        data_home: session.data,
    };
    let finished = dispatch(session.command, session.config, session.cwd, env)?;
    Ok(exit_code(&finished))
}

/// The terminal UI owns the screen until the run stops; what it ended with
/// stays on the terminal afterwards.
fn execute_on_screen(session: &Session<'_>, runner: &mut lanes::Lanes) -> anyhow::Result<ExitCode> {
    let mut screen = tui::Screen::new(session.config);
    let finished = Arc::new(AtomicBool::new(false));
    let mut terminal = ratatui::init();
    let keys = tui::watch_keys(session.stop.clone(), Arc::clone(&finished));
    let _ = terminal.draw(|frame| screen.render(frame));
    let result = execute(session, runner, &mut |record| {
        screen.apply(record);
        let _ = terminal.draw(|frame| screen.render(frame));
    });
    finished.store(true, Ordering::Relaxed);
    let _ = keys.join();
    ratatui::restore();
    screen
        .closing_lines()
        .iter()
        .for_each(|line| println!("{line}"));
    result
}

fn dispatch<'a>(
    command: &Command,
    config: &'a Config,
    cwd: &Path,
    env: onerig_core::run::Env<'a>,
) -> Result<Finished, run::RunError> {
    match command {
        Command::Resume { run_id, .. } => run::resume(config, cwd, run_id.as_deref(), env),
        Command::Run { goal, .. } => run::start(config, cwd, goal, env),
        Command::Doctor => unreachable!("doctor is answered before a run starts"),
    }
}

/// SIGINT, SIGTERM and SIGHUP set the quit flag; the run then pauses.
fn watch_signals() -> anyhow::Result<Halt> {
    let stop = Halt::new();
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, stop.quit_flag()).context("cannot watch signals")?;
    }
    Ok(stop)
}

fn data_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").ok();
    data_home(
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
        home.as_deref(),
    )
    .context("set HOME or XDG_DATA_HOME so onerig knows where to put worktrees")
}

fn config_path(given: Option<&Path>) -> anyhow::Result<PathBuf> {
    match given {
        Some(path) => Ok(path.to_path_buf()),
        None => config::default_path(
            std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        )
        .context("set HOME or pass --config"),
    }
}

/// `None` when there is no configuration file yet; an example is printed.
fn load_config(path: &Path) -> anyhow::Result<Option<Config>> {
    let allow_fake = std::env::var_os("ONERIG_TEST").is_some();
    match config::load(path, allow_fake) {
        Ok(config) => Ok(Some(config)),
        Err(error @ ConfigError::Missing(_)) => {
            eprintln!("onerig: {error}\n\n{}", config::EXAMPLE);
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

fn warn_if_dirty(git: &Git, cwd: &Path) {
    let dirty = git
        .main_checkout(cwd)
        .and_then(|main| git.is_dirty(&main))
        .unwrap_or(false);
    if dirty {
        eprintln!("onerig: note: uncommitted changes in your checkout are not part of the run");
    }
}

fn exit_code(finished: &Finished) -> ExitCode {
    match finished.exit {
        Exit::Done => ExitCode::SUCCESS,
        Exit::Failed => ExitCode::from(1),
        Exit::Paused => ExitCode::from(3),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}
