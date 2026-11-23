//! The configuration file: its data model here, loading and validation below.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub run: RunSettings,
    pub policy: PolicySettings,
    pub roles: Roles,
    pub lanes: BTreeMap<String, Lane>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSettings {
    pub max_attempts: u32,
    pub idle_timeout: Duration,
    pub job_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PolicySettings {
    pub min_remaining: f64,
}

/// Lane names per role. Planner and reviewer have exactly one lane each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roles {
    pub planner: String,
    pub reviewer: String,
    pub worker: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessKind {
    Claude,
    Agy,
    Dsh,
    /// Scripted lane for tests; accepted only when `ONERIG_TEST` is set.
    Fake,
}

/// How an `agy` lane is driven. Lanes of every other harness are always `Direct`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    /// A human runs the harness; onerig starts no process for this lane.
    Handover,
    Direct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    pub harness: HarnessKind,
    /// Binary name or path; defaults to the harness's usual name.
    pub bin: String,
    /// Required for every harness except `Fake`.
    pub model: Option<String>,
    pub effort: Effort,
    pub mode: Mode,
    pub quota_group: Option<String>,
    pub accept_vendor_terms_risk: bool,
    /// Script file of a `Fake` lane.
    pub script: Option<PathBuf>,
}

impl Lane {
    /// A manual lane is done by a human: onerig writes a hand-over and pauses.
    pub fn is_manual(&self) -> bool {
        self.mode == Mode::Handover
    }

    pub fn is_off(&self) -> bool {
        self.mode == Mode::Off
    }
}
