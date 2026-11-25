//! Core library of onerig: the run state machine, lane policy and harness
//! adapters. It has no terminal or argument-parsing dependencies, so a
//! command-line tool, a terminal UI or a GUI can all be built on it.

pub mod adapter;
pub mod catalog;
pub mod config;
pub mod fake;
pub mod halt;
pub mod harness;
pub mod plan;
pub mod policy;
pub mod process;
pub mod prompt;
pub mod run;
pub mod runner;
pub mod store;
pub mod worktree;
