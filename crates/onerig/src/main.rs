//! The `onerig` command: `run` starts a run, `resume` continues one, `doctor`
//! checks the lanes.

mod doctor;
mod lanes;
mod plain;
mod tui;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use clap::{Parser, Subcommand};
use onerig_core::config::{self, Config, ConfigError};
use onerig_core::run::{self, Env, Exit, Finished};
use onerig_core::store::LogRecord;
use onerig_core::worktree::{Git, data_home};

/// One rig for every agent harness: Claude Code plans and reviews, cheaper
/// harnesses do the work.
#[derive(Parser)]
#[command(name = "onerig", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
    Doctor {
        /// Configuration file; default $XDG_CONFIG_HOME/onerig/config.toml.
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[derive(clap::Args)]
struct Options {
    /// Print one line per event instead of the terminal UI.
    #[arg(long)]
    plain: bool,
    /// Configuration file; default $XDG_CONFIG_HOME/onerig/config.toml.
    #[arg(long)]
    config: Option<PathBuf>,
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
    stop: &'s Arc<AtomicBool>,
}

fn run_cli(cli: Cli) -> anyhow::Result<ExitCode> {
    let (config_arg, plain_wanted) = config_and_mode(&cli.command);
    let Some(config) = load_config(config_arg)? else {
        return Ok(ExitCode::from(2));
    };
    let stop = watch_signals()?;
    let data = data_dir()?;
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let mut runner = lanes::Lanes::build(&config, &stop, &cwd, &data)?;
    if let Command::Doctor { .. } = cli.command {
        return Ok(run_doctor(&config, &mut runner));
    }
    let session = Session {
        command: &cli.command,
        config: &config,
        cwd: &cwd,
        data: &data,
        stop: &stop,
    };
    run_session(&session, &mut runner, plain_wanted)
}

/// The configuration file to use, and whether plain output was asked for.
fn config_and_mode(command: &Command) -> (Option<&Path>, bool) {
    match command {
        Command::Run { options, .. } | Command::Resume { options, .. } => {
            (options.config.as_deref(), options.plain)
        }
        Command::Doctor { config } => (config.as_deref(), true),
    }
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
    let env = Env {
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
    let keys = tui::watch_keys(Arc::clone(session.stop), Arc::clone(&finished));
    // A frame that cannot be drawn is skipped; the next record draws again.
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
    env: Env<'a>,
) -> Result<Finished, run::RunError> {
    match command {
        Command::Resume { run_id, .. } => run::resume(config, cwd, run_id.as_deref(), env),
        Command::Run { goal, .. } => run::start(config, cwd, goal, env),
        Command::Doctor { .. } => unreachable!("doctor is answered before a run starts"),
    }
}

/// SIGINT, SIGTERM and SIGHUP set the stop flag; the run then pauses.
fn watch_signals() -> anyhow::Result<Arc<AtomicBool>> {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, Arc::clone(&stop)).context("cannot watch signals")?;
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
fn load_config(given: Option<&Path>) -> anyhow::Result<Option<Config>> {
    let path = config_path(given)?;
    let allow_fake = std::env::var_os("ONERIG_TEST").is_some();
    match config::load(&path, allow_fake) {
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
