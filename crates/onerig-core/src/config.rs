//! The configuration file of design section 11: its data model, loading and
//! validation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::plan::is_valid_id;

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
    Codex,
    /// Scripted lane for tests; accepted only when `ONERIG_TEST` is set.
    Fake,
}

impl HarnessKind {
    /// The name used in the configuration file.
    pub fn name(self) -> &'static str {
        match self {
            HarnessKind::Claude => "claude",
            HarnessKind::Agy => "agy",
            HarnessKind::Codex => "codex",
            HarnessKind::Fake => "fake",
        }
    }
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

/// The example configuration of design section 11; printed when no file exists.
pub const EXAMPLE: &str = r#"[run]
max_attempts = 5
idle_timeout = "5m"
job_timeout = "30m"

[policy]
min_remaining = 0.05

[roles]
planner  = ["plan"]
worker   = ["gemini", "deepseek"]
reviewer = ["review"]

[lanes.plan]
harness = "claude"
model = "sonnet"
effort = "medium"

[lanes.review]
harness = "claude"
model = "sonnet"
effort = "low"

[lanes.deepseek]
harness = "codex"
model = "deepseek-flash"

[lanes.gemini]
harness = "agy"
mode = "handover"
model = "gemini-4-hydrogen-medium"
quota_group = "Gemini Models"
# accept_vendor_terms_risk = true    # required for mode = "direct"
"#;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("no configuration file at {}; create one (an example follows)", .0.display())]
    Missing(PathBuf),
    #[error("cannot read {}: {message}", path.display())]
    Unreadable { path: PathBuf, message: String },
    #[error("the configuration is not valid TOML of the expected shape: {0}")]
    Syntax(String),
    #[error("{key}: {problem}")]
    Invalid { key: String, problem: String },
}

fn invalid(key: impl Into<String>, problem: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key: key.into(),
        problem: problem.into(),
    }
}

/// `$XDG_CONFIG_HOME/onerig/config.toml`, else `~/.config/onerig/config.toml`.
/// The caller reads the environment.
pub fn default_path(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = match (xdg_config_home.filter(|v| !v.is_empty()), home) {
        (Some(xdg), _) => PathBuf::from(xdg),
        (None, Some(home)) if !home.is_empty() => Path::new(home).join(".config"),
        _ => return None,
    };
    Some(base.join("onerig").join("config.toml"))
}

pub fn load(path: &Path, allow_fake: bool) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ConfigError::Missing(path.to_path_buf()),
        _ => ConfigError::Unreadable {
            path: path.to_path_buf(),
            message: e.to_string(),
        },
    })?;
    parse(&text, allow_fake)
}

/// An integer followed by `s`, `m` or `h`.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let seconds_per_unit = match text.chars().last()? {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        _ => return None,
    };
    // The unit is one ASCII byte; `parse` alone would also accept a sign.
    let number = &text[..text.len() - 1];
    if !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: u64 = number.parse().ok()?;
    value.checked_mul(seconds_per_unit).map(Duration::from_secs)
}

