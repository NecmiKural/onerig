//! Pure provider/model catalog and role-lane editing helpers for `Config`.

use std::collections::BTreeSet;

use crate::config::{CodexProvider, Config, Effort, HarnessKind, Lane, Mode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleSlot {
    Planner,
    Reviewer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderId {
    Claude,
    CodexOpenai,
    CodexDeepseek,
    AgyDirect,
    AgyHandover,
}

impl ProviderId {
    pub fn label(self) -> &'static str {
        match self {
            ProviderId::Claude => "Anthropic (Claude Code)",
            ProviderId::CodexOpenai => "OpenAI (Codex)",
            ProviderId::CodexDeepseek => "DeepSeek (Codex)",
            ProviderId::AgyDirect => "Antigravity (direct)",
            ProviderId::AgyHandover => "Antigravity (manual handover)",
        }
    }

    pub fn base_lane_name(self) -> &'static str {
        match self {
            ProviderId::Claude => "claude",
            ProviderId::CodexOpenai => "openai",
            ProviderId::CodexDeepseek => "deepseek",
            ProviderId::AgyDirect | ProviderId::AgyHandover => "gemini",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneSpec {
    pub provider: ProviderId,
    pub model: String,
    pub effort: Effort,
    pub accept_vendor_terms_risk: bool,
}

const SINGLE_ROLE_PROVIDERS: [ProviderId; 4] = [
    ProviderId::Claude,
    ProviderId::CodexOpenai,
    ProviderId::CodexDeepseek,
    ProviderId::AgyDirect,
];

const WORKER_PROVIDERS: [ProviderId; 5] = [
    ProviderId::Claude,
    ProviderId::CodexOpenai,
    ProviderId::CodexDeepseek,
    ProviderId::AgyDirect,
    ProviderId::AgyHandover,
];

const CLAUDE_MODELS: [&str; 5] = [
    "opus",
    "sonnet",
    "haiku",
    "claude-opus-5-5",
    "claude-sonnet-5-5",
];
const OPENAI_MODELS: [&str; 4] = ["gpt-5.4", "gpt-5.3-codex", "o3", "o4-mini"];
const DEEPSEEK_MODELS: [&str; 3] = ["deepseek-flash", "deepseek-chat", "deepseek-reasoner"];
const AGY_MODELS: [&str; 6] = [
    "gemini-4-carbon-high",
    "gemini-4-hydrogen-medium",
    "gemini-4-neon-high",
    "gemini-3.8-flash-high",
    "claude-sonnet-5-5-medium",
    "gpt-oss-120b-medium",
];

pub fn providers_for_single_role() -> &'static [ProviderId] {
    &SINGLE_ROLE_PROVIDERS
}

pub fn providers_for_worker() -> &'static [ProviderId] {
    &WORKER_PROVIDERS
}

pub fn default_models(provider: ProviderId) -> &'static [&'static str] {
    match provider {
        ProviderId::Claude => &CLAUDE_MODELS,
        ProviderId::CodexOpenai => &OPENAI_MODELS,
        ProviderId::CodexDeepseek => &DEEPSEEK_MODELS,
        ProviderId::AgyDirect | ProviderId::AgyHandover => &AGY_MODELS,
    }
}

pub fn agy_quota_group(model: &str) -> &'static str {
    if model.starts_with("claude-") || model.starts_with("gpt-") {
        "Claude and GPT models"
    } else {
        "Gemini Models"
    }
}

pub fn provider_of(lane: &Lane) -> Option<ProviderId> {
    match lane.harness {
        HarnessKind::Claude => Some(ProviderId::Claude),
        HarnessKind::Codex => Some(match lane.provider.unwrap_or(CodexProvider::Deepseek) {
            CodexProvider::Deepseek => ProviderId::CodexDeepseek,
            CodexProvider::Openai => ProviderId::CodexOpenai,
        }),
        HarnessKind::Agy => Some(match lane.mode {
            Mode::Direct => ProviderId::AgyDirect,
            Mode::Handover | Mode::Off => ProviderId::AgyHandover,
        }),
        HarnessKind::Fake => None,
    }
}

