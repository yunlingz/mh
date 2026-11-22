//! Bounded text reads, with complete-file line counts and image attachments.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::output::{MAX_BYTES, MAX_LINES, Truncation, format_size};
use super::path::{io_code, resolve_read};
use crate::message::ToolResult;
use crate::text::{Utf8Decoder, clip_head};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    /// Relative or absolute file path.
    pub path: String,
    /// First line to read, starting at 1.
    pub offset: Option<f64>,
    /// Maximum number of selected lines.
    pub limit: Option<f64>,
}

pub fn execute(
    args: ReadArgs,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<ToolResult, String> {
    let path = resolve_read(&args.path, cwd);
    if cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }
    let file = crate::regular_file::open_read(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::InvalidInput {
            format!(
                "Could not read {}. Only regular files are supported.",
                args.path
            )
        } else {
            format!(
                "Could not read {}. Error code: {}.",
                args.path,
                io_code(&error)
            )
        }
    })?;
    let metadata = file.metadata().map_err(|error| io_code(&error))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let header = reader.fill_buf().map_err(|error| io_code(&error))?;
    if let Some(format) = detect_image(header) {
        return super::images::read_image(&path, format, cancel);
    }
    let start_number = args
        .offset
        .filter(|offset| *offset != 0.0)
        .map_or(0.0, |offset| (offset - 1.0).max(0.0));
    let size = if metadata.len() > 0 {
        metadata.len()
    } else {
        u64::MAX
    };
    let end_number = args.limit.map(|limit| start_number + limit);
    // JS slice truncates toward zero. Only a genuinely negative end index
    // needs the complete line count; fractions and -0.x do not need a first pass.
    let end = match end_number {
        Some(number) if number.trunc() < 0.0 => {
            let total = count_lines((&mut reader).take(size), cancel)?;
            reader
                .seek(SeekFrom::Start(0))
                .map_err(|error| io_code(&error))?;
            slice_index(number.min(total as f64), total)
        }
        Some(number) => number as usize,
        None => usize::MAX,
    };
    let mut selection = Selection::new(start_number as usize, end);
    let mut decoder = Utf8Decoder::default();
    let mut chunk = [0; 64 * 1024];
    let mut reader = reader.take(size);
    loop {
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }
        let count = reader.read(&mut chunk).map_err(|error| io_code(&error))?;
        if count == 0 {
            break;
        }
        selection.push(&decoder.push(&chunk[..count]));
    }
    selection.push(&decoder.finish());
    selection.finish_line();
    let total_file_lines = selection.line + 1;
    check_offset(&args, start_number, total_file_lines)?;
    Ok(format_result(
        &args,
        start_number,
        total_file_lines,
        selection.result(),
        selection.first_bytes,
    ))
}

fn count_lines(mut reader: impl Read, cancel: &CancellationToken) -> Result<usize, String> {
    let mut total = 1;
    let mut chunk = [0; 64 * 1024];
    loop {
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }
        let count = reader.read(&mut chunk).map_err(|error| io_code(&error))?;
        if count == 0 {
            return Ok(total);
        }
        total += chunk[..count].iter().filter(|byte| **byte == b'\n').count();
    }
}

fn detect_image(bytes: &[u8]) -> Option<image::ImageFormat> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) && bytes.get(3) != Some(&0xf7) {
        return Some(image::ImageFormat::Jpeg);
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
        && bytes.len() >= 16
        && read_u32_be(bytes, 8) == Some(13)
        && bytes.get(12..16) == Some(b"IHDR")
        && !animated_png(bytes)
    {
        return Some(image::ImageFormat::Png);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(image::ImageFormat::Gif);
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Some(image::ImageFormat::WebP);
    }
    if bytes.starts_with(b"BM") && valid_bmp(bytes) {
        return Some(image::ImageFormat::Bmp);
    }
    None
}

