//! Ratatui rendering for the interactive chat screen and `/model` modal.

use onerig_core::turn::TaskMark;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::chat::picker::ModelPicker;
use crate::chat::state::Chat;

const PANEL_WIDTH: u16 = 36;
const MIN_PANEL_COLS: u16 = 80;

pub fn render(chat: &Chat, frame: &mut Frame) {
    let [title, body, input, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_title(chat, frame, title);
    if body.width >= MIN_PANEL_COLS {
        let [left, right] =
            Layout::horizontal([Constraint::Min(20), Constraint::Length(PANEL_WIDTH)]).areas(body);
        render_conversation(chat, frame, left);
        render_panel(chat, frame, right);
    } else {
        render_conversation(chat, frame, body);
    }
    render_input(chat, frame, input);
    render_footer(chat, frame, footer);

    if let Some(picker) = &chat.picker {
        render_picker(picker, frame, frame.area());
    }
}

fn render_title(chat: &Chat, frame: &mut Frame, area: Rect) {
    let status = match (&chat.main_busy, chat.side_busy) {
        (Some(main), true) => format!(" [{main} + btw]"),
        (Some(main), false) => format!(" [{main}]"),
        (None, true) => " [btw]".to_string(),
        (None, false) => String::new(),
    };
    let text = format!(" onerig chat{status}");
    frame.render_widget(Paragraph::new(text), area);
}

fn render_conversation(chat: &Chat, frame: &mut Frame, area: Rect) {
    let block = Block::bordered().title(" Conversation ");
    let inner_width = usize::from(area.width.saturating_sub(2)).max(1);
    let inner_height = usize::from(area.height.saturating_sub(2)).max(1);
    let mut wrapped: Vec<String> = Vec::new();
    for line in &chat.lines {
        wrapped.extend(wrap_text(line, inner_width));
    }
    let total = wrapped.len();
    let end = total.saturating_sub(chat.scroll_from_bottom);
    let start = end.saturating_sub(inner_height);
    let visible: Vec<Line<'_>> = wrapped[start..end]
        .iter()
        .map(|l| Line::from(l.as_str()))
        .collect();
    frame.render_widget(Paragraph::new(visible).block(block), area);
}

fn render_panel(chat: &Chat, frame: &mut Frame, area: Rect) {
    let lanes_len = u16::try_from(chat.lanes.len() + chat.role_summary.len() + 3).unwrap_or(10);
    let [tasks_area, lanes_area] =
        Layout::vertical([Constraint::Min(4), Constraint::Length(lanes_len)]).areas(area);

    let task_lines = panel_task_lines(chat);
    let task_title = match chat.active_run.as_ref().and_then(|r| r.lane.as_deref()) {
        Some(lane) => format!(" Tasks ({lane}) "),
        None if chat.waiting_plan.is_some() => " Tasks (proposed) ".to_string(),
        None => " Tasks ".to_string(),
    };
    frame.render_widget(
        Paragraph::new(task_lines).block(Block::bordered().title(task_title)),
        tasks_area,
    );

    let mut lane_lines: Vec<Line<'_>> = chat
        .role_summary
        .iter()
        .map(|s| Line::from(format!(" {s}")))
        .collect();
    lane_lines.push(Line::from(""));
    for lane in &chat.lanes {
        lane_lines.push(Line::from(format!(
            " {:<10} {:<6} {}",
            lane.name, lane.harness, lane.status
        )));
    }
    frame.render_widget(
        Paragraph::new(lane_lines).block(Block::bordered().title(" Roles & Lanes ")),
        lanes_area,
    );
}

fn panel_task_lines(chat: &Chat) -> Vec<Line<'_>> {
    if let Some(run) = &chat.active_run {
        return run
            .tasks
            .iter()
            .enumerate()
            .map(|(i, (title, mark))| {
                let icon = match mark {
                    TaskMark::Done => "✓",
                    TaskMark::Current => "▸",
                    TaskMark::Waiting => "·",
                };
                Line::from(format!(" {icon} t{} {title}", i + 1))
            })
            .collect();
    }
    if let Some(waiting) = &chat.waiting_plan {
        return waiting
            .plan
            .tasks
            .iter()
            .enumerate()
            .map(|(i, t)| Line::from(format!(" · t{} {}", i + 1, t.title)))
            .collect();
    }
    vec![Line::from(" (no active plan)")]
}

