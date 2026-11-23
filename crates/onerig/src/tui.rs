//! The terminal UI (design section 14): one screen fed by the same log
//! records as the plain output. It redraws on every record; `q` or Ctrl-C
//! set the stop flag, and the run then pauses as interrupted.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use onerig_core::config::Config;
use onerig_core::store::{LogEntry, LogRecord, PolicyEvent, StateEvent};
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph};

use crate::lanes::lanes_in_roles;
use crate::plain;

/// Shown while the run has nothing to report yet, which is while the lanes
/// are probed. Every line has the same width so centring keeps the shape.
const RING: [&str; 6] = [
    r#"   .-""""""""-.   "#,
    r#" .'  .-""""-.  '. "#,
    r#"/   /        \   \"#,
    r#"\   \        /   /"#,
    r#" '.  '-....-'  .' "#,
    r#"   '-........-'   "#,
];
const TAGLINE: [&str; 2] = ["o n e r i g", "one rig for every agent harness"];

/// Activity lines kept for the right-hand panel.
const ACTIVITY_LINES: usize = 500;
const LEFT_WIDTH: u16 = 36;
const KEY_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
struct LaneRow {
    name: String,
    harness: &'static str,
    status: String,
}

#[derive(Debug, Default)]
pub struct Screen {
    run: Option<(String, String)>,
    tasks: Vec<String>,
    /// 1-based index of the task being worked on.
    current: Option<usize>,
    attempt: Option<u32>,
    lane: Option<String>,
    lanes: Vec<LaneRow>,
    activity: VecDeque<String>,
    /// The plain line of the record that ended the run.
    ending: Option<String>,
    done: bool,
}

impl Screen {
    pub fn new(config: &Config) -> Screen {
        let lanes = lanes_in_roles(config)
            .into_iter()
            .map(|name| {
                let lane = &config.lanes[name];
                let status = if lane.is_off() {
                    "off"
                } else if lane.is_manual() {
                    "manual"
                } else {
                    "…"
                };
                LaneRow {
                    name: name.clone(),
                    harness: lane.harness.name(),
                    status: status.to_string(),
                }
            })
            .collect();
        Screen {
            lanes,
            ..Screen::default()
        }
    }

    pub fn apply(&mut self, record: &LogRecord) {
        if let Some(task) = record.task {
            self.current = Some(task);
        }
        match &record.entry {
            LogEntry::State(event) => self.apply_state(event, record),
            LogEntry::Policy(event) => self.apply_policy(event, record),
            LogEntry::Harness(_) | LogEntry::Outcome(_) => {}
        }
        if let Some(line) = plain::line(record) {
            self.activity.extend(line.lines().map(str::to_string));
            let excess = self.activity.len().saturating_sub(ACTIVITY_LINES);
            self.activity.drain(..excess);
        }
    }

    fn apply_state(&mut self, event: &StateEvent, record: &LogRecord) {
        match event {
            StateEvent::Run { run_id, goal, .. } => self.run = Some((run_id.clone(), goal.clone())),
            StateEvent::Plan { titles, .. } => self.tasks = titles.clone(),
            StateEvent::Done { .. } => {
                self.done = true;
                self.ending = plain::line(record);
            }
            StateEvent::Paused { .. } | StateEvent::Failed { .. } => {
                self.ending = plain::line(record)
            }
            StateEvent::Phase { .. } | StateEvent::Verdict { .. } | StateEvent::Commit { .. } => {}
        }
    }

    fn apply_policy(&mut self, event: &PolicyEvent, record: &LogRecord) {
        let (lane, status) = match event {
            PolicyEvent::Probe { lane, ok, .. } => (lane, if *ok { "ok" } else { "failed" }),
            PolicyEvent::LaneExhausted { lane, .. } => (lane, "exhausted"),
            PolicyEvent::LaneSkipped { lane } => (lane, "skipped"),
            PolicyEvent::LaneSelected { lane, .. } => {
                self.lane = Some(lane.clone());
                self.attempt = record.attempt;
                return;
            }
            PolicyEvent::Handover { .. } => return,
        };
        if let Some(row) = self.lanes.iter_mut().find(|row| &row.name == lane) {
            row.status = status.to_string();
        }
    }