fn animated_png(bytes: &[u8]) -> bool {
    let mut offset = 8_usize;
    while offset.checked_add(8).is_some_and(|end| end <= bytes.len()) {
        let Some(length) = read_u32_be(bytes, offset).map(|value| value as usize) else {
            return false;
        };
        let kind = offset + 4;
        if bytes.get(kind..kind + 4) == Some(b"acTL") {
            return true;
        }
        if bytes.get(kind..kind + 4) == Some(b"IDAT") {
            return false;
        }
        let Some(next) = offset
            .checked_add(8)
            .and_then(|value| value.checked_add(length))
            .and_then(|value| value.checked_add(4))
        else {
            return false;
        };
        if next <= offset || next > bytes.len() {
            return false;
        }
        offset = next;
    }
    false
}

fn valid_bmp(bytes: &[u8]) -> bool {
    if bytes.len() < 30 {
        return false;
    }
    let Some(file_size) = read_u32_le(bytes, 2) else {
        return false;
    };
    let Some(pixel_offset) = read_u32_le(bytes, 10) else {
        return false;
    };
    let Some(header_size) = read_u32_le(bytes, 14) else {
        return false;
    };
    if (file_size != 0 && file_size < 26)
        || pixel_offset < 14_u32.saturating_add(header_size)
        || (file_size != 0 && pixel_offset >= file_size)
    {
        return false;
    }
    let (planes, bits) = if header_size == 12 {
        (read_u16_le(bytes, 22), read_u16_le(bytes, 24))
    } else if (40..=124).contains(&header_size) {
        (read_u16_le(bytes, 26), read_u16_le(bytes, 28))
    } else {
        return false;
    };
    planes == Some(1) && matches!(bits, Some(1 | 4 | 8 | 16 | 24 | 32))
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u32_be(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn slice_index(value: f64, length: usize) -> usize {
    let value = value.trunc();
    if value < 0.0 {
        (length as f64 + value).max(0.0) as usize
    } else {
        (value as usize).min(length)
    }
}

fn check_offset(args: &ReadArgs, start: f64, total: usize) -> Result<(), String> {
    if start >= total as f64 {
        Err(format!(
            "Offset {} is beyond end of file ({total} lines total)",
            args.offset.unwrap_or(1.0)
        ))
    } else {
        Ok(())
    }
}

fn format_result(
    args: &ReadArgs,
    start: f64,
    total: usize,
    truncation: Truncation,
    first_bytes: usize,
) -> ToolResult {
    let mut text = truncation.content.clone();
    let mut details = serde_json::Value::Null;
    if truncation.first_line_exceeds_limit {
        text = format!(
            "[Line {} is {}, exceeds {} limit. Use bash to inspect part of this line.]",
            start + 1.0,
            format_size(first_bytes),
            format_size(MAX_BYTES)
        );
        details = json!({"truncation":truncation});
    } else if truncation.truncated {
        let end = start + truncation.output_lines as f64;
        text.push_str(&format!(
            "\n\n[Showing lines {}-{end} of {total}. Use offset={} to continue.]",
            start + 1.0,
            end + 1.0
        ));
        details = json!({"truncation":truncation});
    } else if let Some(limit) = args.limit {
        let end = if (start + limit).trunc() < 0.0 {
            slice_index(start + limit, total) as f64
        } else {
            (start + limit).min(total as f64).max(start)
        };
        if end < total as f64 {
            text.push_str(&format!(
                "\n\n[{} more lines in file. Use offset={} to continue.]",
                total as f64 - end,
                end + 1.0
            ));
        }
    }
    ToolResult {
        details,
        ..ToolResult::text(text)
    }
}

struct Selection {
    start: usize,
    end: usize,
    line: usize,
    current: String,
    current_bytes: usize,
    lines: Vec<String>,
    stored_bytes: usize,
    selected_bytes: usize,
    selected_lines: usize,
    last_empty: bool,
    first_bytes: usize,
    closed: bool,
}

impl Selection {
    fn new(start: usize, end: usize) -> Self {
        Self {
            start,
            end,
            line: 0,
            current: String::new(),
            current_bytes: 0,
            lines: Vec::new(),
            stored_bytes: 0,
            selected_bytes: 0,
            selected_lines: 0,
            last_empty: false,
            first_bytes: 0,
            closed: false,
        }
    }

    fn push(&mut self, text: &str) {
        for piece in text.split_inclusive('\n') {
            let terminated = piece.ends_with('\n');
            let text = piece.strip_suffix('\n').unwrap_or(piece);
            if self.line >= self.start && self.line < self.end {
                self.current_bytes += text.len();
                if !self.closed {
                    let remaining = MAX_BYTES.saturating_sub(self.current.len());
                    self.current.push_str(clip_head(text, remaining));
                }
            }
            if terminated {
                self.finish_line();
                self.line += 1;
            }
        }
    }

    fn finish_line(&mut self) {
        if self.line >= self.start && self.line < self.end {
            let separator = usize::from(self.selected_lines > 0);
            if self.selected_lines == 0 {
                self.first_bytes = self.current_bytes;
            }
            self.selected_bytes += self.current_bytes + separator;
            self.selected_lines += 1;
            self.last_empty = self.current_bytes == 0;
            if !self.closed
                && self.lines.len() <= MAX_LINES
                && self.stored_bytes + self.current_bytes + separator <= MAX_BYTES
            {
                self.stored_bytes += self.current_bytes + separator;
                self.lines.push(std::mem::take(&mut self.current));
            } else {
                self.closed = true;
            }
        }
        self.current.clear();
        self.current_bytes = 0;
    }

    fn result(&self) -> Truncation {
        let total_lines = self
            .selected_lines
            .saturating_sub(usize::from(self.last_empty));
        let truncated = total_lines > MAX_LINES || self.selected_bytes > MAX_BYTES;
        let first_line_exceeds_limit = self.first_bytes > MAX_BYTES;
        let shown = if truncated {
            self.lines.len().min(MAX_LINES)
        } else {
            self.lines.len()
        };
        let content = self.lines[..shown].join("\n");
        Truncation {
            output_bytes: content.len(),
            content,
            truncated,
            truncated_by: truncated.then_some(if self.lines.len() >= MAX_LINES {
                "lines"
            } else {
                "bytes"
            }),
            total_lines,
            total_bytes: self.selected_bytes,
            output_lines: if truncated { shown } else { total_lines },
            first_line_exceeds_limit,
            last_line_partial: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_range_counting_checks_cancellation_between_bounded_chunks() {
        struct CancellingReader {
            cancel: CancellationToken,
            calls: usize,
        }
        impl Read for CancellingReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                assert!(buffer.len() <= 64 * 1024);
                self.calls += 1;
                buffer.fill(b'\n');
                self.cancel.cancel();
                Ok(buffer.len())
            }
        }
        let cancel = CancellationToken::new();
        let mut reader = CancellingReader {
            cancel: cancel.clone(),
            calls: 0,
        };
        assert_eq!(
            count_lines(&mut reader, &cancel).unwrap_err(),
            "Operation aborted"
        );
        assert_eq!(reader.calls, 1);
    }

    #[test]
    fn selection_keeps_only_bounded_head_storage_while_counting_a_large_line() {
        let mut selection = Selection::new(0, usize::MAX);
        let chunk = "🙂".repeat(16 * 1024);
        for _ in 0..256 {
            selection.push(&chunk);
            assert!(selection.current.len() <= MAX_BYTES);
            assert!(selection.stored_bytes <= MAX_BYTES);
        }
        selection.push("\ntail\n");
        selection.finish_line();
        let result = selection.result();
        assert!(result.first_line_exceeds_limit);
        assert_eq!(selection.first_bytes, 16 * 1024 * 1024);
        assert_eq!(selection.line + 1, 3);
        assert!(result.content.is_empty());
    }
}
