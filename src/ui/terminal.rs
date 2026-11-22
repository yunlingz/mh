//! Terminal mode transitions with rollback after partially failed entry.

use std::io::{self, Write};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, SetTitle};

/// The title contains only mh-owned activity labels, never prompts or paths.
#[derive(Default)]
pub struct Title {
    last: String,
}

impl Title {
    pub fn update(&mut self, writer: &mut impl Write, activity: &str) -> io::Result<()> {
        let safe = crate::text::terminal_text(activity, false);
        let safe = crate::ui::footer::ellipsize(&safe, 60);
        let title = format!("🍀 mh · {safe}");
        if title != self.last {
            execute!(writer, SetTitle(&title))?;
            self.last = title;
        }
        Ok(())
    }
}

// Xterm-compatible title stack: ignored by terminals that do not support it.
// Unsupported stack restoration does not alter input/cursor/screen modes.
const SAVE_TITLE: &[u8] = b"\x1b[22;0t";
const RESTORE_TITLE: &[u8] = b"\x1b[23;0t";

pub fn enter_screen(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(SAVE_TITLE)?;
    if let Err(error) = execute!(
        writer,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        Hide
    ) {
        let _ = leave_screen(writer);
        return Err(error);
    }
    Ok(())
}

pub fn leave_screen(writer: &mut impl Write) -> io::Result<()> {
    // Mouse capture uses a Windows API fallback and can fail if startup stopped
    // before it was enabled. A failed step must not skip paste/screen restoration.
    let modes = [
        execute!(writer, Show),
        execute!(writer, DisableMouseCapture),
        execute!(writer, DisableBracketedPaste),
        execute!(writer, LeaveAlternateScreen),
    ];
    let title = writer
        .write_all(RESTORE_TITLE)
        .and_then(|()| writer.flush());
    match modes.into_iter().find_map(Result::err) {
        Some(error) => Err(error),
        None => title,
    }
}

pub fn cursor_cell(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
) -> Option<(u16, u16)> {
    let buffer = frame.buffer_mut();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if buffer[(x, y)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
            {
                return Some((x, y));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FailOnce {
        output: Vec<u8>,
        writes: usize,
        fail_after: Option<usize>,
    }

    impl Write for FailOnce {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if self.fail_after == Some(self.writes) {
                self.fail_after = None;
                return Err(io::Error::other("controlled terminal write failure"));
            }
            self.output.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn partial_entry_rolls_back_all_terminal_modes() {
        for fail_after in 2..=3 {
            let mut writer = FailOnce {
                fail_after: Some(fail_after),
                ..FailOnce::default()
            };
            assert!(enter_screen(&mut writer).is_err());
            let output = String::from_utf8(writer.output).unwrap();
            assert!(
                output.contains("\u{1b}[?25h"),
                "cursor not restored: {output:?}"
            );
            assert!(
                output.contains("\u{1b}[?2004l"),
                "paste not restored: {output:?}"
            );
            assert!(
                output.contains("\u{1b}[?1049l"),
                "screen not restored: {output:?}"
            );
        }
    }

    #[test]
    fn cleanup_continues_after_a_restore_write_failure() {
        let mut writer = FailOnce {
            fail_after: Some(1),
            ..FailOnce::default()
        };
        assert!(leave_screen(&mut writer).is_err());
        let output = String::from_utf8(writer.output).unwrap();
        assert!(output.contains("\x1b[?2004l"));
        assert!(output.contains("\x1b[?1049l"));
        assert!(output.ends_with("\x1b[23;0t"));
    }

    #[test]
    fn titles_are_bounded_sanitized_and_written_only_on_change() {
        let mut title = Title::default();
        let mut output = Vec::new();
        enter_screen(&mut output).unwrap();
        title.update(&mut output, "Thinking\x1b]0;bad\x07").unwrap();
        let length = output.len();
        title.update(&mut output, "Thinking\x1b]0;bad\x07").unwrap();
        assert_eq!(output.len(), length);
        title.update(&mut output, &"x".repeat(500)).unwrap();
        leave_screen(&mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("🍀 mh"));
        assert!(output.starts_with("\x1b[22;0t"));
        assert!(output.ends_with("\x1b[23;0t"));
        assert!(!output.contains("Thinking\x1b]0;bad"));
        assert!(!output.contains(&"x".repeat(100)));
    }

    #[test]
    fn normal_entry_and_exit_use_paired_modes() {
        let mut writer = Vec::new();
        enter_screen(&mut writer).unwrap();
        leave_screen(&mut writer).unwrap();
        let output = String::from_utf8(writer).unwrap();
        for control in ["?1049h", "?2004h", "?25l", "?25h", "?2004l", "?1049l"] {
            assert!(output.contains(control));
        }
    }
}