/// Builds a valid `Lane` from `spec`, optionally preserving `bin` on same-harness edits.
pub fn lane_from_spec(spec: &LaneSpec, existing_bin: Option<&str>) -> Lane {
    let (harness, provider, mode) = match spec.provider {
        ProviderId::Claude => (HarnessKind::Claude, None, Mode::Direct),
        ProviderId::CodexOpenai => (
            HarnessKind::Codex,
            Some(CodexProvider::Openai),
            Mode::Direct,
        ),
        ProviderId::CodexDeepseek => (
            HarnessKind::Codex,
            Some(CodexProvider::Deepseek),
            Mode::Direct,
        ),
        ProviderId::AgyDirect => (HarnessKind::Agy, None, Mode::Direct),
        ProviderId::AgyHandover => (HarnessKind::Agy, None, Mode::Handover),
    };
    let quota_group =
        (harness == HarnessKind::Agy).then(|| agy_quota_group(&spec.model).to_string());
    Lane {
        harness,
        bin: existing_bin
            .filter(|b| !b.is_empty())
            .unwrap_or(harness.name())
            .to_string(),
        provider,
        model: Some(spec.model.clone()),
        effort: spec.effort,
        mode,
        quota_group,
        accept_vendor_terms_risk: harness == HarnessKind::Agy
            && mode == Mode::Direct
            && spec.accept_vendor_terms_risk,
        script: None,
    }
}

/// Updates `planner` or `reviewer` to `spec`, forking a new lane name if shared.
pub fn set_role_lane(config: &mut Config, slot: RoleSlot, spec: LaneSpec) {
    let current = role_name(config, slot).to_string();
    let default_name = match slot {
        RoleSlot::Planner => "plan",
        RoleSlot::Reviewer => "review",
    };
    let target = if !used_outside_slot(config, slot, &current) {
        current
    } else {
        allocate_lane_name(config, default_name)
    };
    let bin = preserved_bin(config.lanes.get(&target), spec.provider);
    config
        .lanes
        .insert(target.clone(), lane_from_spec(&spec, bin));
    match slot {
        RoleSlot::Planner => config.roles.planner = target,
        RoleSlot::Reviewer => config.roles.reviewer = target,
    }
}

/// Updates `roles.worker[index]` to `spec`, forking if shared with planner/reviewer.
pub fn set_worker_lane(config: &mut Config, index: usize, spec: LaneSpec) {
    let Some(current) = config.roles.worker.get(index).cloned() else {
        return;
    };
    let shared_with_single = current == config.roles.planner || current == config.roles.reviewer;
    let target = if shared_with_single {
        allocate_lane_name(config, spec.provider.base_lane_name())
    } else {
        current
    };
    let bin = preserved_bin(config.lanes.get(&target), spec.provider);
    config
        .lanes
        .insert(target.clone(), lane_from_spec(&spec, bin));
    config.roles.worker[index] = target;
}

/// Appends a new lane built from `spec` to `roles.worker` and returns its name.
pub fn add_worker_lane(config: &mut Config, spec: LaneSpec) -> String {
    let name = allocate_lane_name(config, spec.provider.base_lane_name());
    config
        .lanes
        .insert(name.clone(), lane_from_spec(&spec, None));
    config.roles.worker.push(name.clone());
    name
}

/// Removes `roles.worker[index]` unless doing so leaves zero active worker lanes.
pub fn remove_worker_lane(config: &mut Config, index: usize) -> Result<(), &'static str> {
    if index >= config.roles.worker.len() {
        return Err("worker lane index out of range");
    }
    let has_other_active = config
        .roles
        .worker
        .iter()
        .enumerate()
        .any(|(i, name)| i != index && !config.lanes[name].is_off());
    if !has_other_active {
        return Err("at least one active worker lane must remain");
    }
    config.roles.worker.remove(index);
    Ok(())
}