    /// What stays on the terminal after the screen closes.
    pub fn closing_lines(&self) -> Vec<String> {
        self.ending
            .iter()
            .flat_map(|line| line.lines().map(str::to_string))
            .collect()
    }

    fn task_lines(&self) -> Vec<Line<'_>> {
        let current = self.current.unwrap_or(0);
        self.tasks
            .iter()
            .enumerate()
            .map(|(i, title)| {
                let number = i + 1;
                let mark = if self.done || number < current {
                    "ok"
                } else if number == current {
                    ">>"
                } else {
                    ".."
                };
                Line::from(format!(" {mark}  t{number} {title}"))
            })
            .collect()
    }

    fn lane_lines(&self) -> Vec<Line<'_>> {
        self.lanes
            .iter()
            .map(|row| {
                Line::from(format!(
                    " {:<10} {:<6} {}",
                    row.name, row.harness, row.status
                ))
            })
            .collect()
    }

    fn header(&self) -> String {
        match (self.current, &self.lane) {
            (Some(task), Some(lane)) => {
                let attempt = self
                    .attempt
                    .map_or(String::new(), |a| format!("  attempt {a}"));
                format!(" t{task}{attempt}  lane: {lane} ")
            }
            _ => " Activity ".to_string(),
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        let [title, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        let [left, right] =
            Layout::horizontal([Constraint::Length(LEFT_WIDTH), Constraint::Min(10)]).areas(body);
        let lane_height = u16::try_from(self.lanes.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2);
        let [tasks, lanes] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(lane_height)]).areas(left);

        let heading = match &self.run {
            Some((id, goal)) => format!(" onerig  run {id}   goal: {goal}"),
            None => " onerig".to_string(),
        };
        frame.render_widget(Paragraph::new(heading), title);
        frame.render_widget(
            Paragraph::new(self.task_lines()).block(Block::bordered().title(" Tasks ")),
            tasks,
        );
        frame.render_widget(
            Paragraph::new(self.lane_lines()).block(Block::bordered().title(" Lanes ")),
            lanes,
        );
        let block = Block::bordered().title(self.header());
        if self.activity.is_empty() {
            frame.render_widget(splash(usize::from(right.height)).block(block), right);
        } else {
            let visible = usize::from(right.height.saturating_sub(2));
            let skip = self.activity.len().saturating_sub(visible);
            let activity: Vec<Line<'_>> = self
                .activity
                .iter()
                .skip(skip)
                .map(|l| Line::from(l.as_str()))
                .collect();
            frame.render_widget(Paragraph::new(activity).block(block), right);
        }
        let footer_text = match &self.ending {
            Some(ending) => format!(" {}", ending.lines().next().unwrap_or_default()),
            None => " q quit (the run can be resumed)".to_string(),
        };
        frame.render_widget(Paragraph::new(footer_text), footer);
    }
}

/// The ring and the name, centred in a panel of the given height.
fn splash(height: usize) -> Paragraph<'static> {
    let art = RING.len() + 1 + TAGLINE.len();
    let top = height.saturating_sub(2).saturating_sub(art) / 2;
    let lines: Vec<Line<'static>> = std::iter::repeat_n(Line::from(""), top)
        .chain(RING.iter().map(|l| Line::from(*l)))
        .chain(std::iter::once(Line::from("")))
        .chain(TAGLINE.iter().map(|l| Line::from(*l)))
        .collect();
    Paragraph::new(lines).centered()
}

/// Watches the keyboard until `finished` is set; `q` and Ctrl-C set `stop`.
pub fn watch_keys(stop: Arc<AtomicBool>, finished: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::spawn(move || key_loop(&stop, &finished, next_key))
}

fn next_key() -> Option<Event> {
    if event::poll(KEY_POLL).unwrap_or(false) {
        event::read().ok()
    } else {
        None
    }
}

