//! Disjoint replacements against one original file, with Pi's fuzzy matching rules.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use tokio_util::sync::CancellationToken;
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

use crate::message::ToolResult;

use super::output::{MAX_BYTES, MAX_LINES};
use super::path::{io_code, resolve};
use crate::text::{clip_head, clip_tail};

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Edit {
    /// Unique original text to replace. Regions must not overlap.
    pub old_text: String,
    /// Replacement text.
    pub new_text: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditArgs {
    /// Relative or absolute file path.
    pub path: String,
    /// Targeted replacements, all matched against the original file.
    #[schemars(length(min = 1))]
    pub edits: Vec<Edit>,
}

pub fn prepare(mut args: Value) -> Value {
    let Some(object) = args.as_object_mut() else {
        return args;
    };
    let edits = match object.get("edits") {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text).ok(),
        Some(value @ Value::Object(_)) => Some(value.clone()),
        _ => None,
    };
    if let Some(value) = edits {
        if value.is_array() {
            object.insert("edits".into(), value);
        } else if single_edit(&value) {
            object.insert("edits".into(), json!([value]));
        }
    }
    let old = object
        .get("oldText")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let new = object
        .get("newText")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let (Some(old), Some(new)) = (old, new) {
        object.remove("oldText");
        object.remove("newText");
        let edit = json!({"oldText":old, "newText":new});
        if let Some(Value::Array(edits)) = object.get_mut("edits") {
            edits.push(edit);
        } else {
            object.insert("edits".into(), json!([edit]));
        }
    }
    args
}

fn single_edit(value: &Value) -> bool {
    value.get("oldText").is_some_and(Value::is_string)
        && value.get("newText").is_some_and(Value::is_string)
}

pub fn execute(
    args: EditArgs,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<ToolResult, String> {
    if cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }
    if args.edits.is_empty() {
        return Err(
            "Edit tool input is invalid. edits must contain at least one replacement.".into(),
        );
    }
    let path = resolve(&args.path, cwd);
    let access_error = |error: std::io::Error| {
        format!(
            "Could not edit file: {}. Error code: {}.",
            args.path,
            io_code(&error)
        )
    };
    let mut file = crate::regular_file::open_edit(&path).map_err(access_error)?;
    let metadata = file.metadata().map_err(access_error)?;
    if metadata.permissions().readonly() {
        return Err(format!(
            "Could not edit file: {}. Error code: EACCES.",
            args.path
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(access_error)?;
    let raw = std::str::from_utf8(&bytes)
        .map_err(|_| format!("Could not edit {}: file is not valid UTF-8.", args.path))?;
    let (bom, original) = raw
        .strip_prefix('\u{feff}')
        .map_or(("", raw), |text| ("\u{feff}", text));
    let base = normalize_lf(original);
    let replacements = match_edits(&base, &args.edits, &args.path)?;
    let updated = apply(&base, &replacements);
    if base == updated {
        return Err(format!(
            "No changes made to {}. The replacements produced identical content.",
            args.path
        ));
    }
    // Finish all matching and diff work before touching the destination.
    let (diff, mut patch, first_changed_line) =
        make_diff(&args.path, &base, &updated, &replacements);
    if !bom.is_empty() || original != base {
        let warning = "[Normalized LF/BOM-free patch preview; not directly applicable to the original file bytes.]\n";
        patch = format!("{warning}{}", clip_head(&patch, MAX_BYTES - warning.len()));
    }
    if cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }
    let content = if original == base {
        updated
    } else {
        apply_to_original(original, &replacements)
    };
    if cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }
    // Keep the verified handle: reopening the path could hit a replaced FIFO
    // or overwrite an unrelated replacement file after matching has finished.
    file.seek(SeekFrom::Start(0)).map_err(access_error)?;
    file.set_len(0).map_err(access_error)?;
    file.write_all(bom.as_bytes()).map_err(access_error)?;
    file.write_all(content.as_bytes()).map_err(access_error)?;
    if cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }
    Ok(ToolResult {
        details: json!({"diff":diff, "patch":patch, "firstChangedLine":first_changed_line}),
        ..ToolResult::text(format!(
            "Successfully replaced {} block(s) in {}.",
            args.edits.len(),
            args.path
        ))
    })
}

