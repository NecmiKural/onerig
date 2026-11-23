//! Harness adapters. An adapter knows one harness's command line and output
//! format. Every method is pure: it starts nothing and reads nothing, so an
//! adapter is tested with captured output alone.

use std::path::PathBuf;

use crate::runner::{HarnessEvent, Job, Outcome, Probe, ProbeError, QuotaReport};

/// A subprocess call, described but not started.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Spawn {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env_set: Vec<(String, String)>,
    /// Variable names to remove; a trailing `*` removes every variable with that prefix.
    pub env_remove: Vec<String>,
    pub stdin: Option<String>,
    /// Files the driver writes before it starts the process.
    pub files: Vec<(PathBuf, String)>,
}

/// Everything a finished process produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Output {
    pub exit_code: Option<i32>,
    pub stdout: Vec<String>,
    pub stderr_tail: String,
}

/// One value per lane, built from that lane's configuration.
pub trait Adapter {
    fn probe_spawns(&self) -> Vec<Spawn>;
    fn probe_result(&self, outputs: &[Output]) -> Result<Probe, ProbeError>;
    fn quota_spawn(&self) -> Option<Spawn>;
    fn quota_result(&self, output: &Output) -> Option<QuotaReport>;
    fn spawn(&self, job: &Job) -> Spawn;
    /// One stdout line to zero or more events. Unknown lines yield nothing.
    fn parse(&self, line: &str) -> Vec<HarnessEvent>;
    /// The outcome of a job that ended by itself.
    fn outcome(&self, job: &Job, output: &Output) -> Outcome;
}

pub mod claude;
pub mod codex;