/// Moves `roles.worker[index]` by `delta` (-1 or +1) and returns the new index.
pub fn move_worker_lane(config: &mut Config, index: usize, delta: isize) -> Option<usize> {
    let target = index.checked_add_signed(delta)?;
    if target >= config.roles.worker.len() || target == index {
        return None;
    }
    config.roles.worker.swap(index, target);
    Some(target)
}

/// Returns `true` if any lane already carries `accept_vendor_terms_risk = true`.
pub fn any_agy_risk_accepted(config: &Config) -> bool {
    config
        .lanes
        .values()
        .any(|lane| lane.harness == HarnessKind::Agy && lane.accept_vendor_terms_risk)
}

fn role_name(config: &Config, slot: RoleSlot) -> &str {
    match slot {
        RoleSlot::Planner => &config.roles.planner,
        RoleSlot::Reviewer => &config.roles.reviewer,
    }
}

fn used_outside_slot(config: &Config, slot: RoleSlot, name: &str) -> bool {
    let in_other_single = match slot {
        RoleSlot::Planner => config.roles.reviewer == name,
        RoleSlot::Reviewer => config.roles.planner == name,
    };
    in_other_single || config.roles.worker.iter().any(|w| w == name)
}

fn preserved_bin(existing: Option<&Lane>, provider: ProviderId) -> Option<&str> {
    let lane = existing?;
    (provider_of(lane) == Some(provider)).then_some(lane.bin.as_str())
}