fn key_loop(stop: &AtomicBool, finished: &AtomicBool, mut next: impl FnMut() -> Option<Event>) {
    while !finished.load(Ordering::Relaxed) {
        if next().is_some_and(|event| is_stop_key(&event)) {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

/// In raw mode Ctrl-C arrives as a key, not as a signal.
fn is_stop_key(event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    let ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
    key.code == KeyCode::Char('q') || ctrl_c
}

#[cfg(test)]
mod tests {
    use super::*;
    use onerig_core::config;
    use onerig_core::runner::{HarnessEvent, Outcome};
    use onerig_core::store::PauseReason;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    const CONFIG: &str = r#"
[roles]
planner = ["plan"]
reviewer = ["plan"]
worker = ["deepseek", "gemini"]

[lanes.plan]
harness = "claude"
model = "sonnet"

[lanes.deepseek]
harness = "codex"
model = "deepseek-flash"

[lanes.gemini]
harness = "agy"
model = "m"
"#;

    fn record(
        task: Option<usize>,
        attempt: Option<u32>,
        lane: Option<&str>,
        entry: LogEntry,
    ) -> LogRecord {
        LogRecord {
            ts: 1_795_464_901_000,
            task,
            attempt,
            lane: lane.map(str::to_string),
            entry,
        }
    }

    fn screen_after(records: &[LogRecord]) -> Screen {
        let mut screen = Screen::new(&config::parse(CONFIG, false).unwrap());
        records.iter().for_each(|r| screen.apply(r));
        screen
    }

    fn a_run_on_task_two() -> Vec<LogRecord> {
        vec![
            record(
                None,
                None,
                None,
                LogEntry::State(StateEvent::Run {
                    run_id: "r1".into(),
                    goal: "Add retries".into(),
                    branch: "onerig/r1".into(),
                }),
            ),
            record(
                None,
                None,
                None,
                LogEntry::Policy(PolicyEvent::Probe {
                    lane: "deepseek".into(),
                    ok: true,
                    message: "codex-cli 0.171.0".into(),
                }),
            ),
            record(
                None,
                None,
                None,
                LogEntry::State(StateEvent::Plan {
                    tasks: 2,
                    titles: vec!["Add a helper".into(), "Use it".into()],
                }),
            ),
            record(
                Some(2),
                Some(1),
                Some("deepseek"),
                LogEntry::Policy(PolicyEvent::LaneSelected {
                    lane: "deepseek".into(),
                    manual: false,
                }),
            ),
            record(
                Some(2),
                Some(1),
                Some("deepseek"),
                LogEntry::Harness(HarnessEvent::Tool {
                    name: "edit".into(),
                    detail: "src/http.rs".into(),
                }),
            ),
        ]
    }

    fn rendered(screen: &Screen) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(90, 16)).unwrap();
        terminal.draw(|frame| screen.render(frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn lanes_start_from_the_configuration() {
        let screen = screen_after(&[]);

        let rows: Vec<(&str, &str, &str)> = screen
            .lanes
            .iter()
            .map(|r| (r.name.as_str(), r.harness, r.status.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("plan", "claude", "…"),
                ("deepseek", "codex", "…"),
                ("gemini", "agy", "manual")
            ]
        );
    }

    #[test]
    fn records_fill_in_the_run_the_tasks_the_lane_and_the_activity() {
        let screen = screen_after(&a_run_on_task_two());

        assert_eq!(screen.run, Some(("r1".into(), "Add retries".into())));
        assert_eq!(screen.current, Some(2));
        assert_eq!(
            (screen.attempt, screen.lane.as_deref()),
            (Some(1), Some("deepseek"))
        );
        assert_eq!(screen.lanes[1].status, "ok");
        assert!(
            screen
                .activity
                .back()
                .unwrap()
                .ends_with("t2 a1 deepseek  tool edit src/http.rs")
        );
    }

    #[test]
    fn the_screen_shows_goal_tasks_lanes_activity_and_the_quit_key() {
        let screen = screen_after(&a_run_on_task_two());

        let lines = rendered(&screen);

        let text = lines.join("\n");
        assert!(lines[0].starts_with(" onerig  run r1   goal: Add retries"));
        assert!(text.contains(" ok  t1 Add a helper"));
        assert!(text.contains(" >>  t2 Use it"));
        assert!(text.contains(" deepseek   codex  ok"));
        assert!(text.contains(" gemini     agy    manual"));
        assert!(text.contains(" t2  attempt 1  lane: deepseek "));
        assert!(text.contains("tool edit src/http.rs"));
        assert!(
            lines
                .last()
                .unwrap()
                .starts_with(" q quit (the run can be resumed)")
        );
    }

    #[test]
    fn exhausted_and_skipped_lanes_are_marked() {
        let mut records = a_run_on_task_two();
        records.push(record(
            Some(2),
            None,
            None,
            LogEntry::Policy(PolicyEvent::LaneExhausted {
                lane: "deepseek".into(),
                until: None,
            }),
        ));
        records.push(record(
            Some(2),
            None,
            None,
            LogEntry::Policy(PolicyEvent::LaneSkipped {
                lane: "plan".into(),
            }),
        ));

        let screen = screen_after(&records);

        assert_eq!(screen.lanes[1].status, "exhausted");
        assert_eq!(screen.lanes[0].status, "skipped");
    }

    #[test]
    fn a_pause_ends_the_screen_with_its_full_message() {
        let mut records = a_run_on_task_two();
        records.push(record(
            Some(2),
            Some(1),
            Some("deepseek"),
            LogEntry::Outcome(Outcome::failed(
                onerig_core::runner::FailureKind::Quota,
                "balance",
            )),
        ));
        records.push(record(
            None,
            None,
            None,
            LogEntry::State(StateEvent::Paused {
                reason: PauseReason::NoLane,
                message: "deepseek: quota exhausted\nAfterwards: onerig resume r1".into(),
            }),
        ));

        let screen = screen_after(&records);

        assert_eq!(
            screen.closing_lines(),
            [
                "20:15:01 paused    no_lane: deepseek: quota exhausted",
                "Afterwards: onerig resume r1"
            ]
        );
        assert!(
            rendered(&screen)
                .last()
                .unwrap()
                .starts_with(" 20:15:01 paused    no_lane")
        );
    }

    #[test]
    fn a_finished_run_marks_every_task_done() {
        let mut records = a_run_on_task_two();
        records.push(record(
            None,
            None,
            None,
            LogEntry::State(StateEvent::Done {
                branch: "onerig/r1".into(),
            }),
        ));

        let screen = screen_after(&records);

        let text = rendered(&screen).join("\n");
        assert!(text.contains(" ok  t2 Use it"));
        assert_eq!(
            screen.closing_lines(),
            ["20:15:01 done      branch onerig/r1"]
        );
    }

    #[test]
    fn the_ring_greets_until_the_first_activity_arrives() {
        let empty = screen_after(&[]);
        let busy = screen_after(&a_run_on_task_two());

        let greeting = rendered(&empty).join("\n");
        let working = rendered(&busy).join("\n");

        assert!(greeting.contains(r#".-""""""""-."#));
        assert!(greeting.contains(r#"/   /        \   \"#));
        assert!(greeting.contains("one rig for every agent harness"));
        assert!(!working.contains("one rig for every agent harness"));
    }

    #[test]
    fn the_ring_lines_share_one_width() {
        assert!(
            RING.iter()
                .all(|line| line.chars().count() == RING[0].chars().count())
        );
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(ratatui::crossterm::event::KeyEvent::new(code, modifiers))
    }

    #[test]
    fn q_and_ctrl_c_stop_the_run_and_other_keys_do_not() {
        assert!(is_stop_key(&key(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(is_stop_key(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(!is_stop_key(&key(KeyCode::Char('c'), KeyModifiers::NONE)));
        assert!(!is_stop_key(&key(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(!is_stop_key(&Event::FocusGained));
    }

    #[test]
    fn the_key_loop_sets_stop_and_ends_when_the_run_is_finished() {
        let stop = AtomicBool::new(false);
        let finished = AtomicBool::new(false);
        let mut keys = vec![
            None,
            Some(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(key(KeyCode::Char('q'), KeyModifiers::NONE)),
        ]
        .into_iter();

        key_loop(&stop, &finished, || {
            let next = keys.next();
            if next.is_none() {
                finished.store(true, Ordering::Relaxed);
            }
            next.flatten()
        });

        assert!(stop.load(Ordering::Relaxed));
    }

    #[test]
    fn only_the_newest_activity_lines_are_kept() {
        let mut screen = screen_after(&[]);
        for i in 0..ACTIVITY_LINES + 5 {
            screen.apply(&record(
                Some(1),
                Some(1),
                Some("w"),
                LogEntry::Harness(HarnessEvent::Text {
                    text: format!("line {i}"),
                }),
            ));
        }

        assert_eq!(screen.activity.len(), ACTIVITY_LINES);
        assert!(screen.activity.front().unwrap().ends_with("line 5"));
    }
}