fn normalize_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn fuzzy(text: &str) -> String {
    let normalized: String = text.nfkc().collect();
    normalized
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .map(fuzzy_character)
        .collect()
}

fn fuzzy_character(ch: char) -> char {
    match ch {
        '\u{2018}'..='\u{201b}' => '\'',
        '\u{201c}'..='\u{201f}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
        ch => ch,
    }
}

#[derive(Clone)]
struct Replacement {
    start: usize,
    length: usize,
    text: String,
    edit_index: usize,
}

fn match_edits(base: &str, edits: &[Edit], path: &str) -> Result<Vec<Replacement>, String> {
    let old: Vec<_> = edits
        .iter()
        .map(|edit| normalize_lf(&edit.old_text))
        .collect();
    if old.iter().any(String::is_empty) {
        return Err(format!("oldText must not be empty in {path}."));
    }
    let counted_base = fuzzy(base);
    let mut replacements = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        let normalized = fuzzy(&old[index]);
        if normalized.is_empty() {
            return Err(format!(
                "oldText must contain non-whitespace text in {path}."
            ));
        }
        let (start, length) = if let Some(start) = base.find(&old[index]) {
            (start, old[index].len())
        } else {
            let Some(start) = counted_base.find(&normalized) else {
                return Err(format!(
                    "Could not find edits[{index}] in {path}. The oldText must match including whitespace and newlines."
                ));
            };
            let end = start + normalized.len();
            let Some((raw_start, raw_end)) = fuzzy_range(base, start, end) else {
                return Err(
                    "Fuzzy replacement splits a Unicode grapheme; use the original text".into(),
                );
            };
            (raw_start, raw_end - raw_start)
        };
        let occurrences = counted_base
            .as_bytes()
            .windows(normalized.len())
            .filter(|window| *window == normalized.as_bytes())
            .count();
        if occurrences > 1 {
            return Err(format!(
                "Found {occurrences} occurrences of edits[{index}] in {path}. Each oldText must be unique."
            ));
        }
        replacements.push(Replacement {
            start,
            length,
            text: normalize_lf(&edit.new_text),
            edit_index: index,
        });
    }
    replacements.sort_by_key(|replacement| replacement.start);
    for pair in replacements.windows(2) {
        if pair[0].start + pair[0].length > pair[1].start {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them or target disjoint regions.",
                pair[0].edit_index, pair[1].edit_index
            ));
        }
    }
    Ok(replacements)
}

/// Map normalized grapheme boundaries back to the untouched source. A boundary
/// can skip trimmed whitespace: its start points after the gap, its end before
/// the gap, so a substring never rewrites unrelated literals on the same line.
fn fuzzy_range(text: &str, target_start: usize, target_end: usize) -> Option<(usize, usize)> {
    let mut offset = 0;
    let mut raw_start = None;
    let mut line_start = 0;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line).trim_end();
        for (index, grapheme) in body.grapheme_indices(true) {
            let start = line_start + index;
            if offset == target_start {
                raw_start = Some(start);
            }
            offset += grapheme
                .nfkc()
                .map(fuzzy_character)
                .map(char::len_utf8)
                .sum::<usize>();
            if offset == target_end {
                return raw_start.map(|start| (start, line_start + index + grapheme.len()));
            }
            if offset > target_end {
                return None;
            }
        }
        if line.ends_with('\n') {
            let newline = line_start + line.len() - 1;
            if offset == target_start {
                raw_start = Some(newline);
            }
            offset += 1;
            if offset == target_end {
                return raw_start.map(|start| (start, newline + 1));
            }
        }
        line_start += line.len();
    }
    None
}

fn apply(base: &str, replacements: &[Replacement]) -> String {
    let mut result = base.to_owned();
    for replacement in replacements.iter().rev() {
        result.replace_range(
            replacement.start..replacement.start + replacement.length,
            &replacement.text,
        );
    }
    result
}

