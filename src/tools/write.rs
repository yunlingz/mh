//! Create or overwrite a UTF-8 file, creating parent directories.

use std::io::Write;
use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::message::ToolResult;

use super::path::{io_code, resolve};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteArgs {
    /// Relative or absolute destination path.
    pub path: String,
    /// Complete UTF-8 file content.
    pub content: String,
}

pub fn execute(
    args: WriteArgs,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<ToolResult, String> {
    if cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }
    let path = resolve(&args.path, cwd);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| io_code(&error))?;
    }
    if cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }
    let mut file = crate::regular_file::open_write(&path).map_err(|error| io_code(&error))?;
    if cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }
    file.set_len(0).map_err(|error| io_code(&error))?;
    file.write_all(args.content.as_bytes())
        .map_err(|error| io_code(&error))?;
    if cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }
    Ok(ToolResult::text(format!(
        "Successfully wrote to {}",
        args.path
    )))
}
