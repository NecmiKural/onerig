//! `onerig doctor`: one line per lane on whether it can run. It makes no
//! model call; manual and off lanes are not probed.

use onerig_core::config::Config;
use onerig_core::runner::Runner;

use crate::lanes::lanes_in_roles;

/// The report lines, and whether every probed lane is ready.
pub fn report(config: &Config, runner: &mut dyn Runner) -> (Vec<String>, bool) {
    let mut ready = true;
    let lines = lanes_in_roles(config)
        .into_iter()
        .map(|name| {
            let lane = &config.lanes[name];
            let status = if lane.is_off() {
                "off".to_string()
            } else if lane.is_manual() {
                "manual, not probed".to_string()
            } else {
                match runner.preflight(name) {
                    Ok(probe) => with_quota(format!("ok {}", probe.version), runner, name),
                    Err(error) => {
                        ready = false;
                        format!("failed: {error}")
                    }
                }
            };
            format!("{name:<10} {:<6} {status}", lane.harness.name())
        })
        .collect();
    (lines, ready)
}

fn with_quota(status: String, runner: &mut dyn Runner, lane: &str) -> String {
    match runner.quota(lane) {
        Some(report) => format!(
            "{status}, {:.0}% quota left",
            report.remaining_fraction * 100.0
        ),
        None => status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use onerig_core::config;
    use onerig_core::runner::{HarnessEvent, Job, Outcome, Probe, ProbeError, QuotaReport};

    struct Answers;

    impl Runner for Answers {
        fn preflight(&mut self, lane: &str) -> Result<Probe, ProbeError> {
            match lane {
                "plan" => Ok(Probe {
                    version: "2.1.330".into(),
                }),
                _ => Err(ProbeError::NotLoggedIn(
                    "DEEPSEEK_API_KEY is not set".into(),
                )),
            }
        }

        fn quota(&mut self, lane: &str) -> Option<QuotaReport> {
            (lane == "plan").then_some(QuotaReport {
                remaining_fraction: 0.42,
                reset_at: None,
            })
        }

        fn run(
            &mut self,
            _: &str,
            _: &Job,
            _: &mut dyn FnMut(i32),
            _: &mut dyn FnMut(HarnessEvent),
        ) -> Outcome {
            unreachable!("doctor never runs a job")
        }
    }

    #[test]
    fn every_lane_gets_one_line_and_a_failed_probe_makes_the_report_unready() {
        let config = config::parse(
            r#"
[roles]
planner = ["plan"]
reviewer = ["plan"]
worker = ["deepseek", "gemini", "spare"]

[lanes.plan]
harness = "claude"
model = "sonnet"

[lanes.deepseek]
harness = "codex"
model = "deepseek-flash"

[lanes.gemini]
harness = "agy"
model = "m"

[lanes.spare]
harness = "agy"
model = "m"
mode = "off"
"#,
            false,
        )
        .unwrap();

        let (lines, ready) = report(&config, &mut Answers);

        assert_eq!(
            lines,
            [
                "plan       claude ok 2.1.330, 42% quota left",
                "deepseek   codex  failed: DEEPSEEK_API_KEY is not set",
                "gemini     agy    manual, not probed",
                "spare      agy    off",
            ]
        );
        assert!(!ready);
    }
}