fn allocate_lane_name(config: &Config, base: &str) -> String {
    let used: BTreeSet<&str> = config.lanes.keys().map(String::as_str).collect();
    let trimmed: String = base.chars().take(28).collect();
    if !used.contains(trimmed.as_str()) {
        return trimmed;
    }
    (2..)
        .map(|n| format!("{trimmed}-{n}"))
        .find(|candidate| !used.contains(candidate.as_str()))
        .expect("infinite range yields an unused lane name")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EXAMPLE, parse, to_toml};

    fn base_config() -> Config {
        parse(EXAMPLE, false).unwrap()
    }

    #[test]
    fn quota_group_follows_model_family() {
        assert_eq!(agy_quota_group("gemini-4-carbon-high"), "Gemini Models");
        assert_eq!(
            agy_quota_group("claude-sonnet-5-5-medium"),
            "Claude and GPT models"
        );
        assert_eq!(
            agy_quota_group("gpt-oss-120b-medium"),
            "Claude and GPT models"
        );
    }

    #[test]
    fn role_editors_keep_roles_isolated_and_round_trip_through_toml() {
        let mut config = base_config();
        config.roles.reviewer = "plan".into();

        set_role_lane(
            &mut config,
            RoleSlot::Reviewer,
            LaneSpec {
                provider: ProviderId::CodexOpenai,
                model: "gpt-5.4".into(),
                effort: Effort::Medium,
                accept_vendor_terms_risk: false,
            },
        );

        assert_eq!(config.roles.planner, "plan");
        assert_ne!(config.roles.reviewer, "plan");
        assert_eq!(
            config.lanes[&config.roles.reviewer].provider,
            Some(CodexProvider::Openai)
        );
        let reparsed = parse(&to_toml(&config), false).unwrap();
        assert_eq!(reparsed, config);
    }

    #[test]
    fn worker_chain_supports_add_edit_move_and_guards_last_active_remove() {
        let mut config = base_config();

        let added = add_worker_lane(
            &mut config,
            LaneSpec {
                provider: ProviderId::Claude,
                model: "sonnet".into(),
                effort: Effort::High,
                accept_vendor_terms_risk: false,
            },
        );
        assert_eq!(config.roles.worker, ["gemini", "deepseek", added.as_str()]);

        assert_eq!(move_worker_lane(&mut config, 2, -1), Some(1));
        assert_eq!(config.roles.worker, ["gemini", added.as_str(), "deepseek"]);
        assert_eq!(move_worker_lane(&mut config, 0, -1), None);

        set_worker_lane(
            &mut config,
            0,
            LaneSpec {
                provider: ProviderId::AgyDirect,
                model: "claude-sonnet-5-5-medium".into(),
                effort: Effort::Medium,
                accept_vendor_terms_risk: true,
            },
        );
        assert_eq!(
            config.lanes[&config.roles.worker[0]].quota_group.as_deref(),
            Some("Claude and GPT models")
        );

        assert!(remove_worker_lane(&mut config, 99).is_err());
        assert!(remove_worker_lane(&mut config, 2).is_ok());
        assert!(remove_worker_lane(&mut config, 1).is_ok());
        assert!(remove_worker_lane(&mut config, 0).is_err());
        assert!(parse(&to_toml(&config), false).is_ok());
    }

    #[test]
    fn shared_worker_forks_and_bin_is_preserved_only_when_provider_matches() {
        let mut config = base_config();
        config.lanes.get_mut("plan").unwrap().bin = "/opt/claude".into();
        config.roles.worker.push("plan".into());
        config.roles.worker.push("review".into());

        // Shared with planner -> forks instead of overwriting `plan`.
        set_worker_lane(
            &mut config,
            2,
            LaneSpec {
                provider: ProviderId::CodexDeepseek,
                model: "deepseek-chat".into(),
                effort: Effort::Medium,
                accept_vendor_terms_risk: false,
            },
        );
        assert_ne!(config.roles.worker[2], "plan");
        assert_eq!(config.lanes["plan"].bin, "/opt/claude");
        // Collides with existing `deepseek` -> allocates `deepseek-2`.
        assert_eq!(config.roles.worker[2], "deepseek-2");

        // Shared with reviewer -> forks too.
        set_worker_lane(
            &mut config,
            3,
            LaneSpec {
                provider: ProviderId::Claude,
                model: "haiku".into(),
                effort: Effort::Low,
                accept_vendor_terms_risk: false,
            },
        );
        assert_ne!(config.roles.worker[3], "review");

        // Same provider on non-shared planner preserves custom bin; switching resets it.
        config.roles.worker.retain(|w| w != "plan");
        set_role_lane(
            &mut config,
            RoleSlot::Planner,
            LaneSpec {
                provider: ProviderId::Claude,
                model: "sonnet".into(),
                effort: Effort::High,
                accept_vendor_terms_risk: false,
            },
        );
        assert_eq!(config.lanes[&config.roles.planner].bin, "/opt/claude");
        set_role_lane(
            &mut config,
            RoleSlot::Planner,
            LaneSpec {
                provider: ProviderId::CodexOpenai,
                model: "gpt-5.4".into(),
                effort: Effort::Medium,
                accept_vendor_terms_risk: false,
            },
        );
        assert_eq!(config.lanes[&config.roles.planner].bin, "codex");

        let last = config.roles.worker.len() - 1;
        assert_eq!(move_worker_lane(&mut config, last, 1), None);
        assert_eq!(move_worker_lane(&mut config, 0, 0), None);
    }

    #[test]
    fn catalog_lists_labels_models_and_detects_provider_and_risk() {
        assert!(!providers_for_single_role().contains(&ProviderId::AgyHandover));
        assert!(providers_for_worker().contains(&ProviderId::AgyHandover));
        for &provider in providers_for_worker() {
            assert!(!provider.label().is_empty());
            assert!(!default_models(provider).is_empty());
            let lane = lane_from_spec(
                &LaneSpec {
                    provider,
                    model: default_models(provider)[0].into(),
                    effort: Effort::Medium,
                    accept_vendor_terms_risk: provider == ProviderId::AgyDirect,
                },
                None,
            );
            assert_eq!(provider_of(&lane), Some(provider));
        }

        let mut config = base_config();
        assert!(!any_agy_risk_accepted(&config));
        set_role_lane(
            &mut config,
            RoleSlot::Planner,
            LaneSpec {
                provider: ProviderId::AgyDirect,
                model: "gemini-4-carbon-high".into(),
                effort: Effort::Medium,
                accept_vendor_terms_risk: true,
            },
        );
        assert!(any_agy_risk_accepted(&config));
        assert_eq!(config.roles.planner, "plan");
    }
}
