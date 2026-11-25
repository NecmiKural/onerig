//! Pure chat screen state machine (design sections 6, 8 and 9).

use onerig_core::config::Config;
use onerig_core::session::TranscriptEntry;
use onerig_core::store::{LogEntry, LogRecord, PolicyEvent, StateEvent};
use onerig_core::turn::{ActiveRun, Proposal, TaskMark};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::chat::format::{LogFormatter, format_transcript_entry};
use crate::chat::picker::{ModelPicker, PickerAction, lane_summary, worker_summary};
use crate::lanes::lanes_in_roles;
use crate::plain;

const MAX_CHAT_LINES: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelLane {
    pub name: String,
    pub harness: &'static str,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPanel {
    pub run_id: Option<String>,
    pub goal: String,
    pub tasks: Vec<(String, TaskMark)>,
    pub lane: Option<String>,
    pub attempt: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CommandAction {
    None,
    SendPlanner(String),
    Go,
    DirectLane { lane: String, text: String },
    Btw(String),
    OpenModelPicker,
    SaveConfig(Config, String),
    StopMain,
    Quit,
}

#[derive(Debug)]
pub struct Chat {
    pub lines: Vec<String>,
    pub scroll_from_bottom: usize,
    pub input: String,
    pub cursor: usize,
    pub history: Vec<String>,
    pub history_pos: Option<usize>,
    pub waiting_plan: Option<Proposal>,
    pub main_busy: Option<String>,
    pub side_busy: bool,
    pub active_run: Option<RunPanel>,
    pub lanes: Vec<PanelLane>,
    pub role_summary: [String; 3],
    pub picker: Option<ModelPicker>,
    pub agy_models: Vec<(String, String)>,
}

impl Chat {
    pub fn new(config: &Config) -> Chat {
        let mut chat = Chat {
            lines: Vec::new(),
            scroll_from_bottom: 0,
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_pos: None,
            waiting_plan: None,
            main_busy: None,
            side_busy: false,
            active_run: None,
            lanes: Vec::new(),
            role_summary: [String::new(), String::new(), String::new()],
            picker: None,
            agy_models: Vec::new(),
        };
        chat.refresh_config(config);
        chat
    }

    pub fn refresh_config(&mut self, config: &Config) {
        let prev: std::collections::BTreeMap<String, String> =
            self.lanes.drain(..).map(|l| (l.name, l.status)).collect();
        self.lanes = lanes_in_roles(config)
            .into_iter()
            .map(|name| {
                let lane = &config.lanes[name];
                let status = if lane.is_off() {
                    "off".to_string()
                } else if lane.is_manual() {
                    "manual".to_string()
                } else {
                    prev.get(name).cloned().unwrap_or_else(|| "…".to_string())
                };
                PanelLane {
                    name: name.clone(),
                    harness: lane.harness.name(),
                    status,
                }
            })
            .collect();
        self.role_summary = [
            format!("plan: {}", lane_summary(config, &config.roles.planner)),
            format!("rev : {}", lane_summary(config, &config.roles.reviewer)),
            format!("work: {}", worker_summary(config)),
        ];
        if let Some(picker) = &mut self.picker {
            picker.sync_config(config.clone());
        }
    }

    pub fn push_lines(&mut self, new_lines: impl IntoIterator<Item = String>) {
        self.lines.extend(new_lines);
        let excess = self.lines.len().saturating_sub(MAX_CHAT_LINES);
        if excess > 0 {
            self.lines.drain(..excess);
        }
        if self.scroll_from_bottom > 0 {
            self.scroll_from_bottom = self.scroll_from_bottom.min(self.lines.len());
        }
    }

    pub fn push_system(&mut self, text: &str) {
        self.push_lines(text.lines().map(|l| format!("onerig  {l}")));
    }

    pub fn apply_transcript(&mut self, entry: &TranscriptEntry) {
        if let TranscriptEntry::Owner { text, btw: false } = entry {
            self.record_history(text);
        }
        if let TranscriptEntry::Planner {
            plan: Some(proposal),
            btw: false,
            ..
        } = entry
        {
            self.waiting_plan = Some(proposal.clone());
        }
        if let TranscriptEntry::Run {
            from_owner: false, ..
        } = entry
        {
            self.waiting_plan = None;
        }
        self.push_lines(format_transcript_entry(entry));
    }

    pub fn apply_log(&mut self, record: &LogRecord, formatter: &mut LogFormatter) {
        self.update_panel_from_log(record);
        self.push_lines(formatter.format(record));
    }

    fn update_panel_from_log(&mut self, record: &LogRecord) {
        match &record.entry {
            LogEntry::State(event) => self.update_run_panel(record, event),
            LogEntry::Policy(event) => self.update_lane_panel(record, event),
            LogEntry::Harness(_) | LogEntry::Outcome(_) => {}
        }
    }

    fn update_run_panel(&mut self, record: &LogRecord, event: &StateEvent) {
        match event {
            StateEvent::Run { run_id, goal, .. } => {
                self.active_run = Some(RunPanel {
                    run_id: Some(run_id.clone()),
                    goal: goal.clone(),
                    tasks: Vec::new(),
                    lane: None,
                    attempt: None,
                });
            }
            StateEvent::Plan { titles, .. } => {
                if let Some(panel) = &mut self.active_run {
                    panel.tasks = titles
                        .iter()
                        .map(|t| (t.clone(), TaskMark::Waiting))
                        .collect();
                }
            }
            StateEvent::Done { .. } => {
                if let Some(panel) = &mut self.active_run {
                    panel
                        .tasks
                        .iter_mut()
                        .for_each(|(_, mark)| *mark = TaskMark::Done);
                }
            }
            StateEvent::Phase { .. }
            | StateEvent::Verdict { .. }
            | StateEvent::Commit { .. }
            | StateEvent::Paused { .. }
            | StateEvent::Failed { .. } => {}
        }
        if let Some(task) = record.task {
            self.mark_active_task(task);
        }
    }

    fn mark_active_task(&mut self, task: usize) {
        let Some(panel) = &mut self.active_run else {
            return;
        };
        let current_idx = task.saturating_sub(1);
        for (i, (_, mark)) in panel.tasks.iter_mut().enumerate() {
            if *mark != TaskMark::Done {
                *mark = match i.cmp(&current_idx) {
                    std::cmp::Ordering::Less => TaskMark::Done,
                    std::cmp::Ordering::Equal => TaskMark::Current,
                    std::cmp::Ordering::Greater => TaskMark::Waiting,
                };
            }
        }
    }

    fn update_lane_panel(&mut self, record: &LogRecord, event: &PolicyEvent) {
        if let PolicyEvent::LaneSelected { lane, .. } = event {
            if let Some(panel) = &mut self.active_run {
                panel.lane = Some(lane.clone());
                panel.attempt = record.attempt;
            }
            return;
        }
        if let Some((name, status)) = lane_status_from_policy(event)
            && let Some(row) = self.lanes.iter_mut().find(|r| &r.name == name)
        {
            row.status = status;
        }
    }

    pub fn active_run_snapshot(&self) -> Option<ActiveRun> {
        let panel = self.active_run.as_ref()?;
        Some(ActiveRun {
            goal: panel.goal.clone(),
            tasks: panel.tasks.clone(),
        })
    }

    pub fn open_picker(&mut self, config: &Config) {
        self.picker = Some(ModelPicker::new(config.clone(), self.agy_models.clone()));
    }

    pub fn set_agy_models(&mut self, models: Vec<(String, String)>) {
        self.agy_models = models.clone();
        if let Some(picker) = &mut self.picker {
            picker.set_agy_models(models);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, config: &Config) -> CommandAction {
        if self.picker.is_some() {
            return self.handle_picker_key(key);
        }
        if let Some(action) = self.handle_shortcut_key(key) {
            return action;
        }
        match key.code {
            KeyCode::Enter => self.submit(config),
            _ => {
                self.edit_input(key.code);
                CommandAction::None
            }
        }
    }

    fn handle_picker_key(&mut self, key: KeyEvent) -> CommandAction {
        let Some(picker) = &mut self.picker else {
            return CommandAction::None;
        };
        match picker.handle_key(key) {
            PickerAction::None => CommandAction::None,
            PickerAction::Close => {
                self.picker = None;
                CommandAction::None
            }
            PickerAction::Apply(new_cfg, summary) => CommandAction::SaveConfig(new_cfg, summary),
        }
    }

    fn handle_shortcut_key(&mut self, key: KeyEvent) -> Option<CommandAction> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => Some(CommandAction::StopMain),
            KeyCode::Char('d') if ctrl && self.input.is_empty() => Some(CommandAction::Quit),
            KeyCode::PageUp => {
                self.scroll_from_bottom = self
                    .scroll_from_bottom
                    .saturating_add(8)
                    .min(self.lines.len());
                Some(CommandAction::None)
            }
            KeyCode::PageDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(8);
                Some(CommandAction::None)
            }
            KeyCode::Up => {
                self.history_up();
                Some(CommandAction::None)
            }
            KeyCode::Down => {
                self.history_down();
                Some(CommandAction::None)
            }
            _ => None,
        }
    }

    fn edit_input(&mut self, code: KeyCode) {
        let total = self.input.chars().count();
        match code {
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(total),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = total,
            KeyCode::Backspace => self.delete_char(true),
            KeyCode::Delete => self.delete_char(false),
            KeyCode::Char(c) => {
                let byte = char_to_byte(&self.input, self.cursor);
                self.input.insert(byte, c);
                self.cursor += 1;
            }
            _ => {}
        }
    }

    fn delete_char(&mut self, backward: bool) {
        let total = self.input.chars().count();
        if backward && self.cursor > 0 {
            self.cursor -= 1;
            let byte = char_to_byte(&self.input, self.cursor);
            self.input.remove(byte);
        } else if !backward && self.cursor < total {
            let byte = char_to_byte(&self.input, self.cursor);
            self.input.remove(byte);
        }
    }

    fn submit(&mut self, config: &Config) -> CommandAction {
        let raw = self.input.trim().to_string();
        if raw.is_empty() {
            return CommandAction::None;
        }
        if raw == "/stop" {
            self.clear_input();
            return CommandAction::StopMain;
        }
        if raw == "/quit" {
            self.clear_input();
            return CommandAction::Quit;
        }
        if let Some(action) = self.try_btw(&raw) {
            return action;
        }
        if let Some(busy) = self.main_busy.clone() {
            self.push_system(&format!("busy: {busy} — use /btw to ask meanwhile"));
            return CommandAction::None;
        }
        self.dispatch_idle(&raw, config)
    }

    fn try_btw(&mut self, raw: &str) -> Option<CommandAction> {
        let rest = raw.strip_prefix("/btw")?;
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let text = rest.trim().to_string();
        if text.is_empty() {
            self.clear_input();
            self.push_system("usage: /btw <question>");
            return Some(CommandAction::None);
        }
        if self.side_busy {
            self.push_system("busy: side turn already answering");
            return Some(CommandAction::None);
        }
        self.clear_input();
        self.record_history(raw);
        Some(if self.main_busy.is_some() {
            CommandAction::Btw(text)
        } else {
            CommandAction::SendPlanner(text)
        })
    }

    fn dispatch_idle(&mut self, raw: &str, config: &Config) -> CommandAction {
        self.clear_input();
        self.record_history(raw);
        match raw {
            "/go" => CommandAction::Go,
            "/model" => CommandAction::OpenModelPicker,
            _ if raw.starts_with('@') => self.parse_direct_lane(raw, config),
            _ if raw.starts_with('/') => {
                self.push_system(&format!("unknown command: {raw}"));
                CommandAction::None
            }
            _ => CommandAction::SendPlanner(raw.to_string()),
        }
    }

    fn parse_direct_lane(&mut self, raw: &str, config: &Config) -> CommandAction {
        let rest = &raw[1..];
        let Some((lane, text)) = rest.split_once(char::is_whitespace) else {
            self.push_system("usage: @<lane> <task>");
            return CommandAction::None;
        };
        let text = text.trim();
        if lane.is_empty() || text.is_empty() {
            self.push_system("usage: @<lane> <task>");
            return CommandAction::None;
        }
        if !config.roles.worker.iter().any(|w| w == lane) {
            self.push_system(&format!("lane {lane} is not in roles.worker"));
            return CommandAction::None;
        }
        if config.lanes[lane].is_off() {
            self.push_system(&format!("lane {lane} is off"));
            return CommandAction::None;
        }
        CommandAction::DirectLane {
            lane: lane.to_string(),
            text: text.to_string(),
        }
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.history_pos = None;
        self.scroll_from_bottom = 0;
    }

    fn record_history(&mut self, entry: &str) {
        if self.history.last().map(String::as_str) != Some(entry) {
            self.history.push(entry.to_string());
        }
    }

    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_pos {
            None => self.history.len() - 1,
            Some(pos) => pos.saturating_sub(1),
        };
        self.history_pos = Some(next);
        self.input = self.history[next].clone();
        self.cursor = self.input.chars().count();
    }

    fn history_down(&mut self) {
        let Some(pos) = self.history_pos else {
            return;
        };
        if pos + 1 >= self.history.len() {
            self.history_pos = None;
            self.input.clear();
            self.cursor = 0;
        } else {
            let next = pos + 1;
            self.history_pos = Some(next);
            self.input = self.history[next].clone();
            self.cursor = self.input.chars().count();
        }
    }
}

