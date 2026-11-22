//! Width-aware status rows. Billing totals and current context stay separate.

use std::path::Path;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::config::Config;
use crate::session::Snapshot;
use crate::text::terminal_text;
use crate::theme::Palette;

pub fn lines(
    snapshot: &Snapshot,
    config: &Config,
    cwd: &Path,
    palette: &Palette,
    width: u16,
) -> Vec<Line<'static>> {
    let Some(choice) = &snapshot.choice else {
        return Vec::new();
    };
    let model = config
        .model(&choice.qualified)
        .ok()
        .map(|(_, _, model)| model);
    let model_label = model.map_or_else(
        || choice.qualified.clone(),
        |model| {
            if model.reasoning {
                format!("{} ({})", choice.qualified, model.effort_label())
            } else {
                choice.qualified.clone()
            }
        },
    );
    let context = model
        .map(|model| snapshot.estimate.tokens() as f64 / model.context_window as f64 * 100.0)
        .unwrap_or(0.0)
        .clamp(0.0, 999.9);
    let approximate = if snapshot.estimate.approximate() {
        "~"
    } else {
        ""
    };
    let home = crate::storage::Layout::user()
        .ok()
        .and_then(|layout| layout.root.parent().map(crate::text::display_path));
    let cwd_label = crate::text::display_path(cwd);
    let cwd_display = Path::new(&cwd_label);
    let path = home
        .as_deref()
        .and_then(|home| cwd_display.strip_prefix(Path::new(home)).ok())
        .map_or_else(
            || cwd_label.clone(),
            |suffix| {
                if suffix.as_os_str().is_empty() {
                    "~".into()
                } else {
                    format!("~/{}", suffix.to_string_lossy())
                }
            },
        );
    let mut rows = wrap_segments(
        vec![
            segment("model", &model_label, palette.md_heading, palette),
            segment("path", &terminal_text(&path, false), palette.muted, palette),
            segment(
                "context",
                &model
                    .map(|model| {
                        format!(
                            "{approximate}{}/{} ({context:.1}%)",
                            tokens(snapshot.estimate.tokens()),
                            tokens(model.context_window)
                        )
                    })
                    .unwrap_or_else(|| "unavailable".into()),
                if context >= 80.0 {
                    palette.warning
                } else {
                    palette.muted
                },
                palette,
            ),
        ],
        width,
        palette,
    );
    let usage = &snapshot.usage;
    let cache = usage
        .cache_percent()
        .map_or("—".into(), |value| format!("{value:.1}%"));
    rows.extend(wrap_segments(
        vec![
            segment("in", &tokens(usage.input), palette.muted, palette),
            segment("cr", &tokens(usage.cache_read), palette.muted, palette),
            segment("cw", &tokens(usage.cache_write), palette.muted, palette),
            segment("out", &tokens(usage.output), palette.muted, palette),
            segment("cache", &cache, palette.accent, palette),
        ],
        width,
        palette,
    ));
    rows
}

fn segment(label: &str, value: &str, color: Color, palette: &Palette) -> Vec<Span<'static>> {
    vec![
        Span::styled(format!("{label} "), Style::default().fg(palette.dim)),
        Span::styled(terminal_text(value, false), Style::default().fg(color)),
    ]
}

fn wrap_segments(
    segments: Vec<Vec<Span<'static>>>,
    width: u16,
    palette: &Palette,
) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let width = usize::from(width);
    let mut rows = Vec::new();
    let mut current = Vec::new();
    let mut used = 0;
    for spans in segments {
        let length = spans.iter().map(Span::width).sum::<usize>();
        if !current.is_empty() && used + 3 + length.min(width) > width {
            rows.push(Line::from(std::mem::take(&mut current)));
            used = 0;
        }
        if !current.is_empty() {
            current.push(Span::styled(" · ", Style::default().fg(palette.dim)));
            used += 3;
        }
        for span in spans {
            for grapheme in span.content.graphemes(true) {
                let text = ellipsize(grapheme, width);
                let rendered = UnicodeWidthStr::width(text.as_str());
                if used + rendered > width && !current.is_empty() {
                    rows.push(Line::from(std::mem::take(&mut current)));
                    used = 0;
                }
                current.push(Span::styled(text, span.style));
                used += rendered;
            }
        }
    }
    if !current.is_empty() {
        rows.push(Line::from(current));
    }
    rows
}

pub fn ellipsize(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let length = UnicodeWidthStr::width(grapheme);
        if used + length > width - 1 {
            break;
        }
        used += length;
        out.push_str(grapheme);
    }
    out.push('…');
    out
}

pub(crate) fn tokens(value: u64) -> String {
    match value {
        0..=999 => value.to_string(),
        1_000..=999_999 => format!("{:.1}k", value as f64 / 1_000.0),
        _ => format!("{:.1}M", value as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_preserves_graphemes_and_cell_width() {
        for text in [
            "long/path/file.rs",
            "你好🙂hello",
            "e\u{301} combining",
            "👩‍💻 coding",
        ] {
            for width in 0..20 {
                assert!(UnicodeWidthStr::width(ellipsize(text, width).as_str()) <= width);
            }
        }
    }

    #[test]
    fn segments_wrap_without_changing_order_or_overflowing() {
        let palette = Palette::default();
        for width in [1, 20, 40, 80, 120] {
            let rows = wrap_segments(
                vec![
                    segment("model", "test/long-model-name", palette.text, &palette),
                    segment("thinking", "high", palette.text, &palette),
                    segment("path", "/你好/long/project", palette.text, &palette),
                    segment("context", "~12.3%", palette.text, &palette),
                ],
                width,
                &palette,
            );
            assert!(rows.iter().all(|row| row.width() <= usize::from(width)));
            if width >= 40 {
                let text = rows
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.find("model").unwrap() < text.find("thinking").unwrap());
                assert!(text.find("thinking").unwrap() < text.find("path").unwrap());
                assert!(text.find("path").unwrap() < text.find("context").unwrap());
            }
        }
    }
}