pub fn parse(text: &str, allow_fake: bool) -> Result<Config, ConfigError> {
    let raw: RawConfig = toml::from_str(text).map_err(|e| ConfigError::Syntax(e.to_string()))?;
    let run = run_settings(&raw.run)?;
    let policy = policy_settings(&raw.policy)?;
    let lanes = raw
        .lanes
        .iter()
        .map(|(name, lane)| Ok((name.clone(), lane_from(name, lane, allow_fake)?)))
        .collect::<Result<BTreeMap<_, _>, ConfigError>>()?;
    let roles = roles_from(&raw.roles, &lanes)?;
    Ok(Config {
        run,
        policy,
        roles,
        lanes,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    run: RawRun,
    #[serde(default)]
    policy: RawPolicy,
    roles: RawRoles,
    lanes: BTreeMap<String, RawLane>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawRun {
    max_attempts: Option<i64>,
    idle_timeout: Option<String>,
    job_timeout: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    min_remaining: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoles {
    planner: Vec<String>,
    reviewer: Vec<String>,
    worker: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLane {
    harness: String,
    bin: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    mode: Option<String>,
    quota_group: Option<String>,
    accept_vendor_terms_risk: Option<bool>,
    script: Option<PathBuf>,
}

fn run_settings(raw: &RawRun) -> Result<RunSettings, ConfigError> {
    let max_attempts = raw.max_attempts.unwrap_or(5);
    if !(1..=20).contains(&max_attempts) {
        return Err(invalid("run.max_attempts", "use a number from 1 to 20"));
    }
    Ok(RunSettings {
        max_attempts: max_attempts as u32,
        idle_timeout: duration_setting("run.idle_timeout", raw.idle_timeout.as_deref(), 300)?,
        job_timeout: duration_setting("run.job_timeout", raw.job_timeout.as_deref(), 1800)?,
    })
}

fn duration_setting(
    key: &str,
    text: Option<&str>,
    default_secs: u64,
) -> Result<Duration, ConfigError> {
    let Some(text) = text else {
        return Ok(Duration::from_secs(default_secs));
    };
    match parse_duration(text) {
        Some(duration) if duration >= Duration::from_secs(1) => Ok(duration),
        Some(_) => Err(invalid(key, "use at least 1s")),
        None => Err(invalid(
            key,
            "use a whole number followed by s, m or h, for example \"5m\"",
        )),
    }
}

fn policy_settings(raw: &RawPolicy) -> Result<PolicySettings, ConfigError> {
    let min_remaining = raw.min_remaining.unwrap_or(0.05);
    if !(0.0..=1.0).contains(&min_remaining) {
        return Err(invalid("policy.min_remaining", "use a number from 0 to 1"));
    }
    Ok(PolicySettings { min_remaining })
}

fn lane_from(name: &str, raw: &RawLane, allow_fake: bool) -> Result<Lane, ConfigError> {
    if !is_valid_id(name) {
        return Err(invalid(
            format!("lanes.{name}"),
            "a lane name uses lowercase letters, digits, '-' and '_', at most 32 characters",
        ));
    }
    let harness = harness_kind(name, &raw.harness, allow_fake)?;
    check_keys_apply(name, raw, harness)?;
    let mode = mode_of(name, raw, harness)?;
    check_direct_agy(name, raw, harness, mode)?;
    Ok(Lane {
        harness,
        bin: raw
            .bin
            .clone()
            .unwrap_or_else(|| harness.name().to_string()),
        model: required(
            name,
            "model",
            raw.model.clone(),
            harness != HarnessKind::Fake,
        )?,
        effort: effort_of(name, raw)?,
        mode,
        quota_group: raw.quota_group.clone(),
        accept_vendor_terms_risk: raw.accept_vendor_terms_risk.unwrap_or(false),
        script: required(
            name,
            "script",
            raw.script.clone(),
            harness == HarnessKind::Fake,
        )?,
    })
}

fn harness_kind(name: &str, text: &str, allow_fake: bool) -> Result<HarnessKind, ConfigError> {
    let key = format!("lanes.{name}.harness");
    match text {
        "claude" => Ok(HarnessKind::Claude),
        "agy" => Ok(HarnessKind::Agy),
        "codex" => Ok(HarnessKind::Codex),
        "fake" if allow_fake => Ok(HarnessKind::Fake),
        "fake" => Err(invalid(
            key,
            "fake lanes are for tests and need ONERIG_TEST to be set",
        )),
        _ => Err(invalid(key, "use claude, agy or codex")),
    }
}

fn check_keys_apply(name: &str, raw: &RawLane, harness: HarnessKind) -> Result<(), ConfigError> {
    use HarnessKind::{Agy, Claude, Codex, Fake};
    let rules: [(&str, bool, &[HarnessKind]); 6] = [
        ("model", raw.model.is_some(), &[Claude, Agy, Codex]),
        ("effort", raw.effort.is_some(), &[Claude]),
        ("mode", raw.mode.is_some(), &[Agy]),
        ("quota_group", raw.quota_group.is_some(), &[Agy]),
        (
            "accept_vendor_terms_risk",
            raw.accept_vendor_terms_risk.is_some(),
            &[Agy],
        ),
        ("script", raw.script.is_some(), &[Fake]),
    ];
    match rules
        .iter()
        .find(|(_, set, allowed)| *set && !allowed.contains(&harness))
    {
        Some((key, _, _)) => Err(invalid(
            format!("lanes.{name}.{key}"),
            format!("does not apply to a {} lane", harness.name()),
        )),
        None => Ok(()),
    }
}

fn required<T>(
    name: &str,
    key: &str,
    value: Option<T>,
    needed: bool,
) -> Result<Option<T>, ConfigError> {
    match (needed, value) {
        (true, None) => Err(invalid(format!("lanes.{name}.{key}"), "is required")),
        (_, value) => Ok(value),
    }
}

fn mode_of(name: &str, raw: &RawLane, harness: HarnessKind) -> Result<Mode, ConfigError> {
    if harness != HarnessKind::Agy {
        return Ok(Mode::Direct);
    }
    match raw.mode.as_deref().unwrap_or("handover") {
        "off" => Ok(Mode::Off),
        "handover" => Ok(Mode::Handover),
        "direct" => Ok(Mode::Direct),
        _ => Err(invalid(
            format!("lanes.{name}.mode"),
            "use off, handover or direct",
        )),
    }
}

fn check_direct_agy(
    name: &str,
    raw: &RawLane,
    harness: HarnessKind,
    mode: Mode,
) -> Result<(), ConfigError> {
    if harness != HarnessKind::Agy || mode != Mode::Direct {
        return Ok(());
    }
    if raw.accept_vendor_terms_risk != Some(true) {
        return Err(invalid(
            format!("lanes.{name}.accept_vendor_terms_risk"),
            "mode = \"direct\" lets onerig drive agy on your Google sign-in, which Google's \
             Antigravity terms treat as a breach that can suspend the account; set this to true \
             to accept that risk",
        ));
    }
    required(name, "quota_group", raw.quota_group.as_ref(), true).map(|_| ())
}

fn effort_of(name: &str, raw: &RawLane) -> Result<Effort, ConfigError> {
    match raw.effort.as_deref().unwrap_or("medium") {
        "low" => Ok(Effort::Low),
        "medium" => Ok(Effort::Medium),
        "high" => Ok(Effort::High),
        _ => Err(invalid(
            format!("lanes.{name}.effort"),
            "use low, medium or high",
        )),
    }
}

fn roles_from(raw: &RawRoles, lanes: &BTreeMap<String, Lane>) -> Result<Roles, ConfigError> {
    Ok(Roles {
        planner: single_lane("roles.planner", &raw.planner, lanes)?,
        reviewer: single_lane("roles.reviewer", &raw.reviewer, lanes)?,
        worker: worker_lanes(&raw.worker, lanes)?,
    })
}

fn existing<'a>(
    key: &str,
    name: &str,
    lanes: &'a BTreeMap<String, Lane>,
) -> Result<&'a Lane, ConfigError> {
    lanes
        .get(name)
        .ok_or_else(|| invalid(key, format!("there is no lane named {name}")))
}

fn single_lane(
    key: &str,
    names: &[String],
    lanes: &BTreeMap<String, Lane>,
) -> Result<String, ConfigError> {
    let [name] = names else {
        return Err(invalid(key, "name exactly one lane"));
    };
    let lane = existing(key, name, lanes)?;
    if !matches!(lane.harness, HarnessKind::Claude | HarnessKind::Fake) {
        return Err(invalid(key, format!("lane {name} must use harness claude")));
    }
    Ok(name.clone())
}

fn worker_lanes(
    names: &[String],
    lanes: &BTreeMap<String, Lane>,
) -> Result<Vec<String>, ConfigError> {
    const KEY: &str = "roles.worker";
    if names.is_empty() {
        return Err(invalid(KEY, "name at least one lane"));
    }
    for (index, name) in names.iter().enumerate() {
        if names[..index].contains(name) {
            return Err(invalid(KEY, format!("lane {name} is named twice")));
        }
        let lane = existing(KEY, name, lanes)?;
        if lane.harness == HarnessKind::Claude {
            return Err(invalid(
                KEY,
                format!("lane {name} must use harness codex or agy"),
            ));
        }
    }
    if names.iter().all(|name| lanes[name].is_off()) {
        return Err(invalid(KEY, "at least one worker lane must not be off"));
    }
    Ok(names.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
[roles]
planner = ["plan"]
reviewer = ["review"]
worker = ["deepseek"]

[lanes.plan]
harness = "claude"
model = "sonnet"

[lanes.review]
harness = "claude"
model = "sonnet"

[lanes.deepseek]
harness = "codex"
model = "deepseek-flash"
"#;

    const GEMINI: &str = "\n[lanes.gemini]\nharness = \"agy\"\nmodel = \"m\"\n";

    /// BASE with `extra` appended; appended keys land in the last table.
    fn with(extra: &str) -> String {
        format!("{BASE}{extra}\n")
    }

    fn with_gemini(extra: &str) -> String {
        with(&format!("{GEMINI}{extra}")).replace(
            r#"worker = ["deepseek"]"#,
            r#"worker = ["deepseek", "gemini"]"#,
        )
    }

    fn invalid_key(text: &str) -> String {
        match parse(text, false) {
            Err(ConfigError::Invalid { key, .. }) => key,
            other => panic!("expected an invalid key, got {other:?}"),
        }
    }

    #[test]
    fn the_example_is_valid_and_keeps_agy_manual() {
        let config = parse(EXAMPLE, false).unwrap();

        assert_eq!(config.roles.worker, ["gemini", "deepseek"]);
        assert_eq!(config.lanes["gemini"].mode, Mode::Handover);
        assert!(config.lanes["gemini"].is_manual());
        assert_eq!(config.lanes["deepseek"].mode, Mode::Direct);
        assert_eq!(config.lanes["review"].effort, Effort::Low);
    }

    #[test]
    fn missing_settings_take_their_defaults() {
        let config = parse(BASE, false).unwrap();

        assert_eq!(config.run.max_attempts, 5);
        assert_eq!(config.run.idle_timeout, Duration::from_secs(300));
        assert_eq!(config.run.job_timeout, Duration::from_secs(1800));
        assert_eq!(config.policy.min_remaining, 0.05);
        assert_eq!(config.lanes["deepseek"].bin, "codex");
        assert_eq!(config.lanes["plan"].effort, Effort::Medium);
        assert_eq!(config.roles.planner, "plan");
    }

    #[test]
    fn an_unknown_key_or_a_wrong_type_is_a_syntax_error_naming_it() {
        let unknown = parse(&with("\n[run]\nmax_attempt = 3"), false);
        let wrong_type = parse(&with("\n[run]\nmax_attempts = \"3\""), false);

        assert!(matches!(&unknown, Err(ConfigError::Syntax(m)) if m.contains("max_attempt")));
        assert!(matches!(wrong_type, Err(ConfigError::Syntax(_))));
    }

    #[test]
    fn max_attempts_must_be_from_1_to_20() {
        assert_eq!(
            invalid_key(&with("\n[run]\nmax_attempts = 0")),
            "run.max_attempts"
        );
        assert_eq!(
            invalid_key(&with("\n[run]\nmax_attempts = 21")),
            "run.max_attempts"
        );
        assert_eq!(
            parse(&with("\n[run]\nmax_attempts = 20"), false)
                .unwrap()
                .run
                .max_attempts,
            20
        );
    }

    #[test]
    fn durations_need_a_unit_and_at_least_one_second() {
        for bad in ["\"5\"", "\"5d\"", "\"m\"", "\"0s\"", "\"-1s\""] {
            assert_eq!(
                invalid_key(&with(&format!("\n[run]\nidle_timeout = {bad}"))),
                "run.idle_timeout",
                "{bad}"
            );
        }
        let config = parse(
            &with("\n[run]\nidle_timeout = \"1s\"\njob_timeout = \"2h\""),
            false,
        )
        .unwrap();
        assert_eq!(config.run.idle_timeout, Duration::from_secs(1));
        assert_eq!(config.run.job_timeout, Duration::from_secs(7200));
    }

    #[test]
    fn parse_duration_reads_seconds_minutes_and_hours_only() {
        assert_eq!(parse_duration("45s"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        for bad in ["", "s", "5", "5x", " 5m", "5.5m", "+5m", "é"] {
            assert_eq!(parse_duration(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn min_remaining_must_be_from_0_to_1() {
        assert_eq!(
            invalid_key(&with("\n[policy]\nmin_remaining = 1.5")),
            "policy.min_remaining"
        );
        assert_eq!(
            invalid_key(&with("\n[policy]\nmin_remaining = -0.1")),
            "policy.min_remaining"
        );
        assert_eq!(
            parse(&with("\n[policy]\nmin_remaining = 1.0"), false)
                .unwrap()
                .policy
                .min_remaining,
            1.0
        );
    }

    #[test]
    fn a_lane_name_must_be_safe() {
        let text = with("\n[lanes.Deep]\nharness = \"codex\"\nmodel = \"m\"");

        assert_eq!(invalid_key(&text), "lanes.Deep");
    }

    #[test]
    fn an_unknown_harness_is_rejected() {
        let text = BASE.replace(r#"harness = "codex""#, r#"harness = "aider""#);

        assert_eq!(invalid_key(&text), "lanes.deepseek.harness");
    }

    #[test]
    fn a_key_on_a_lane_whose_harness_it_does_not_apply_to_is_rejected() {
        assert_eq!(
            invalid_key(&with("effort = \"low\"")),
            "lanes.deepseek.effort"
        );
        assert_eq!(invalid_key(&with("mode = \"off\"")), "lanes.deepseek.mode");
        assert_eq!(
            invalid_key(&with("quota_group = \"g\"")),
            "lanes.deepseek.quota_group"
        );
        assert_eq!(
            invalid_key(&with("accept_vendor_terms_risk = true")),
            "lanes.deepseek.accept_vendor_terms_risk"
        );
        assert_eq!(
            invalid_key(&with("script = \"s.json\"")),
            "lanes.deepseek.script"
        );
    }

    #[test]
    fn a_lane_needs_a_model() {
        let text = BASE.replace("model = \"deepseek-flash\"\n", "");

        assert_eq!(invalid_key(&text), "lanes.deepseek.model");
    }

    #[test]
    fn effort_must_be_low_medium_or_high() {
        let text = BASE.replacen(
            "model = \"sonnet\"",
            "model = \"sonnet\"\neffort = \"max\"",
            1,
        );

        assert_eq!(invalid_key(&text), "lanes.plan.effort");
    }

    #[test]
    fn an_agy_lane_is_manual_unless_told_otherwise() {
        let config = parse(&with_gemini(""), false).unwrap();

        assert_eq!(config.lanes["gemini"].mode, Mode::Handover);
        assert_eq!(
            invalid_key(&with_gemini("mode = \"auto\"")),
            "lanes.gemini.mode"
        );
    }

    #[test]
    fn direct_agy_needs_the_risk_acknowledgement_and_a_quota_group() {
        let no_ack = with_gemini("mode = \"direct\"\nquota_group = \"Gemini Models\"");
        let no_group = with_gemini("mode = \"direct\"\naccept_vendor_terms_risk = true");
        let both =
            with_gemini("mode = \"direct\"\nquota_group = \"G\"\naccept_vendor_terms_risk = true");

        assert_eq!(
            invalid_key(&no_ack),
            "lanes.gemini.accept_vendor_terms_risk"
        );
        assert_eq!(invalid_key(&no_group), "lanes.gemini.quota_group");
        assert_eq!(
            parse(&both, false).unwrap().lanes["gemini"].mode,
            Mode::Direct
        );
    }

    #[test]
    fn fake_lanes_need_the_test_switch_and_a_script() {
        let fake = BASE.replace(
            "harness = \"codex\"\nmodel = \"deepseek-flash\"",
            "harness = \"fake\"\nscript = \"s.json\"",
        );
        let no_script = BASE.replace(
            "harness = \"codex\"\nmodel = \"deepseek-flash\"",
            "harness = \"fake\"",
        );

        assert_eq!(invalid_key(&fake), "lanes.deepseek.harness");
        assert_eq!(
            parse(&fake, true).unwrap().lanes["deepseek"].script,
            Some(PathBuf::from("s.json"))
        );
        assert!(
            matches!(parse(&no_script, true), Err(ConfigError::Invalid { key, .. }) if key == "lanes.deepseek.script")
        );
    }

    #[test]
    fn a_fake_lane_may_serve_every_role() {
        let text = r#"
[roles]
planner = ["f"]
reviewer = ["f"]
worker = ["f"]

[lanes.f]
harness = "fake"
script = "s.json"
"#;

        assert!(parse(text, true).is_ok());
    }

    #[test]
    fn planner_and_reviewer_name_exactly_one_existing_claude_lane() {
        let two = BASE.replace(r#"planner = ["plan"]"#, r#"planner = ["plan", "review"]"#);
        let unknown = BASE.replace(r#"reviewer = ["review"]"#, r#"reviewer = ["nobody"]"#);
        let worker_as_reviewer =
            BASE.replace(r#"reviewer = ["review"]"#, r#"reviewer = ["deepseek"]"#);

        assert_eq!(invalid_key(&two), "roles.planner");
        assert_eq!(invalid_key(&unknown), "roles.reviewer");
        assert_eq!(invalid_key(&worker_as_reviewer), "roles.reviewer");
    }

    #[test]
    fn the_worker_chain_has_distinct_worker_lanes_and_one_not_off() {
        let empty = BASE.replace(r#"worker = ["deepseek"]"#, "worker = []");
        let twice = BASE.replace(
            r#"worker = ["deepseek"]"#,
            r#"worker = ["deepseek", "deepseek"]"#,
        );
        let claude = BASE.replace(r#"worker = ["deepseek"]"#, r#"worker = ["plan"]"#);
        let all_off = with_gemini("mode = \"off\"").replace(
            r#"worker = ["deepseek", "gemini"]"#,
            r#"worker = ["gemini"]"#,
        );

        for text in [empty, twice, claude, all_off] {
            assert_eq!(invalid_key(&text), "roles.worker");
        }
    }

    #[test]
    fn the_error_message_names_the_key_and_the_fix() {
        let error = parse(&with("\n[run]\nmax_attempts = 0"), false).unwrap_err();

        assert_eq!(
            error.to_string(),
            "run.max_attempts: use a number from 1 to 20"
        );
    }

    #[test]
    fn load_reports_a_missing_file_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        assert_eq!(load(&path, false), Err(ConfigError::Missing(path.clone())));
        std::fs::write(&path, BASE).unwrap();
        assert!(load(&path, false).is_ok());
    }

    #[test]
    fn the_default_path_prefers_xdg_and_falls_back_to_home() {
        assert_eq!(
            default_path(Some("/x"), Some("/h")),
            Some(PathBuf::from("/x/onerig/config.toml"))
        );
        assert_eq!(
            default_path(Some(""), Some("/h")),
            Some(PathBuf::from("/h/.config/onerig/config.toml"))
        );
        assert_eq!(
            default_path(None, Some("/h")),
            Some(PathBuf::from("/h/.config/onerig/config.toml"))
        );
        assert_eq!(default_path(None, None), None);
    }
}