fn lane_status_from_policy(event: &PolicyEvent) -> Option<(&String, String)> {
    match event {
        PolicyEvent::Probe { lane, ok, .. } => {
            Some((lane, if *ok { "ok" } else { "failed" }.to_string()))
        }
        PolicyEvent::LaneExhausted { lane, .. } => Some((lane, "exhausted".to_string())),
        PolicyEvent::LaneSkipped { lane } => Some((lane, "skipped".to_string())),
        PolicyEvent::Quota {
            lane,
            remaining_fraction,
            ..
        } => Some((lane, plain::percent_left(*remaining_fraction))),
        PolicyEvent::LaneSelected { .. } | PolicyEvent::Handover { .. } => None,
    }
}

fn char_to_byte(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

#[cfg(test)]
mod tests {
    use super::*;
    use onerig_core::config::{EXAMPLE, parse};

    fn cfg() -> Config {
        parse(EXAMPLE, false).unwrap()
    }

    fn type_line(chat: &mut Chat, cfg: &Config, line: &str) -> CommandAction {
        for ch in line.chars() {
            chat.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), cfg);
        }
        chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), cfg)
    }

    #[test]
    fn busy_action_refuses_text_keeping_input_and_allows_btw_and_stop() {
        let cfg = cfg();
        let mut chat = Chat::new(&cfg);
        chat.main_busy = Some("run".into());

        assert_eq!(type_line(&mut chat, &cfg, "new goal"), CommandAction::None);
        assert_eq!(chat.input, "new goal");
        assert!(chat.lines.last().unwrap().contains("use /btw"));

        chat.input.clear();
        chat.cursor = 0;
        assert_eq!(
            type_line(&mut chat, &cfg, "/btw how far?"),
            CommandAction::Btw("how far?".into())
        );
        assert!(chat.input.is_empty());

        chat.side_busy = true;
        assert_eq!(
            type_line(&mut chat, &cfg, "/btw again"),
            CommandAction::None
        );
        assert!(chat.lines.last().unwrap().contains("side turn"));

        assert_eq!(
            chat.handle_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &cfg
            ),
            CommandAction::StopMain
        );
    }

    #[test]
    fn direct_lane_validates_worker_membership_and_history_navigates() {
        let cfg = cfg();
        let mut chat = Chat::new(&cfg);

        assert_eq!(
            type_line(&mut chat, &cfg, "@plan nope"),
            CommandAction::None
        );
        assert!(chat.lines.last().unwrap().contains("not in roles.worker"));

        assert_eq!(
            type_line(&mut chat, &cfg, "@deepseek fix typo"),
            CommandAction::DirectLane {
                lane: "deepseek".into(),
                text: "fix typo".into()
            }
        );

        chat.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &cfg);
        assert_eq!(chat.input, "@deepseek fix typo");
        chat.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &cfg);
        assert!(chat.input.is_empty());
    }

    #[test]
    fn editing_scroll_commands_picker_and_panel_updates_work() {
        let cfg = cfg();
        let mut chat = Chat::new(&cfg);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

        // Input editing: Left, Right, Home, End, Backspace, Delete.
        for c in "abcd".chars() {
            chat.handle_key(key(KeyCode::Char(c)), &cfg);
        }
        chat.handle_key(key(KeyCode::Left), &cfg);
        chat.handle_key(key(KeyCode::Backspace), &cfg);
        assert_eq!(chat.input, "abd");
        chat.handle_key(key(KeyCode::Home), &cfg);
        chat.handle_key(key(KeyCode::Delete), &cfg);
        chat.handle_key(key(KeyCode::End), &cfg);
        chat.handle_key(key(KeyCode::Left), &cfg);
        chat.handle_key(key(KeyCode::Right), &cfg);
        assert_eq!(chat.input, "bd");

        chat.input.clear();
        chat.cursor = 0;
        assert_eq!(
            chat.handle_key(
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                &cfg
            ),
            CommandAction::Quit
        );

        assert_eq!(type_line(&mut chat, &cfg, "/go"), CommandAction::Go);
        assert_eq!(
            type_line(&mut chat, &cfg, "/model"),
            CommandAction::OpenModelPicker
        );
        assert_eq!(type_line(&mut chat, &cfg, "/quit"), CommandAction::Quit);
        assert_eq!(type_line(&mut chat, &cfg, "/stop"), CommandAction::StopMain);
        assert_eq!(type_line(&mut chat, &cfg, "/bogus"), CommandAction::None);
        assert!(chat.lines.last().unwrap().contains("unknown command"));

        // Scroll PageUp / PageDown.
        for i in 0..20 {
            chat.push_system(&format!("line {i}"));
        }
        chat.handle_key(key(KeyCode::PageUp), &cfg);
        assert!(chat.scroll_from_bottom > 0);
        chat.handle_key(key(KeyCode::PageDown), &cfg);
        assert_eq!(chat.scroll_from_bottom, 0);

        // Picker intercepts keys and closes cleanly.
        chat.open_picker(&cfg);
        chat.set_agy_models(vec![("gemini-4".into(), "Gemini".into())]);
        assert_eq!(
            chat.handle_key(key(KeyCode::Esc), &cfg),
            CommandAction::None
        );
        assert!(chat.picker.is_none());

        // Panel policy & run updates.
        let mut fmt = LogFormatter::default();
        let rec = |task, entry| LogRecord {
            ts: 1,
            task,
            attempt: Some(1),
            lane: None,
            entry,
        };
        chat.apply_log(
            &rec(
                None,
                LogEntry::Policy(PolicyEvent::Probe {
                    lane: "plan".into(),
                    ok: true,
                    message: "v1".into(),
                }),
            ),
            &mut fmt,
        );
        chat.apply_log(
            &rec(
                None,
                LogEntry::Policy(PolicyEvent::LaneSkipped {
                    lane: "deepseek".into(),
                }),
            ),
            &mut fmt,
        );
        assert!(
            chat.lanes
                .iter()
                .any(|l| l.name == "deepseek" && l.status == "skipped")
        );
        chat.apply_log(
            &rec(
                Some(1),
                LogEntry::State(StateEvent::Plan {
                    tasks: 2,
                    titles: vec!["one".into(), "two".into()],
                }),
            ),
            &mut fmt,
        );
        assert!(chat.active_run_snapshot().is_none());
    }
}
