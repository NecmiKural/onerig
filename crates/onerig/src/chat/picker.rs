//! Pure `/model` modal picker state machine for selecting roles, providers,
//! models, effort, and the ordered worker fail-over chain.

use onerig_core::adapter::agy;
use onerig_core::catalog::{
    LaneSpec, ProviderId, RoleSlot, add_worker_lane, any_agy_risk_accepted, default_models,
    move_worker_lane, provider_of, providers_for_single_role, providers_for_worker,
    remove_worker_lane, set_role_lane, set_worker_lane,
};
use onerig_core::config::{Config, Effort, HarnessKind, Lane};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerLine {
    pub text: String,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerView {
    pub title: String,
    pub lines: Vec<PickerLine>,
    pub footer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditTarget {
    Planner,
    Reviewer,
    WorkerEdit(usize),
    WorkerAdd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Roles {
        selected: usize,
    },
    WorkerChain {
        selected: usize,
        notice: Option<String>,
    },
    ChooseProvider {
        target: EditTarget,
        selected: usize,
    },
    ChooseModel {
        target: EditTarget,
        provider: ProviderId,
        effort: Effort,
        selected: usize,
        custom: Option<String>,
    },
    ConfirmRisk {
        target: EditTarget,
        model: String,
        effort: Effort,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PickerAction {
    None,
    Close,
    Apply(Config, String),
}

#[derive(Debug, Clone)]
pub struct ModelPicker {
    config: Config,
    agy_models: Vec<(String, String)>,
    step: Step,
}

impl ModelPicker {
    pub fn new(config: Config, agy_models: Vec<(String, String)>) -> ModelPicker {
        ModelPicker {
            config,
            agy_models,
            step: Step::Roles { selected: 0 },
        }
    }

    pub fn set_agy_models(&mut self, models: Vec<(String, String)>) {
        self.agy_models = models;
    }

    pub fn sync_config(&mut self, config: Config) {
        self.config = config;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PickerAction {
        match self.step.clone() {
            Step::Roles { selected } => self.on_roles(key, selected),
            Step::WorkerChain { selected, .. } => self.on_worker_chain(key, selected),
            Step::ChooseProvider { target, selected } => {
                self.on_choose_provider(key, target, selected)
            }
            Step::ChooseModel {
                target,
                provider,
                effort,
                selected,
                custom,
            } => self.on_choose_model(key, target, provider, effort, selected, custom),
            Step::ConfirmRisk {
                target,
                model,
                effort,
            } => self.on_confirm_risk(key, target, &model, effort),
        }
    }

    fn on_roles(&mut self, key: KeyEvent, selected: usize) -> PickerAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => PickerAction::Close,
            KeyCode::Up => {
                self.step = Step::Roles {
                    selected: selected.saturating_sub(1),
                };
                PickerAction::None
            }
            KeyCode::Down => {
                self.step = Step::Roles {
                    selected: (selected + 1).min(2),
                };
                PickerAction::None
            }
            KeyCode::Enter => {
                match selected {
                    0 => self.open_provider(EditTarget::Planner),
                    1 => self.open_provider(EditTarget::Reviewer),
                    _ => {
                        self.step = Step::WorkerChain {
                            selected: 0,
                            notice: None,
                        };
                    }
                }
                PickerAction::None
            }
            _ => PickerAction::None,
        }
    }

    fn on_worker_chain(&mut self, key: KeyEvent, selected: usize) -> PickerAction {
        if let Some(action) = self.on_worker_nav_or_reorder(key, selected) {
            return action;
        }
        let len = self.config.roles.worker.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.step = Step::Roles { selected: 2 };
                PickerAction::None
            }
            KeyCode::Enter if len > 0 => {
                self.open_provider(EditTarget::WorkerEdit(selected));
                PickerAction::None
            }
            KeyCode::Char('a') => {
                self.open_provider(EditTarget::WorkerAdd);
                PickerAction::None
            }
            KeyCode::Char('x') | KeyCode::Delete => self.delete_worker(selected),
            _ => PickerAction::None,
        }
    }

    fn on_worker_nav_or_reorder(&mut self, key: KeyEvent, selected: usize) -> Option<PickerAction> {
        let len = self.config.roles.worker.len();
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Up if shift => Some(self.move_worker(selected, -1)),
            KeyCode::Char('[') => Some(self.move_worker(selected, -1)),
            KeyCode::Down if shift => Some(self.move_worker(selected, 1)),
            KeyCode::Char(']') => Some(self.move_worker(selected, 1)),
            KeyCode::Up => {
                self.step = Step::WorkerChain {
                    selected: selected.saturating_sub(1),
                    notice: None,
                };
                Some(PickerAction::None)
            }
            KeyCode::Down => {
                self.step = Step::WorkerChain {
                    selected: (selected + 1).min(len.saturating_sub(1)),
                    notice: None,
                };
                Some(PickerAction::None)
            }
            _ => None,
        }
    }

    fn move_worker(&mut self, selected: usize, delta: isize) -> PickerAction {
        let mut next = self.config.clone();
        if let Some(new_idx) = move_worker_lane(&mut next, selected, delta) {
            self.config = next.clone();
            self.step = Step::WorkerChain {
                selected: new_idx,
                notice: None,
            };
            return PickerAction::Apply(
                next,
                format!("worker order: {}", worker_summary(&self.config)),
            );
        }
        PickerAction::None
    }

    fn delete_worker(&mut self, selected: usize) -> PickerAction {
        let mut next = self.config.clone();
        match remove_worker_lane(&mut next, selected) {
            Ok(()) => {
                let new_sel = selected.min(next.roles.worker.len().saturating_sub(1));
                self.config = next.clone();
                self.step = Step::WorkerChain {
                    selected: new_sel,
                    notice: None,
                };
                PickerAction::Apply(
                    next,
                    format!("worker order: {}", worker_summary(&self.config)),
                )
            }
            Err(msg) => {
                self.step = Step::WorkerChain {
                    selected,
                    notice: Some(msg.to_string()),
                };
                PickerAction::None
            }
        }
    }

    fn open_provider(&mut self, target: EditTarget) {
        let list = providers_for(target);
        let current = self.current_lane(target).and_then(provider_of);
        let selected = current
            .and_then(|p| list.iter().position(|&item| item == p))
            .unwrap_or(0);
        self.step = Step::ChooseProvider { target, selected };
    }

    fn on_choose_provider(
        &mut self,
        key: KeyEvent,
        target: EditTarget,
        selected: usize,
    ) -> PickerAction {
        let list = providers_for(target);
        match key.code {
            KeyCode::Esc => {
                self.back_from_provider(target);
                PickerAction::None
            }
            KeyCode::Up => {
                self.step = Step::ChooseProvider {
                    target,
                    selected: selected.saturating_sub(1),
                };
                PickerAction::None
            }
            KeyCode::Down => {
                self.step = Step::ChooseProvider {
                    target,
                    selected: (selected + 1).min(list.len().saturating_sub(1)),
                };
                PickerAction::None
            }
            KeyCode::Enter => {
                let provider = list[selected];
                let current_lane = self.current_lane(target);
                let effort = current_lane
                    .filter(|l| l.harness == HarnessKind::Claude)
                    .map_or(Effort::Medium, |l| l.effort);
                let models = self.models_for(provider);
                let model_sel = current_lane
                    .and_then(|l| l.model.as_deref())
                    .and_then(|m| models.iter().position(|(id, _)| id == m))
                    .unwrap_or(0);
                self.step = Step::ChooseModel {
                    target,
                    provider,
                    effort,
                    selected: model_sel,
                    custom: None,
                };
                PickerAction::None
            }
            _ => PickerAction::None,
        }
    }

    fn on_choose_model(
        &mut self,
        key: KeyEvent,
        target: EditTarget,
        provider: ProviderId,
        effort: Effort,
        selected: usize,
        custom: Option<String>,
    ) -> PickerAction {
        if let Some(buf) = custom {
            return self.on_custom_model(key, target, provider, effort, selected, buf);
        }
        if key.code == KeyCode::Enter {
            return self.confirm_model_row(target, provider, effort, selected);
        }
        if key.code == KeyCode::Esc {
            self.open_provider(target);
            return PickerAction::None;
        }
        let max_idx = self.models_for(provider).len();
        let (next_effort_val, next_sel) =
            step_model_cursor(key.code, provider, effort, selected, max_idx);
        self.step = Step::ChooseModel {
            target,
            provider,
            effort: next_effort_val,
            selected: next_sel,
            custom: None,
        };
        PickerAction::None
    }

    fn confirm_model_row(
        &mut self,
        target: EditTarget,
        provider: ProviderId,
        effort: Effort,
        selected: usize,
    ) -> PickerAction {
        let models = self.models_for(provider);
        if selected >= models.len() {
            self.step = Step::ChooseModel {
                target,
                provider,
                effort,
                selected,
                custom: Some(String::new()),
            };
            return PickerAction::None;
        }
        self.commit_selection(target, provider, models[selected].0.clone(), effort)
    }

    fn on_custom_model(
        &mut self,
        key: KeyEvent,
        target: EditTarget,
        provider: ProviderId,
        effort: Effort,
        selected: usize,
        mut buf: String,
    ) -> PickerAction {
        match key.code {
            KeyCode::Esc => {
                self.step = Step::ChooseModel {
                    target,
                    provider,
                    effort,
                    selected,
                    custom: None,
                };
                PickerAction::None
            }
            KeyCode::Enter => {
                let model = buf.trim().to_string();
                if model.is_empty() {
                    return PickerAction::None;
                }
                self.commit_selection(target, provider, model, effort)
            }
            KeyCode::Backspace => {
                buf.pop();
                self.step = Step::ChooseModel {
                    target,
                    provider,
                    effort,
                    selected,
                    custom: Some(buf),
                };
                PickerAction::None
            }
            KeyCode::Char(c) if !c.is_control() => {
                buf.push(c);
                self.step = Step::ChooseModel {
                    target,
                    provider,
                    effort,
                    selected,
                    custom: Some(buf),
                };
                PickerAction::None
            }
            _ => PickerAction::None,
        }
    }

    fn on_confirm_risk(
        &mut self,
        key: KeyEvent,
        target: EditTarget,
        model: &str,
        effort: Effort,
    ) -> PickerAction {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => self.apply_spec(
                target,
                ProviderId::AgyDirect,
                model.to_string(),
                effort,
                true,
            ),
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.step = Step::ChooseModel {
                    target,
                    provider: ProviderId::AgyDirect,
                    effort,
                    selected: 0,
                    custom: None,
                };
                PickerAction::None
            }
            _ => PickerAction::None,
        }
    }

    fn commit_selection(
        &mut self,
        target: EditTarget,
        provider: ProviderId,
        model: String,
        effort: Effort,
    ) -> PickerAction {
        if provider == ProviderId::AgyDirect && !any_agy_risk_accepted(&self.config) {
            self.step = Step::ConfirmRisk {
                target,
                model,
                effort,
            };
            return PickerAction::None;
        }
        self.apply_spec(
            target,
            provider,
            model,
            effort,
            provider == ProviderId::AgyDirect,
        )
    }

    fn apply_spec(
        &mut self,
        target: EditTarget,
        provider: ProviderId,
        model: String,
        effort: Effort,
        accept_risk: bool,
    ) -> PickerAction {
        let mut next = self.config.clone();
        let spec = LaneSpec {
            provider,
            model: model.clone(),
            effort,
            accept_vendor_terms_risk: accept_risk,
        };
        let summary = match target {
            EditTarget::Planner => {
                set_role_lane(&mut next, RoleSlot::Planner, spec);
                self.step = Step::Roles { selected: 0 };
                format!("planner → {} ({model})", provider.label())
            }
            EditTarget::Reviewer => {
                set_role_lane(&mut next, RoleSlot::Reviewer, spec);
                self.step = Step::Roles { selected: 1 };
                format!("reviewer → {} ({model})", provider.label())
            }
            EditTarget::WorkerEdit(idx) => {
                set_worker_lane(&mut next, idx, spec);
                self.step = Step::WorkerChain {
                    selected: idx,
                    notice: None,
                };
                format!("worker[{}] → {} ({model})", idx + 1, provider.label())
            }
            EditTarget::WorkerAdd => {
                let name = add_worker_lane(&mut next, spec);
                let idx = next.roles.worker.len().saturating_sub(1);
                self.step = Step::WorkerChain {
                    selected: idx,
                    notice: None,
                };
                format!("worker added: {name} ({model})")
            }
        };
        self.config = next.clone();
        PickerAction::Apply(next, summary)
    }

    fn back_from_provider(&mut self, target: EditTarget) {
        self.step = match target {
            EditTarget::Planner => Step::Roles { selected: 0 },
            EditTarget::Reviewer => Step::Roles { selected: 1 },
            EditTarget::WorkerEdit(idx) => Step::WorkerChain {
                selected: idx,
                notice: None,
            },
            EditTarget::WorkerAdd => Step::WorkerChain {
                selected: self.config.roles.worker.len().saturating_sub(1),
                notice: None,
            },
        };
    }

    fn current_lane(&self, target: EditTarget) -> Option<&Lane> {
        let name = match target {
            EditTarget::Planner => &self.config.roles.planner,
            EditTarget::Reviewer => &self.config.roles.reviewer,
            EditTarget::WorkerEdit(idx) => self.config.roles.worker.get(idx)?,
            EditTarget::WorkerAdd => return None,
        };
        self.config.lanes.get(name)
    }

    fn models_for(&self, provider: ProviderId) -> Vec<(String, String)> {
        if matches!(provider, ProviderId::AgyDirect | ProviderId::AgyHandover)
            && !self.agy_models.is_empty()
        {
            return self.agy_models.clone();
        }
        default_models(provider)
            .iter()
            .map(|m| ((*m).to_string(), (*m).to_string()))
            .collect()
    }

    pub fn view(&self) -> PickerView {
        match &self.step {
            Step::Roles { selected } => self.view_roles(*selected),
            Step::WorkerChain { selected, notice } => {
                self.view_workers(*selected, notice.as_deref())
            }
            Step::ChooseProvider { target, selected } => self.view_providers(*target, *selected),
            Step::ChooseModel {
                provider,
                effort,
                selected,
                custom,
                ..
            } => self.view_models(*provider, *effort, *selected, custom.as_deref()),
            Step::ConfirmRisk { model, .. } => PickerView {
                title: " Accept Antigravity Terms Risk? ".into(),
                lines: vec![
                    PickerLine {
                        text: format!("Model: {model}"),
                        selected: false,
                    },
                    PickerLine {
                        text: "Google Antigravity terms treat driving agy from another agent on a"
                            .into(),
                        selected: false,
                    },
                    PickerLine {
                        text: "consumer sign-in as a breach that can suspend the account.".into(),
                        selected: false,
                    },
                ],
                footer: "y accept & save  n/Esc cancel".into(),
            },
        }
    }

    fn view_roles(&self, selected: usize) -> PickerView {
        let rows = [
            format!(
                "planner : {}",
                lane_summary(&self.config, &self.config.roles.planner)
            ),
            format!(
                "reviewer: {}",
                lane_summary(&self.config, &self.config.roles.reviewer)
            ),
            format!("worker  : {}", worker_summary(&self.config)),
        ];
        PickerView {
            title: " Models & Providers ".into(),
            lines: to_picker_lines(rows, selected),
            footer: "Up/Down select  Enter edit  Esc/q close".into(),
        }
    }

    fn view_workers(&self, selected: usize, notice: Option<&str>) -> PickerView {
        let mut lines: Vec<PickerLine> = self
            .config
            .roles
            .worker
            .iter()
            .enumerate()
            .map(|(i, name)| PickerLine {
                text: format!("{}. {}", i + 1, lane_summary(&self.config, name)),
                selected: i == selected,
            })
            .collect();
        if let Some(msg) = notice {
            lines.push(PickerLine {
                text: format!("! {msg}"),
                selected: false,
            });
        }
        PickerView {
            title: " Worker Fail-Over Chain ".into(),
            lines,
            footer: "Enter edit  a add  x remove  [/] reorder  Esc back".into(),
        }
    }

    fn view_providers(&self, target: EditTarget, selected: usize) -> PickerView {
        let rows = providers_for(target).iter().map(|p| p.label().to_string());
        PickerView {
            title: format!(" Provider for {} ", target_label(target)),
            lines: to_picker_lines(rows, selected),
            footer: "Enter select  Esc back".into(),
        }
    }

    fn view_models(
        &self,
        provider: ProviderId,
        effort: Effort,
        selected: usize,
        custom: Option<&str>,
    ) -> PickerView {
        let mut lines = to_picker_lines(self.model_rows(provider), selected);
        if let Some(buf) = custom {
            lines.push(PickerLine {
                text: format!("Model name: {buf}_"),
                selected: true,
            });
        }
        PickerView {
            title: format!(" Model — {} ", provider.label()),
            lines,
            footer: model_footer(provider, effort, custom.is_some()),
        }
    }

    fn model_rows(&self, provider: ProviderId) -> Vec<String> {
        let mut rows: Vec<String> = self
            .models_for(provider)
            .into_iter()
            .map(|(id, label)| {
                if id == label {
                    id
                } else {
                    format!("{id}  ({label})")
                }
            })
            .collect();
        rows.push("[Custom model...]".into());
        rows
    }
}

