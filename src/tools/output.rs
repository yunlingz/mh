//! Pi's head/tail limits and a bounded streaming shell-output accumulator.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::storage::new_private_file;
use crate::text::{Utf8Decoder, clip_tail};

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Truncation {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<&'static str>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub first_line_exceeds_limit: bool,
    pub last_line_partial: bool,
}

fn lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<_> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

pub fn truncate_head(text: &str) -> Truncation {
    truncate(text, false)
}

pub fn truncate_tail(text: &str) -> Truncation {
    truncate(text, true)
}

fn truncate(text: &str, tail: bool) -> Truncation {
    let all = lines(text);
    let mut result = Truncation {
        content: text.to_owned(),
        truncated: all.len() > MAX_LINES || text.len() > MAX_BYTES,
        truncated_by: None,
        total_lines: all.len(),
        total_bytes: text.len(),
        output_lines: all.len(),
        output_bytes: text.len(),
        first_line_exceeds_limit: false,
        last_line_partial: false,
    };
    if !result.truncated {
        return result;
    }
    let mut output = Vec::new();
    let mut bytes = 0;
    result.truncated_by = Some("lines");
    for index in 0..all.len().min(MAX_LINES) {
        let line = if tail {
            all[all.len() - 1 - index]
        } else {
            all[index]
        };
        let separator = usize::from(!output.is_empty());
        if bytes + line.len() + separator > MAX_BYTES {
            result.truncated_by = Some("bytes");
            if tail && (output.is_empty() || output.iter().all(|s: &&str| s.trim().is_empty())) {
                let remaining = MAX_BYTES.saturating_sub(bytes + separator);
                if remaining > 0 {
                    output.push(clip_tail(line, remaining));
                    result.last_line_partial = true;
                }
            } else if !tail && output.is_empty() {
                result.first_line_exceeds_limit = true;
            }
            break;
        }
        output.push(line);
        bytes += line.len() + separator;
    }
    if tail {
        output.reverse();
    }
    result.content = output.join("\n");
    result.output_bytes = result.content.len();
    result.output_lines = output.len();
    result
}

pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub struct OutputAccumulator {
    decoder: Utf8Decoder,
    raw: Vec<u8>,
    tail: String,
    boundary: bool,
    total_bytes: usize,
    completed_lines: usize,
    line_bytes: usize,
    path: PathBuf,
    file: Option<tokio::fs::File>,
}

impl OutputAccumulator {
    pub fn new(path: PathBuf) -> Self {
        Self {
            decoder: Utf8Decoder::default(),
            raw: Vec::new(),
            tail: String::new(),
            boundary: true,
            total_bytes: 0,
            completed_lines: 0,
            line_bytes: 0,
            path,
            file: None,
        }
    }

    fn decoded(&mut self, text: &str) {
        self.total_bytes += text.len();
        self.completed_lines += text.bytes().filter(|c| *c == b'\n').count();
        if let Some(last) = text.rfind('\n') {
            self.line_bytes = text.len() - last - 1;
        } else {
            self.line_bytes += text.len();
        }
        self.tail.push_str(text);
        if self.tail.len() > MAX_BYTES * 4 {
            let tail = clip_tail(&self.tail, MAX_BYTES * 2);
            let start = self.tail.len() - tail.len();
            self.boundary = self.tail.as_bytes()[start - 1] == b'\n';
            self.tail = tail.to_owned();
        }
    }

    fn total_lines(&self) -> usize {
        self.completed_lines + usize::from(self.line_bytes > 0)
    }

    fn overflow(&self) -> bool {
        self.total_bytes > MAX_BYTES || self.total_lines() > MAX_LINES
    }

    async fn persist(&mut self) -> std::io::Result<()> {
        if self.file.is_none() {
            let mut file = tokio::fs::File::from_std(new_private_file(&self.path)?);
            file.write_all(&self.raw).await?;
            self.raw.clear();
            self.file = Some(file);
        }
        Ok(())
    }

    pub async fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let text = self.decoder.push(bytes);
        self.decoded(&text);
        if self.file.is_some() || self.overflow() {
            self.persist().await?;
            self.file
                .as_mut()
                .expect("persisted output")
                .write_all(bytes)
                .await?;
        } else {
            self.raw.extend_from_slice(bytes);
        }
        Ok(())
    }

    pub async fn finish(&mut self) -> std::io::Result<()> {
        let pending = self.decoder.finish();
        self.decoded(&pending);
        if self.overflow() {
            self.persist().await?;
        }
        if let Some(file) = &mut self.file {
            file.flush().await?;
            file.sync_all().await?;
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Truncation {
        let clipped = if self.boundary {
            self.tail.as_str()
        } else {
            self.tail.split_once('\n').map_or("", |(_, rest)| rest)
        };
        let text = if clipped.trim().is_empty() {
            self.tail.as_str()
        } else {
            clipped
        };
        let mut result = truncate_tail(text);
        result.total_lines = self.total_lines();
        result.total_bytes = self.total_bytes;
        result.truncated = self.overflow();
        if result.truncated && result.truncated_by.is_none() {
            result.truncated_by = Some(if self.total_bytes > MAX_BYTES {
                "bytes"
            } else {
                "lines"
            });
        }
        result
    }

    pub fn path(&self) -> Option<&Path> {
        self.file.as_ref().map(|_| self.path.as_path())
    }

    pub fn formatted(&self) -> String {
        let result = self.snapshot();
        let mut text = if result.content.is_empty() {
            "(no output)".into()
        } else {
            result.content
        };
        if result.truncated {
            if result.last_line_partial {
                text.push_str(&format!(
                    "\n\n[Showing last {} of line {} (line is {}). Full output: {}]",
                    format_size(MAX_BYTES),
                    result.total_lines,
                    format_size(self.line_bytes),
                    self.path.display()
                ));
            } else {
                let start = result.total_lines.saturating_sub(result.output_lines) + 1;
                let limit = if result.truncated_by == Some("bytes") {
                    format!(" ({} limit)", format_size(MAX_BYTES))
                } else {
                    String::new()
                };
                text.push_str(&format!(
                    "\n\n[Showing lines {start}-{} of {}{limit}. Full output: {}]",
                    result.total_lines,
                    result.total_lines,
                    self.path.display()
                ));
            }
        }
        text
    }

    pub async fn structured_output(&self) -> std::io::Result<(String, bool)> {
        const LIMIT: usize = 1024 * 1024;
        if self.file.is_none() {
            return Ok((String::from_utf8_lossy(&self.raw).into_owned(), false));
        }
        let mut file = tokio::fs::File::open(&self.path).await?;
        let size = file.metadata().await?.len() as usize;
        if size <= LIMIT {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).await?;
            return Ok((String::from_utf8_lossy(&bytes).into_owned(), false));
        }
        let mut head = vec![0; LIMIT / 2];
        let mut tail = vec![0; LIMIT / 2];
        file.read_exact(&mut head).await?;
        file.seek(std::io::SeekFrom::End(-(LIMIT as i64 / 2)))
            .await?;
        file.read_exact(&mut tail).await?;
        let mut decoder = Utf8Decoder::default();
        let head = decoder.push(&head);
        let start = tail
            .iter()
            .position(|c| c & 0xc0 != 0x80)
            .unwrap_or(tail.len());
        Ok((
            format!(
                "{head}\n\n[... {} bytes omitted ...]\n\n{}",
                size - LIMIT,
                String::from_utf8_lossy(&tail[start..])
            ),
            true,
        ))
    }
}
