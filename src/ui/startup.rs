//! Startup sections are presentation only; they never enter the model context.

use ratatui::style::Style;
use ratatui::text::Line;

use crate::context::Resources;
use crate::providers::proxy::Status;
use crate::text::terminal_text;
use crate::theme::Palette;

pub fn header(
    config: &crate::config::Config,
    resources: &Resources,
    status: &Status,
    palette: &Palette,
    expanded: bool,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::styled(
            format!("mh  v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(palette.accent),
        ),
        Line::styled(
            "Esc interrupt · Ctrl+C clear/exit · / commands · Ctrl+O more",
            Style::default().fg(palette.muted),
        ),
    ];
    if expanded {
        for hint in [
            "Enter submit/steer · Alt+Enter follow-up · Alt+Q/Alt+Up edit queued messages",
            "↑/↓ edit/history · Ctrl+J or Shift+Enter newline · Tab completion",
            "Ctrl+V / Alt+V paste image · paste large text again to expand",
            "Ctrl+P cycle fixed-effort model entries · /resume choose a session",
            "Ctrl+Z undo/recover · Ctrl+U/K delete line · Ctrl+D delete forward · /skills create",
            "Drag transcript text or Shift+arrows in review · Ctrl+C / Alt+C copy",
            "Ctrl+O detailed transcript · ↑/↓ scroll there · Ctrl+↑/↓ jump messages",
            "PgUp/PgDn scroll · Ctrl+Home start · Ctrl+End latest · Esc return",
        ] {
            lines.push(Line::styled(hint, Style::default().fg(palette.muted)));
        }
    } else {
        lines.push(Line::styled(
            "Press Ctrl+O to show full startup help and loaded resources.",
            Style::default().fg(palette.dim),
        ));
    }
    lines.push(Line::styled(
        "Use /help for mh's retained commands and keyboard controls.",
        Style::default().fg(palette.dim),
    ));
    let model_count = config
        .providers
        .values()
        .map(|provider| provider.models.len())
        .sum::<usize>();
    let models = config
        .providers
        .iter()
        .flat_map(|(provider, settings)| {
            settings.models.iter().map(move |model| {
                format!("{provider}/{} ({})", model.local_id(), model.effort_label())
            })
        })
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(Line::default());
    lines.push(Line::styled(
        format!(
            "Model scope: {} (Ctrl+P to cycle)",
            if expanded {
                terminal_text(&models, false)
            } else {
                format!("{model_count} configured entries · /model choose")
            }
        ),
        Style::default().fg(palette.muted),
    ));
    lines.push(Line::default());
    lines.extend(resource_lines(resources, status, palette));
    if expanded {
        for skill in &resources.skills {
            lines.push(Line::styled(
                format!(
                    "  {}: {}",
                    terminal_text(&skill.name, false),
                    crate::text::display_path(&skill.path)
                ),
                Style::default().fg(palette.dim),
            ));
        }
    }
    lines.push(Line::default());
    lines
}

pub fn proxy_lines(status: &Status, palette: &Palette) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        "[Proxy]",
        Style::default().fg(palette.md_heading),
    )];
    let details = status.lines();
    let routes = details
        .iter()
        .filter(|line| line.starts_with("HTTP:") || line.starts_with("HTTPS:"))
        .cloned()
        .collect::<Vec<_>>();
    if !routes.is_empty() {
        lines.push(Line::styled(
            format!("  {}", routes.join(" · ")),
            Style::default().fg(palette.muted),
        ));
    }
    lines.extend(
        details
            .into_iter()
            .filter(|line| {
                line.starts_with("NO_PROXY:")
                    || line.starts_with("no_proxy:")
                    || line.starts_with("REQUEST_METHOD")
                    || line.starts_with("Custom HTTP")
            })
            .map(|line| Line::styled(format!("  {line}"), Style::default().fg(palette.dim))),
    );
    lines
}

pub fn resource_lines(
    resources: &Resources,
    status: &Status,
    palette: &Palette,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !resources.instructions.is_empty() {
        lines.push(Line::styled(
            "[Context]",
            Style::default().fg(palette.md_heading),
        ));
        lines.extend(resources.instructions.iter().map(|instruction| {
            Line::styled(
                format!(
                    "  {}",
                    terminal_text(&instruction.path.to_string_lossy(), false)
                ),
                Style::default().fg(palette.muted),
            )
        }));
        lines.push(Line::default());
    }
    if !resources.skills.is_empty() {
        lines.push(Line::styled(
            "[Skills]",
            Style::default().fg(palette.md_heading),
        ));
        lines.push(Line::styled(
            format!(
                "  {}",
                resources
                    .skills
                    .iter()
                    .map(|skill| terminal_text(&skill.name, false))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Style::default().fg(palette.muted),
        ));
        lines.push(Line::default());
    }
    if !resources.warnings.is_empty() {
        lines.push(Line::styled(
            "[Warnings]",
            Style::default().fg(palette.warning),
        ));
        lines.extend(resources.warnings.iter().map(|warning| {
            Line::styled(
                format!("  {}", terminal_text(warning, false)),
                Style::default().fg(palette.warning),
            )
        }));
        lines.push(Line::default());
    }
    lines.extend(proxy_lines(status, palette));
    lines
}
