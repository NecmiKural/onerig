//! The `onerig` command: `run` starts a run, `resume` continues one.

mod plain;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use onerig_core::config::{self, Config, ConfigError, HarnessKind};
use onerig_core::fake::FakeRunner;
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

fn run_cli(cli: Cli) -> anyhow::Result<ExitCode> {
    let (Command::Run { options, .. } | Command::Resume { options, .. }) = &cli.command;
    let Some(config) = load_config(options.config.as_deref())? else {
        return Ok(ExitCode::from(2));
    };
    let stop = watch_signals()?;
    let mut runner = scripted_runner(&config, &stop)?;
    let data = data_dir()?;
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let git = Git::new();
    let mut sink = |record: &LogRecord| {
        if let Some(line) = plain::line(record) {
            println!("{line}");
        }
    };
    let env = Env {
        git: &git,
        runner: &mut runner,
        clock: &now_ms,
        pid: std::process::id(),
        stop: &stop,
        sink: &mut sink,
        data_home: &data,
    };
    let finished = dispatch(&cli.command, &config, &cwd, env)?;
    Ok(exit_code(&finished))
}

fn dispatch<'a>(
    command: &Command,
    config: &'a Config,
    cwd: &Path,
    env: Env<'a>,
) -> Result<Finished, run::RunError> {
    match command {
        Command::Run { goal, .. } => {
            warn_if_dirty(env.git, cwd);
            run::start(config, cwd, goal, env)
        }
        Command::Resume { run_id, .. } => run::resume(config, cwd, run_id.as_deref(), env),
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

// shortcut: only scripted (fake) lanes can run until the harness adapters
// exist (design build step 2); replace this with the real runner then.
fn scripted_runner(config: &Config, stop: &Arc<AtomicBool>) -> anyhow::Result<FakeRunner> {
    let roles = &config.roles;
    let names = [&roles.planner, &roles.reviewer]
        .into_iter()
        .chain(&roles.worker);
    let mut script = None;
    for name in names {
        let lane = &config.lanes[name];
        match (lane.harness, &lane.script) {
            (HarnessKind::Fake, Some(path)) => script = Some(path.clone()),
            (HarnessKind::Agy, _) if lane.is_manual() || lane.is_off() => {}
            _ => bail!(
                "lane {name} needs a harness adapter, and onerig cannot drive real harnesses yet"
            ),
        }
    }
    let script = script.context("no lane names a fake script")?;
    Ok(FakeRunner::new(&script, Arc::clone(stop))?)
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
