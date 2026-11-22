//! Bounded task panel. Rendering does not mutate task state or read the journal.

use std::collections::BTreeSet;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::state::{State, Status};
use crate::text::terminal_text;
use crate::theme::Palette;
use crate::ui::status::Status as UiStatus;

#[derive(Default)]
pub struct Panel {
    pub collapsed: bool,
    hidden: BTreeSet<u64>,
    displayed: BTreeSet<u64>,
    next_id: Option<u64>,
}

impl Panel {
    pub fn reset_completed(&mut self) {
        self.hidden.clear();
        self.displayed.clear();
        self.next_id = None;
    }

    pub fn next_run(&mut self) {
        self.hidden.append(&mut self.displayed);
    }

    pub fn render(
        &mut self,
        state: &State,
        palette: &Palette,
        available: usize,
    ) -> Vec<Line<'static>> {
        if self.next_id.is_some_and(|next| state.next_id < next) {
            self.reset_completed();
        }
        self.next_id = Some(state.next_id);
        let tasks = state
            .tasks
            .iter()
            .filter(|task| {
                task.status != Status::Deleted
                    && !(task.status == Status::Completed && self.hidden.contains(&task.id))
            })
            .collect::<Vec<_>>();
        if tasks.is_empty() || available == 0 {
            return Vec::new();
        }
        let done = tasks
            .iter()
            .filter(|task| task.status == Status::Completed)
            .count();
        let active = done != tasks.len();
        let mut lines = vec![Line::styled(
            format!("Todos ({done}/{})", tasks.len()),
            Style::default().fg(if active { palette.accent } else { palette.dim }),
        )];
        if self.collapsed {
            if available > 1 {
                lines.push(Line::styled(
                    "Ctrl+T to expand",
                    Style::default().fg(palette.dim),
                ));
            }
            return lines;
        }
        let budget = available.min(12).saturating_sub(1);
        let overflow = tasks.len() > budget;
        let body = budget.saturating_sub(usize::from(overflow));
        let mut kept = tasks
            .iter()
            .filter(|task| task.status != Status::Completed)
            .take(body)
            .map(|task| task.id)
            .collect::<BTreeSet<_>>();
        for task in &tasks {
            if kept.len() >= body {
                break;
            }
            if task.status == Status::Completed {
                kept.insert(task.id);
            }
        }
        let show_ids = tasks.iter().any(|task| !task.blocked_by.is_empty());
        let visible = tasks
            .iter()
            .filter(|task| kept.contains(&task.id))
            .collect::<Vec<_>>();
        for (index, task) in visible.iter().enumerate() {
            let branch = if !overflow && index + 1 == visible.len() {
                "└─ "
            } else {
                "├─ "
            };
            let mut spans = vec![
                Span::styled(branch, Style::default().fg(palette.dim)),
                UiStatus::from(task.status).span(palette),
            ];
            if show_ids {
                spans.push(Span::styled(
                    format!("#{} ", task.id),
                    Style::default().fg(palette.dim),
                ));
            }
            let mut style = Style::default().fg(if task.status == Status::InProgress {
                palette.accent
            } else if task.status == Status::Completed {
                palette.muted
            } else {
                palette.text
            });
            if task.status == Status::Completed {
                style = style.add_modifier(Modifier::CROSSED_OUT);
            }
            spans.push(Span::styled(terminal_text(&task.subject, false), style));
            if task.status == Status::InProgress
                && let Some(form) = &task.active_form
            {
                spans.push(Span::styled(
                    format!(" ({})", terminal_text(form, false)),
                    Style::default().fg(palette.muted),
                ));
            }
            if !task.blocked_by.is_empty() {
                spans.push(Span::styled(
                    format!(
                        " blockedBy: {}",
                        task.blocked_by
                            .iter()
                            .map(|id| format!("#{id}"))
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                    Style::default().fg(palette.dim),
                ));
            }
            lines.push(Line::from(spans));
        }
        for task in &tasks {
            if task.status == Status::Completed {
                self.displayed.insert(task.id);
            }
        }
        if overflow && budget > 0 {
            let completed = tasks
                .iter()
                .filter(|task| !kept.contains(&task.id) && task.status == Status::Completed)
                .count();
            let pending = tasks.len() - kept.len() - completed;
            lines.push(Line::styled(
                format!(
                    "└─ +{} more ({completed} completed, {pending} pending)",
                    tasks.len() - kept.len()
                ),
                Style::default().fg(palette.dim),
            ));
        }
        lines
    }
}