fn step_model_cursor(
    code: KeyCode,
    provider: ProviderId,
    effort: Effort,
    selected: usize,
    max_idx: usize,
) -> (Effort, usize) {
    match code {
        KeyCode::Tab if provider == ProviderId::Claude => (next_effort(effort), selected),
        KeyCode::Up => (effort, selected.saturating_sub(1)),
        KeyCode::Down => (effort, (selected + 1).min(max_idx)),
        _ => (effort, selected),
    }
}

fn model_footer(provider: ProviderId, effort: Effort, custom: bool) -> String {
    if custom {
        return "Type model name, Enter save, Esc back".into();
    }
    if provider == ProviderId::Claude {
        return format!(
            "effort: {} (Tab cycle)  Enter save  Esc back",
            effort_word(effort)
        );
    }
    "Enter save  Esc back".into()
}

fn to_picker_lines(rows: impl IntoIterator<Item = String>, selected: usize) -> Vec<PickerLine> {
    rows.into_iter()
        .enumerate()
        .map(|(i, text)| PickerLine {
            text,
            selected: i == selected,
        })
        .collect()
}

fn providers_for(target: EditTarget) -> &'static [ProviderId] {
    match target {
        EditTarget::Planner | EditTarget::Reviewer => providers_for_single_role(),
        EditTarget::WorkerEdit(_) | EditTarget::WorkerAdd => providers_for_worker(),
    }
}

