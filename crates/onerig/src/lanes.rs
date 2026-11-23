//! Builds the runner for a configuration: scripted lanes go to the fake
//! runner, every other lane to its harness adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, bail};
use onerig_core::adapter::Adapter;
use onerig_core::adapter::agy::Agy;
use onerig_core::adapter::claude::Claude;
use onerig_core::adapter::codex::Codex;
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
    /// `data_home` is where lanes that need a directory of their own keep it.
    pub fn build(
        config: &Config,
        stop: &Arc<AtomicBool>,
        cwd: &Path,
        data_home: &Path,
    ) -> anyhow::Result<Lanes> {
        let Sorted {
            adapters,
            scripted_lanes,
            script,
        } = sort_lanes(config, data_home)?;
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

fn sort_lanes(config: &Config, data_home: &Path) -> anyhow::Result<Sorted> {
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
                .insert(name.clone(), adapter_for(name, lane, data_home)?);
        }
    }
    Ok(sorted)
}

/// Every lane named in a role, once, planner first.
pub fn lanes_in_roles(config: &Config) -> Vec<&String> {
    let roles = &config.roles;
    let mut names: Vec<&String> = Vec::new();
    for name in [&roles.planner, &roles.reviewer]
        .into_iter()
        .chain(&roles.worker)
    {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

fn adapter_for(name: &str, lane: &Lane, data_home: &Path) -> anyhow::Result<Box<dyn Adapter>> {
    match lane.harness {
        HarnessKind::Claude => Ok(Box::new(Claude::new(lane))),
        HarnessKind::Codex => Ok(Box::new(Codex::new(name, lane, data_home))),
        HarnessKind::Agy => Ok(Box::new(Agy::new(lane))),
        HarnessKind::Fake => bail!("lane {name}: a scripted lane has no harness adapter"),
    }
}

/// The warning section 15 of the design asks for whenever onerig itself will
/// drive `agy`.
pub fn vendor_terms_warning(config: &Config) -> Option<String> {
    let first = lanes_in_roles(config).into_iter().find(|name| {
        let lane = &config.lanes[*name];
        lane.harness == HarnessKind::Agy && !lane.is_off() && !lane.is_manual()
    })?;
    Some(format!(
        "onerig: warning: lane {first} drives agy directly. Google's Antigravity terms and FAQ \
         treat driving agy from another agent on a consumer sign-in as a breach that can lead to \
         account suspension; you enabled it at your own risk."
    ))
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

        let mut lanes = Lanes::build(&config, &stop, Path::new("."), Path::new("/data")).unwrap();

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
    fn a_direct_agy_lane_gets_its_adapter_and_a_warning() {
        let script = script();
        let direct = "mode = \"direct\"\nquota_group = \"G\"\naccept_vendor_terms_risk = true\n";
        let config = config_with("\"gemini\"", direct, script.path());
        let manual = config_with("\"gemini\"", "", script.path());
        let stop = Arc::new(AtomicBool::new(false));

        assert!(Lanes::build(&config, &stop, Path::new("."), Path::new("/data")).is_ok());
        assert!(
            vendor_terms_warning(&config)
                .unwrap()
                .contains("lane gemini drives agy directly")
        );
        assert_eq!(vendor_terms_warning(&manual), None);
    }

    #[test]
    fn a_codex_lane_gets_its_adapter() {
        let script = script();
        let codex = "\n[lanes.deepseek]\nharness = \"codex\"\nmodel = \"deepseek-flash\"\n";
        let config = config_with("\"deepseek\"", codex, script.path());
        let stop = Arc::new(AtomicBool::new(false));

        let mut lanes = Lanes::build(&config, &stop, Path::new("."), Path::new("/data")).unwrap();

        assert_eq!(lanes.quota("deepseek"), None);
    }
}
