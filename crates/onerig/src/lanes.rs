//! Builds the runner for a configuration: scripted lanes go to the fake
//! runner, every other lane to its harness adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, bail};
use onerig_core::adapter::Adapter;
use onerig_core::adapter::claude::Claude;
use onerig_core::config::{Config, HarnessKind, Lane};
use onerig_core::fake::FakeRunner;
use onerig_core::harness::HarnessRunner;
use onerig_core::process::Limits;
use onerig_core::runner::{HarnessEvent, Job, Outcome, Probe, ProbeError, QuotaReport, Runner};

pub struct Lanes {
    scripted: Option<FakeRunner>,
    scripted_lanes: BTreeSet<String>,
    harness: HarnessRunner,
}

impl Lanes {
    pub fn build(config: &Config, stop: &Arc<AtomicBool>, cwd: &Path) -> anyhow::Result<Lanes> {
        let Sorted {
            adapters,
            scripted_lanes,
            script,
        } = sort_lanes(config)?;
        let scripted = match script {
            Some(path) => Some(
                FakeRunner::new(&path, Arc::clone(stop)).context("cannot load the fake script")?,
            ),
            None => None,
        };
        let limits = Limits {
            idle: config.run.idle_timeout,
            total: config.run.job_timeout,
        };
        Ok(Lanes {
            scripted,
            scripted_lanes,
            harness: HarnessRunner::new(adapters, limits, Arc::clone(stop), cwd.to_path_buf()),
        })
    }

    fn pick(&mut self, lane: &str) -> &mut dyn Runner {
        match (&mut self.scripted, self.scripted_lanes.contains(lane)) {
            (Some(scripted), true) => scripted,
            _ => &mut self.harness,
        }
    }
}

/// The lanes a run can start, sorted by who runs them.
struct Sorted {
    adapters: BTreeMap<String, Box<dyn Adapter>>,
    scripted_lanes: BTreeSet<String>,
    script: Option<PathBuf>,
}

fn sort_lanes(config: &Config) -> anyhow::Result<Sorted> {
    let mut sorted = Sorted {
        adapters: BTreeMap::new(),
        scripted_lanes: BTreeSet::new(),
        script: None,
    };
    for name in lanes_in_roles(config) {
        let lane = &config.lanes[name];
        if lane.is_off() || lane.is_manual() {
            continue;
        }
        if lane.harness == HarnessKind::Fake {
            sorted.scripted_lanes.insert(name.clone());
            sorted.script = lane.script.clone().or(sorted.script);
        } else {
            sorted
                .adapters
                .insert(name.clone(), adapter_for(name, lane)?);
        }
    }
    Ok(sorted)
}

fn lanes_in_roles(config: &Config) -> impl Iterator<Item = &String> {
    let roles = &config.roles;
    [&roles.planner, &roles.reviewer]
        .into_iter()
        .chain(&roles.worker)
}

fn adapter_for(name: &str, lane: &Lane) -> anyhow::Result<Box<dyn Adapter>> {
    match lane.harness {
        HarnessKind::Claude => Ok(Box::new(Claude::new(lane))),
        HarnessKind::Codex | HarnessKind::Agy | HarnessKind::Fake => bail!(
            "lane {name}: onerig cannot drive this harness yet; only claude, scripted lanes and a \
             manual agy lane work in this build"
        ),
    }
}

impl Runner for Lanes {
    fn preflight(&mut self, lane: &str) -> Result<Probe, ProbeError> {
        self.pick(lane).preflight(lane)
    }

    fn quota(&mut self, lane: &str) -> Option<QuotaReport> {
        self.pick(lane).quota(lane)
    }

    fn run(
        &mut self,
        lane: &str,
        job: &Job,
        on_spawn: &mut dyn FnMut(i32),
        on_event: &mut dyn FnMut(HarnessEvent),
    ) -> Outcome {
        self.pick(lane).run(lane, job, on_spawn, on_event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onerig_core::config;

    fn config_with(worker: &str, extra: &str, script: &Path) -> Config {
        let text = format!(
            "[roles]\nplanner = [\"plan\"]\nreviewer = [\"plan\"]\nworker = [{worker}]\n\n\
             [lanes.plan]\nharness = \"claude\"\nmodel = \"sonnet\"\n\n\
             [lanes.w]\nharness = \"fake\"\nscript = \"{}\"\n\n\
             [lanes.gemini]\nharness = \"agy\"\nmodel = \"m\"\n{extra}",
            script.display()
        );
        config::parse(&text, true).unwrap()
    }

    fn script() -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r#"{ "quota": { "w": { "remaining_fraction": 0.5, "reset_at": null } }, "steps": [] }"#,
        )
        .unwrap();
        file
    }

    #[test]
    fn scripted_lanes_go_to_the_fake_runner_and_manual_lanes_need_nothing() {
        let script = script();
        let config = config_with("\"w\", \"gemini\"", "", script.path());
        let stop = Arc::new(AtomicBool::new(false));

        let mut lanes = Lanes::build(&config, &stop, Path::new(".")).unwrap();

        assert_eq!(
            lanes.quota("w"),
            Some(QuotaReport {
                remaining_fraction: 0.5,
                reset_at: None
            })
        );
        assert_eq!(lanes.quota("plan"), None);
    }

    #[test]
    fn a_lane_whose_adapter_is_not_built_yet_stops_the_command() {
        let script = script();
        let codex = "\n[lanes.deepseek]\nharness = \"codex\"\nmodel = \"deepseek-flash\"\n";
        let config = config_with("\"deepseek\"", codex, script.path());
        let stop = Arc::new(AtomicBool::new(false));

        let error = Lanes::build(&config, &stop, Path::new(".")).err().unwrap();

        assert!(
            error
                .to_string()
                .starts_with("lane deepseek: onerig cannot drive")
        );
    }
}