fn target_label(target: EditTarget) -> &'static str {
    match target {
        EditTarget::Planner => "planner",
        EditTarget::Reviewer => "reviewer",
        EditTarget::WorkerEdit(_) => "worker lane",
        EditTarget::WorkerAdd => "new worker lane",
    }
}

fn next_effort(effort: Effort) -> Effort {
    match effort {
        Effort::Low => Effort::Medium,
        Effort::Medium => Effort::High,
        Effort::High => Effort::Low,
    }
}

fn effort_word(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
    }
}

pub fn lane_summary(config: &Config, name: &str) -> String {
    let Some(lane) = config.lanes.get(name) else {
        return name.to_string();
    };
    let model = lane.model.as_deref().unwrap_or("fake");
    let detail = match lane.harness {
        HarnessKind::Claude => format!("claude · {model} ({})", effort_word(lane.effort)),
        HarnessKind::Codex => {
            let prov = lane.provider.map_or("deepseek", |p| match p {
                onerig_core::config::CodexProvider::Deepseek => "deepseek",
                onerig_core::config::CodexProvider::Openai => "openai",
            });
            format!("codex/{prov} · {model}")
        }
        HarnessKind::Agy => format!(
            "agy/{} · {model}",
            if lane.is_manual() {
                "handover"
            } else if lane.is_off() {
                "off"
            } else {
                "direct"
            }
        ),
        HarnessKind::Fake => "fake".to_string(),
    };
    format!("{name} [{detail}]")
}