fn apply_to_original(original: &str, replacements: &[Replacement]) -> String {
    let bytes = original.as_bytes();
    let mut normalized = 0;
    let mut raw = 0;
    let mut to_raw = |target: usize| {
        while normalized < target {
            raw += if bytes[raw] == b'\r' && bytes.get(raw + 1) == Some(&b'\n') {
                2
            } else {
                1
            };
            normalized += 1;
        }
        raw
    };
    let mut result = String::new();
    let mut position = 0;
    for replacement in replacements {
        let start = to_raw(replacement.start);
        push_seam(&mut result, &original[position..start]);
        let newline = original[start..]
            .find('\n')
            .map(|index| index + start)
            .or_else(|| original[..start].rfind('\n'));
        let ending = match newline {
            Some(index) if index > 0 && bytes[index - 1] == b'\r' => "\r\n",
            Some(_) => "\n",
            None if original.contains('\r') => "\r",
            None => "\n",
        };
        push_seam(&mut result, &replacement.text.replace('\n', ending));
        position = to_raw(replacement.start + replacement.length);
    }
    push_seam(&mut result, &original[position..]);
    result
}

fn push_seam(output: &mut String, part: &str) {
    // A kept lone CR must not merge with an inserted LF into one line break.
    if output.ends_with('\r') && part.starts_with('\n') {
        output.push('\n');
    }
    output.push_str(part);
}