fn render_input(chat: &Chat, frame: &mut Frame, area: Rect) {
    let line = format!("> {}", chat.input);
    frame.render_widget(
        Paragraph::new(line).block(Block::bordered().title(" Message ")),
        area,
    );
    if chat.picker.is_none() && area.width > 4 && area.height > 2 {
        let prefix = 2u16;
        let offset = u16::try_from(chat.cursor).unwrap_or(u16::MAX);
        let max_x = area.x + area.width.saturating_sub(2);
        let x = (area.x + 1 + prefix + offset).min(max_x);
        frame.set_cursor_position((x, area.y + 1));
    }
}

fn render_footer(chat: &Chat, frame: &mut Frame, area: Rect) {
    let text = if chat.main_busy.is_some() {
        " /btw <q> ask meanwhile  /stop cancel  /quit exit  PgUp/PgDn scroll"
    } else if chat.waiting_plan.is_some() {
        " /go approve plan  /model providers  @lane <task>  /quit exit"
    } else {
        " Enter send  /model providers  @lane <task>  PgUp/PgDn scroll  /quit exit"
    };
    frame.render_widget(Paragraph::new(text), area);
}

fn render_picker(picker: &ModelPicker, frame: &mut Frame, area: Rect) {
    let view = picker.view();
    let width = area.width.saturating_sub(6).clamp(32, 74);
    let height = u16::try_from(view.lines.len() + 4)
        .unwrap_or(14)
        .min(area.height.saturating_sub(2))
        .max(7);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let mut lines: Vec<Line<'_>> = view
        .lines
        .iter()
        .map(|item| {
            let prefix = if item.selected { " ▸ " } else { "   " };
            let style = if item.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Line::styled(format!("{prefix}{}", item.text), style)
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(format!(" {}", view.footer)));

    let widget = Paragraph::new(lines)
        .block(Block::bordered().title(view.title))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, popup);
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut count = 0usize;
    for ch in text.chars() {
        if count >= width {
            out.push(std::mem::take(&mut current));
            count = 0;
        }
        current.push(ch);
        count += 1;
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use onerig_core::config::{EXAMPLE, parse};
    use onerig_core::plan::{Plan, Task};
    use onerig_core::session::TranscriptEntry;
    use onerig_core::turn::Proposal;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen_text(chat: &Chat, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(chat, frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_panel_at_80_cols_and_hides_it_below_80() {
        let cfg = parse(EXAMPLE, false).unwrap();
        let mut chat = Chat::new(&cfg);
        chat.apply_transcript(&TranscriptEntry::Owner {
            text: "add sub".into(),
            btw: false,
        });
        chat.apply_transcript(&TranscriptEntry::Planner {
            text: "Proposed.".into(),
            plan: Some(Proposal {
                goal: "Add sub".into(),
                plan: Plan {
                    tasks: vec![Task {
                        id: "t1".into(),
                        title: "Add subtract".into(),
                        instructions: "do".into(),
                        acceptance: vec!["ok".into()],
                        lane: None,
                        from_owner: false,
                    }],
                },
            }),
            btw: false,
        });

        let wide = screen_text(&chat, 100, 22);
        let narrow = screen_text(&chat, 70, 22);

        assert!(wide.contains("Tasks (proposed)"));
        assert!(wide.contains("Roles & Lanes"));
        assert!(wide.contains("t1 Add subtract"));
        assert!(!narrow.contains("Roles & Lanes"));
        assert!(narrow.contains("owner → planner: add sub"));
    }

    #[test]
    fn renders_model_picker_modal_when_open() {
        let cfg = parse(EXAMPLE, false).unwrap();
        let mut chat = Chat::new(&cfg);
        chat.open_picker(&cfg);

        let view = screen_text(&chat, 90, 22);

        assert!(view.contains("Models & Providers"));
        assert!(view.contains("opus"));
    }
}