pub fn worker_summary(config: &Config) -> String {
    config.roles.worker.join(" → ")
}

/// Fetches live models from `agy models` (never starts a model turn).
pub fn fetch_agy_models(bin: &str) -> Vec<(String, String)> {
    let Ok(output) = std::process::Command::new(bin)
        .arg("models")
        .env("AGY_CLI_DISABLE_AUTO_UPDATE", "true")
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    agy::parse_models(&lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use onerig_core::config::{CodexProvider, EXAMPLE, parse};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn picker_edits_planner_provider_model_and_effort() {
        let cfg = parse(EXAMPLE, false).unwrap();
        let mut picker = ModelPicker::new(cfg, Vec::new());

        assert_eq!(picker.handle_key(key(KeyCode::Enter)), PickerAction::None);
        // Move to OpenAI (index 1), select it, then pick first model (gpt-5.4).
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Enter));
        let action = picker.handle_key(key(KeyCode::Enter));

        let PickerAction::Apply(updated, summary) = action else {
            panic!("expected Apply, got {action:?}");
        };
        assert!(summary.contains("OpenAI"));
        assert_eq!(
            updated.lanes[&updated.roles.planner].provider,
            Some(CodexProvider::Openai)
        );
        assert_eq!(
            updated.lanes[&updated.roles.planner].model.as_deref(),
            Some("gpt-5.4")
        );
    }

    #[test]
    fn picker_reorders_adds_and_guards_last_worker() {
        let cfg = parse(EXAMPLE, false).unwrap();
        let mut picker = ModelPicker::new(cfg, Vec::new());

        // Enter worker chain (row 2).
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Enter));

        // Move index 0 down -> swaps gemini and deepseek.
        let PickerAction::Apply(reordered, _) = picker.handle_key(key(KeyCode::Char(']'))) else {
            panic!("expected reorder");
        };
        assert_eq!(reordered.roles.worker, ["deepseek", "gemini"]);

        // Add Claude worker (provider 0, Tab -> high, model 1 -> sonnet).
        picker.handle_key(key(KeyCode::Char('a')));
        picker.handle_key(key(KeyCode::Enter));
        picker.handle_key(key(KeyCode::Tab));
        picker.handle_key(key(KeyCode::Down));
        let PickerAction::Apply(added, _) = picker.handle_key(key(KeyCode::Enter)) else {
            panic!("expected add");
        };
        assert_eq!(added.roles.worker.len(), 3);

        // Remove twice succeeds, removing last active shows inline notice instead of Apply.
        assert!(matches!(
            picker.handle_key(key(KeyCode::Char('x'))),
            PickerAction::Apply(_, _)
        ));
        assert!(matches!(
            picker.handle_key(key(KeyCode::Char('x'))),
            PickerAction::Apply(_, _)
        ));
        assert_eq!(
            picker.handle_key(key(KeyCode::Char('x'))),
            PickerAction::None
        );
        assert!(
            picker
                .view()
                .lines
                .iter()
                .any(|l| l.text.contains("at least one active"))
        );
    }

    #[test]
    fn picker_requires_risk_confirmation_for_agy_direct_and_supports_custom_model() {
        let cfg = parse(EXAMPLE, false).unwrap();
        let mut picker = ModelPicker::new(cfg, vec![("gemini-4-neon-high".into(), "Neon".into())]);

        // Reviewer (row 1) -> AgyDirect (row 3) -> live model 0 -> ConfirmRisk.
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Enter));
        for _ in 0..3 {
            picker.handle_key(key(KeyCode::Down));
        }
        picker.handle_key(key(KeyCode::Enter));
        assert_eq!(picker.handle_key(key(KeyCode::Enter)), PickerAction::None);
        assert!(picker.view().title.contains("Risk"));

        let PickerAction::Apply(updated, _) = picker.handle_key(key(KeyCode::Char('y'))) else {
            panic!("expected Apply on y");
        };
        assert!(updated.lanes[&updated.roles.reviewer].accept_vendor_terms_risk);
    }

    #[test]
    fn picker_custom_model_back_navigation_and_summaries_work() {
        let mut cfg = parse(EXAMPLE, false).unwrap();
        cfg.lanes.insert(
            "off".into(),
            Lane {
                harness: HarnessKind::Agy,
                mode: onerig_core::config::Mode::Off,
                model: Some("m".into()),
                effort: Effort::Medium,
                provider: None,
                quota_group: None,
                accept_vendor_terms_risk: false,
                bin: "agy".into(),
                script: None,
            },
        );
        assert!(lane_summary(&cfg, "off").contains("agy/off"));
        assert!(lane_summary(&cfg, "plan").contains("claude"));
        assert!(lane_summary(&cfg, "deepseek").contains("codex/deepseek"));
        assert!(lane_summary(&cfg, "gemini").contains("agy/handover"));
        assert_eq!(lane_summary(&cfg, "missing"), "missing");

        let mut picker = ModelPicker::new(cfg, Vec::new());
        // Planner -> Claude -> move to [Custom model...], test Esc back and custom entry.
        picker.handle_key(key(KeyCode::Enter));
        picker.handle_key(key(KeyCode::Enter));
        assert!(picker.view().footer.contains("effort:"));
        for _ in 0..5 {
            picker.handle_key(key(KeyCode::Down));
        }
        assert_eq!(picker.handle_key(key(KeyCode::Enter)), PickerAction::None);
        assert!(picker.view().footer.contains("Type model name"));
        // Empty enter ignores; Backspace & Esc work.
        assert_eq!(picker.handle_key(key(KeyCode::Enter)), PickerAction::None);
        picker.handle_key(key(KeyCode::Char('m')));
        picker.handle_key(key(KeyCode::Char('x')));
        picker.handle_key(key(KeyCode::Backspace));
        picker.handle_key(key(KeyCode::Char('1')));
        let PickerAction::Apply(saved, _) = picker.handle_key(key(KeyCode::Enter)) else {
            panic!("expected custom Apply");
        };
        assert_eq!(
            saved.lanes[&saved.roles.planner].model.as_deref(),
            Some("m1")
        );

        // Back from Custom -> ChooseModel -> ChooseProvider -> Roles.
        picker.handle_key(key(KeyCode::Enter));
        picker.handle_key(key(KeyCode::Enter));
        for _ in 0..5 {
            picker.handle_key(key(KeyCode::Down));
        }
        picker.handle_key(key(KeyCode::Enter));
        picker.handle_key(key(KeyCode::Esc));
        picker.handle_key(key(KeyCode::Esc));
        assert!(picker.view().title.contains("Provider for planner"));
        picker.handle_key(key(KeyCode::Esc));
        assert!(picker.view().title.contains("Models & Providers"));

        // Worker edit & ConfirmRisk cancel ('n') & WorkerChain Esc.
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Enter));
        assert!(picker.view().title.contains("Worker Fail-Over"));
        picker.handle_key(key(KeyCode::Enter));
        assert!(picker.view().title.contains("worker lane"));
        picker.handle_key(key(KeyCode::Up));
        picker.handle_key(key(KeyCode::Enter));
        picker.handle_key(key(KeyCode::Enter));
        assert!(picker.view().title.contains("Risk"));
        picker.handle_key(key(KeyCode::Char('n')));
        assert!(picker.view().title.contains("Model —"));
        picker.handle_key(key(KeyCode::Esc));
        picker.handle_key(key(KeyCode::Esc));
        picker.handle_key(key(KeyCode::Esc));
        assert!(picker.view().title.contains("Models & Providers"));

        // Exercise Up / Down / Tab / Shift+Up / '[' / ']' / 'q' across Roles & WorkerChain.
        let shift = |code| KeyEvent::new(code, KeyModifiers::SHIFT);
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Up));
        picker.handle_key(key(KeyCode::Char('x')));
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Enter));
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Up));
        assert!(matches!(
            picker.handle_key(shift(KeyCode::Down)),
            PickerAction::Apply(_, _)
        ));
        assert!(matches!(
            picker.handle_key(shift(KeyCode::Up)),
            PickerAction::Apply(_, _)
        ));
        assert!(matches!(
            picker.handle_key(key(KeyCode::Char('['))),
            PickerAction::None
        ));
        assert_eq!(
            picker.handle_key(key(KeyCode::Char('q'))),
            PickerAction::None
        );
        assert_eq!(
            picker.handle_key(key(KeyCode::Char('q'))),
            PickerAction::Close
        );

        // Model list cursor Tab/Up/Down on Claude.
        let mut m = ModelPicker::new(parse(EXAMPLE, false).unwrap(), Vec::new());
        m.handle_key(key(KeyCode::Enter));
        m.handle_key(key(KeyCode::Enter));
        m.handle_key(key(KeyCode::Down));
        m.handle_key(key(KeyCode::Up));
        m.handle_key(key(KeyCode::Tab));
        assert!(m.view().footer.contains("high"));
    }
}