fn make_diff(
    path: &str,
    old: &str,
    new: &str,
    replacements: &[Replacement],
) -> (String, String, Option<usize>) {
    #[derive(Clone, Copy)]
    struct Window {
        old_start: usize,
        old_end: usize,
        new_start: usize,
        new_end: usize,
    }

    let mut windows: Vec<Window> = Vec::new();
    let mut delta = 0_i64;
    for replacement in replacements {
        let new_start = (replacement.start as i64 + delta) as usize;
        let new_end = new_start + replacement.text.len();
        delta += replacement.text.len() as i64 - replacement.length as i64;
        let window = Window {
            old_start: context_start(old, replacement.start, 4),
            old_end: context_end(old, replacement.start + replacement.length, 4),
            new_start: context_start(new, new_start, 4),
            new_end: context_end(new, new_end, 4),
        };
        if let Some(previous) = windows.last_mut()
            && (window.old_start <= previous.old_end || window.new_start <= previous.new_end)
        {
            previous.old_end = previous.old_end.max(window.old_end);
            previous.new_end = previous.new_end.max(window.new_end);
        } else {
            windows.push(window);
        }
    }

    if windows.iter().any(|window| {
        let before = &old[window.old_start..window.old_end];
        let after = &new[window.new_start..window.new_end];
        before.len() + after.len() > MAX_BYTES
            || before.bytes().filter(|byte| *byte == b'\n').count()
                + after.bytes().filter(|byte| *byte == b'\n').count()
                > MAX_LINES
    }) {
        return diff_excerpts(old, new, replacements);
    }

    let width = old
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        .max(new.bytes().filter(|byte| *byte == b'\n').count())
        .saturating_add(1)
        .to_string()
        .len();
    let mut display = BoundedDiff::default();
    let mut patch = BoundedDiff::default();
    patch.push(&format!("--- {path}\n+++ {path}\n"));
    let mut first = None;
    for (window_index, window) in windows.into_iter().enumerate() {
        let old_line_offset = old[..window.old_start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        let new_line_offset = new[..window.new_start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        let diff = TextDiff::from_lines(
            &old[window.old_start..window.old_end],
            &new[window.new_start..window.new_end],
        );
        for (group_index, group) in diff.grouped_ops(4).iter().enumerate() {
            if window_index > 0 || group_index > 0 {
                display.push(" ...\n");
            }
            let old_range = group
                .first()
                .expect("non-empty diff group")
                .old_range()
                .start
                ..group.last().expect("non-empty diff group").old_range().end;
            let new_range = group
                .first()
                .expect("non-empty diff group")
                .new_range()
                .start
                ..group.last().expect("non-empty diff group").new_range().end;
            let old_count = old_range.end - old_range.start;
            let new_count = new_range.end - new_range.start;
            let old_header = old_line_offset + old_range.start + usize::from(old_count > 0);
            let new_header = new_line_offset + new_range.start + usize::from(new_count > 0);
            patch.push(&format!(
                "@@ -{old_header},{old_count} +{new_header},{new_count} @@\n"
            ));
            let mut old_line = old_line_offset + old_range.start + 1;
            let mut new_line = new_line_offset + new_range.start + 1;
            for op in group {
                for change in diff.iter_changes(op) {
                    let (prefix, line) = match change.tag() {
                        ChangeTag::Delete => {
                            first.get_or_insert(new_line);
                            ('-', old_line)
                        }
                        ChangeTag::Insert => {
                            first.get_or_insert(new_line);
                            ('+', new_line)
                        }
                        ChangeTag::Equal => (' ', old_line),
                    };
                    let value = change.value();
                    display.push(&format!(
                        "{prefix}{line:width$} {}\n",
                        value.strip_suffix('\n').unwrap_or(value)
                    ));
                    patch.push(&prefix.to_string());
                    patch.push(value);
                    if !value.ends_with('\n') {
                        patch.push("\n\\ No newline at end of file\n");
                    }
                    if display.truncated || patch.truncated {
                        display.mark_truncated();
                        patch.mark_truncated();
                        return (
                            display.text,
                            patch.text,
                            first.or_else(|| first_changed_line(old, new)),
                        );
                    }
                    if change.tag() != ChangeTag::Insert {
                        old_line += 1;
                    }
                    if change.tag() != ChangeTag::Delete {
                        new_line += 1;
                    }
                }
            }
        }
    }
    if display.text.ends_with('\n') {
        display.text.pop();
    }
    (display.text, patch.text, first)
}

const DIFF_OMITTED: &str = "\n[... diff truncated; incomplete context/patch ...]\n";

#[derive(Default)]
struct BoundedDiff {
    text: String,
    newlines: usize,
    truncated: bool,
}

impl BoundedDiff {
    fn push(&mut self, value: &str) {
        if self.truncated {
            return;
        }
        // Reserve room for the marker, including its lines, at every append.
        let remaining = (MAX_BYTES - DIFF_OMITTED.len()).saturating_sub(self.text.len());
        let mut prefix = clip_head(value, remaining);
        let remaining_lines = (MAX_LINES - 3).saturating_sub(self.newlines);
        if let Some((position, _)) = prefix.match_indices('\n').nth(remaining_lines) {
            prefix = &prefix[..position];
        }
        self.newlines += prefix.bytes().filter(|byte| *byte == b'\n').count();
        self.text.push_str(prefix);
        if prefix.len() != value.len() {
            self.mark_truncated();
        }
    }

    fn mark_truncated(&mut self) {
        if !self.truncated {
            self.text.push_str(DIFF_OMITTED);
            self.truncated = true;
        }
    }
}

fn common_edges(old: &str, new: &str) -> (usize, usize) {
    let prefix = old
        .chars()
        .zip(new.chars())
        .take_while(|(before, after)| before == after)
        .map(|(ch, _)| ch.len_utf8())
        .sum();
    let suffix = old[prefix..]
        .chars()
        .rev()
        .zip(new[prefix..].chars().rev())
        .take_while(|(before, after)| before == after)
        .map(|(ch, _)| ch.len_utf8())
        .sum();
    (prefix, suffix)
}

fn first_changed_line(old: &str, new: &str) -> Option<usize> {
    let mut line = 1;
    for (before, after) in old.chars().zip(new.chars()) {
        if before != after {
            return Some(line);
        }
        line += usize::from(after == '\n');
    }
    (old.len() != new.len()).then_some(line)
}

fn diff_excerpts(
    old: &str,
    new: &str,
    replacements: &[Replacement],
) -> (String, String, Option<usize>) {
    let first = first_changed_line(old, new);
    let mut display = BoundedDiff::default();
    let mut patch = BoundedDiff::default();
    display.push("[Bounded diff excerpts; omitted source is marked with ...]\n");
    patch.push("[Full patch omitted: excerpts below are not an applicable unified patch.]\n");
    let mut delta = 0_i64;
    for replacement in replacements {
        let before = &old[replacement.start..replacement.start + replacement.length];
        let new_start = (replacement.start as i64 + delta) as usize;
        delta += replacement.text.len() as i64 - replacement.length as i64;
        if before == replacement.text {
            continue;
        }
        let (prefix, suffix) = common_edges(before, &replacement.text);
        for (sign, text, start, end) in [
            (
                '-',
                old,
                replacement.start + prefix,
                replacement.start + replacement.length - suffix,
            ),
            (
                '+',
                new,
                new_start + prefix,
                new_start + replacement.text.len() - suffix,
            ),
        ] {
            let line = text[..start].bytes().filter(|byte| *byte == b'\n').count() + 1;
            let preview = format!("{sign}{line} {}\n", changed_excerpt(text, start, end));
            display.push(&preview);
            patch.push(&preview);
        }
        if display.truncated || patch.truncated {
            display.mark_truncated();
            patch.mark_truncated();
            break;
        }
    }
    (display.text, patch.text, first)
}

fn changed_excerpt(text: &str, start: usize, end: usize) -> String {
    let line_start = text[..start].rfind('\n').map_or(0, |position| position + 1);
    let line_end = text[end..]
        .find('\n')
        .map_or(text.len(), |position| end + position);
    let before = clip_tail(&text[line_start..start], 256);
    let after = clip_head(&text[end..line_end], 256);
    let changed = &text[start..end];
    let mut result = String::new();
    if line_start + before.len() < start {
        result.push_str("... ");
    }
    result.push_str(before);
    if changed.len() <= 2048 {
        result.push_str(changed);
    } else {
        let head = clip_head(changed, 1024);
        let tail = clip_tail(changed, 1024);
        result.push_str(head);
        result.push_str(&format!(
            "[... {} bytes omitted ...]",
            changed.len() - head.len() - tail.len()
        ));
        result.push_str(tail);
    }
    result.push_str(after);
    if end + after.len() < line_end {
        result.push_str(" ...");
    }
    result
}

fn context_start(text: &str, position: usize, context: usize) -> usize {
    let mut start = text[..position.min(text.len())]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    for _ in 0..context {
        if start == 0 {
            break;
        }
        start = text[..start - 1].rfind('\n').map_or(0, |index| index + 1);
    }
    start
}

fn context_end(text: &str, position: usize, context: usize) -> usize {
    let mut end = position.min(text.len());
    for _ in 0..=context {
        let Some(offset) = text[end..].find('\n') else {
            return text.len();
        };
        end += offset + 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_budgets_count_utf8_bytes_and_lines_and_reserve_the_marker() {
        for content in ["🙂".repeat(20_000), "line\n".repeat(3000)] {
            let mut budget = BoundedDiff::default();
            budget.push(&content);
            assert!(budget.truncated);
            assert!(budget.text.len() <= MAX_BYTES);
            assert!(budget.text.lines().count() <= MAX_LINES);
            assert!(budget.text.ends_with(DIFF_OMITTED));
            let length = budget.text.len();
            budget.push("must not appear");
            budget.mark_truncated();
            assert_eq!(budget.text.len(), length);
        }
    }

    #[test]
    fn first_changed_line_retains_line_diff_semantics_for_inserted_empty_lines() {
        for prefix in [String::new(), "🙂".repeat(20_000)] {
            let old = format!("{prefix}one\ntwo\n");
            let replacement = Replacement {
                start: prefix.len(),
                length: 3,
                text: "one\n".into(),
                edit_index: 0,
            };
            let new = apply(&old, std::slice::from_ref(&replacement));
            let (diff, patch, first) = make_diff("fixture", &old, &new, &[replacement]);
            assert_eq!(first, Some(2));
            assert!(diff.len() <= MAX_BYTES);
            assert!(patch.len() <= MAX_BYTES);
        }
    }
}
