//! Streaming UTF-8 decoding and terminal-safe display text.

#[derive(Default)]
pub struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let data = std::mem::take(&mut self.pending);
        let mut rest = data.as_slice();
        let mut output = String::new();
        while !rest.is_empty() {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    output.push_str(text);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    output.push_str(
                        std::str::from_utf8(&rest[..valid]).expect("validated UTF-8 prefix"),
                    );
                    rest = &rest[valid..];
                    if let Some(length) = error.error_len() {
                        output.push('\u{fffd}');
                        rest = &rest[length..];
                    } else {
                        self.pending.extend_from_slice(rest);
                        break;
                    }
                }
            }
        }
        output
    }

    pub fn finish(&mut self) -> String {
        let data = std::mem::take(&mut self.pending);
        String::from_utf8_lossy(&data).into_owned()
    }
}

pub fn clip_head(text: &str, bytes: usize) -> &str {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub fn clip_tail(text: &str, bytes: usize) -> &str {
    let mut start = text.len().saturating_sub(bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

pub fn display_path(path: &std::path::Path) -> String {
    let value = path.to_string_lossy();
    #[cfg(windows)]
    {
        windows_path_label(&value)
    }
    #[cfg(not(windows))]
    {
        value.into_owned()
    }
}

#[cfg(any(windows, test))]
fn windows_path_label(value: &str) -> String {
    if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
        format!("//{}", unc.replace('\\', "/"))
    } else {
        value
            .strip_prefix(r"\\?\")
            .unwrap_or(value)
            .replace('\\', "/")
    }
}

pub fn terminal_text(value: &str, multiline: bool) -> String {
    let mut chars = value.chars().peekable();
    let mut output = String::new();
    while let Some(ch) = chars.next() {
        let introducer = match ch {
            '\u{1b}' => chars.next(),
            '\u{9b}' => Some('['),
            '\u{9d}' => Some(']'),
            _ => None,
        };
        if let Some(introducer) = introducer {
            if introducer == '[' {
                for ch in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&ch) {
                        break;
                    }
                }
            } else if introducer == ']' {
                while let Some(ch) = chars.next() {
                    if matches!(ch, '\u{7}' | '\u{9c}') {
                        break;
                    }
                    if ch == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            continue;
        }
        if matches!(ch, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            continue;
        }
        if matches!(ch, '\n' | '\u{2028}' | '\u{2029}') {
            output.push(if multiline { '\n' } else { ' ' });
        } else if ch == '\t' {
            output.push_str(if multiline { "    " } else { " " });
        } else if !ch.is_control() {
            output.push(ch);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    #[test]
    fn windows_display_paths_remove_verbatim_prefixes_without_losing_unc_roots() {
        assert_eq!(
            super::windows_path_label(r"\\?\C:\Users\test\project"),
            "C:/Users/test/project"
        );
        assert_eq!(
            super::windows_path_label(r"\\?\UNC\server\share\project"),
            "//server/share/project"
        );
        assert_eq!(
            super::windows_path_label(r"C:\work\file.rs"),
            "C:/work/file.rs"
        );
    }
}
