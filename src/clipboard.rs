//! Bounded, cancellable OS clipboard transport, separate from terminal drawing.
//!
//! Fixed helpers follow Pi's Wayland/X11/WSL clipboard paths. Text is sent on
//! stdin, never interpolated into a shell script, argument, diagnostic, or log.
//! This clipboard bridge is separate from the model-facing shell-command tool.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use process_wrap::tokio::{CommandWrap, KillOnDrop};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::message::Content;

const IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const TEXT_BYTES: u64 = 2 * 1024 * 1024;

pub enum Item {
    Image(Content),
    Text(String),
    Empty,
}

#[cfg(not(target_os = "macos"))]
const WINDOWS_IMAGE: &str = "Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; $img=[System.Windows.Forms.Clipboard]::GetImage(); if($img){$stream=New-Object System.IO.MemoryStream; try{$img.Save($stream,[System.Drawing.Imaging.ImageFormat]::Png); [Console]::Write([Convert]::ToBase64String($stream.ToArray()))}finally{$stream.Dispose();$img.Dispose()}}";
const WINDOWS_TEXT: &str = "Add-Type -AssemblyName System.Windows.Forms; [Console]::OutputEncoding=New-Object System.Text.UTF8Encoding; [Console]::Write([System.Windows.Forms.Clipboard]::GetText())";

async fn program(
    executable: impl AsRef<std::ffi::OsStr>,
    args: &[&str],
    limit: u64,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, String> {
    transport(executable, args, limit, None, cancel).await
}

async fn transport(
    executable: impl AsRef<std::ffi::OsStr>,
    args: &[&str],
    limit: u64,
    input: Option<&[u8]>,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, String> {
    let mut command = CommandWrap::with_new(executable, |command| {
        command
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let env = [
            "PATH",
            "HOME",
            "USERPROFILE",
            "SystemRoot",
            "SYSTEMROOT",
            "WINDIR",
            "TEMP",
            "TMP",
            "TMPDIR",
            "LANG",
            "LC_ALL",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "XDG_RUNTIME_DIR",
            "WSL_INTEROP",
            "WSL_DISTRO_NAME",
        ]
        .into_iter()
        .filter_map(|key| std::env::var_os(key).map(|value| (key, value)))
        .collect::<Vec<_>>();
        command.env_clear().envs(env);
    });
    command.wrap(KillOnDrop);
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(process_wrap::tokio::JobObject);
    let mut child = command
        .spawn()
        .map_err(|_| "Clipboard reader is unavailable")?;
    let stdout = child
        .stdout()
        .take()
        .ok_or("Clipboard reader has no output")?;
    let stdin = child.stdin().take();
    let read = async {
        let reader = async {
            let mut bytes = Vec::new();
            stdout
                .take(limit + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| "Cannot read clipboard")?;
            if bytes.len() as u64 > limit {
                return Err("Clipboard content exceeds the processing limit");
            }
            Ok(bytes)
        };
        let writer = async {
            if let Some(input) = input {
                let mut stdin = stdin.ok_or("Clipboard helper has no input")?;
                stdin
                    .write_all(input)
                    .await
                    .map_err(|_| "Cannot write clipboard")?;
                stdin
                    .shutdown()
                    .await
                    .map_err(|_| "Cannot write clipboard")?;
            }
            Ok(())
        };
        let (bytes, ()) = tokio::try_join!(reader, writer)?;
        let status = child.wait().await.map_err(|_| "Clipboard helper failed")?;
        if !status.success() {
            return Err("Clipboard helper is unavailable");
        }
        Ok(bytes)
    };
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err("Clipboard read cancelled"),
        result = tokio::time::timeout(Duration::from_secs(3), read) => result.unwrap_or(Err("Clipboard reader timed out")),
    };
    if result.is_err() {
        #[cfg(target_os = "macos")]
        let _ = child.signal(libc::SIGSTOP);
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    }
    result.map_err(str::to_owned)
}

fn wayland_helper_available(name: &str) -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        && std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|directory| {
                let Ok(metadata) = std::fs::metadata(directory.join(name)) else {
                    return false;
                };
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                }
                #[cfg(windows)]
                {
                    metadata.is_file()
                }
            })
        })
}

fn is_wsl() -> bool {
    cfg!(target_os = "linux")
        && (std::env::var_os("WSL_INTEROP").is_some()
            || std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .is_ok_and(|s| s.to_ascii_lowercase().contains("microsoft")))
}

#[cfg(not(target_os = "macos"))]
async fn windows_image(cancel: &CancellationToken) -> Result<Option<Vec<u8>>, String> {
    let bytes = program(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-Command",
            WINDOWS_IMAGE,
        ],
        IMAGE_BYTES * 4 / 3 + 4,
        cancel,
    )
    .await?;
    if bytes.is_empty() {
        return Ok(None);
    }
    STANDARD
        .decode(bytes)
        .map(Some)
        .map_err(|_| "Clipboard image encoding is invalid".into())
}

async fn image_bytes(
    cancel: &CancellationToken,
    scratch: &Path,
) -> Result<Option<Vec<u8>>, String> {
    #[cfg(target_os = "macos")]
    {
        let file = scratch.join("image.png");
        const SCRIPT: &str = "on run argv\nset targetFile to POSIX file (item 1 of argv)\nset handle to open for access targetFile with write permission\ntry\nset eof handle to 0\nwrite (the clipboard as «class PNGf») to handle\nclose access handle\non error\nclose access handle\nerror \"Clipboard image unavailable\"\nend try\nend run";
        let name = file.to_str().ok_or("Clipboard path is unavailable")?;
        if program("/usr/bin/osascript", &["-e", SCRIPT, name], 1024, cancel)
            .await
            .is_err()
        {
            return Ok(None);
        }
        let metadata = tokio::fs::metadata(&file)
            .await
            .map_err(|_| "Cannot read clipboard image")?;
        if metadata.len() > IMAGE_BYTES {
            return Err("Clipboard image exceeds the processing limit".into());
        }
        return tokio::fs::read(file)
            .await
            .map(Some)
            .map_err(|_| "Cannot read clipboard image".into());
    }
    #[cfg(not(target_os = "macos"))]
    let _ = scratch;
    #[cfg(windows)]
    return windows_image(cancel).await;
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some() || is_wsl();
        if wayland
            && let Ok(types) = program("wl-paste", &["--list-types"], 64 * 1024, cancel).await
        {
            let types = String::from_utf8_lossy(&types);
            if let Some(mime) = [
                "image/png",
                "image/jpeg",
                "image/webp",
                "image/gif",
                "image/bmp",
            ]
            .into_iter()
            .find(|mime| types.lines().any(|line| line.trim() == *mime))
            {
                return program(
                    "wl-paste",
                    &["--type", mime, "--no-newline"],
                    IMAGE_BYTES,
                    cancel,
                )
                .await
                .map(Some);
            }
            // An empty Wayland clipboard must not expose stale X11 data.
            if !is_wsl() {
                return Ok(None);
            }
        } else if let Ok(types) = program(
            "xclip",
            &["-selection", "clipboard", "-t", "TARGETS", "-o"],
            64 * 1024,
            cancel,
        )
        .await
        {
            let types = String::from_utf8_lossy(&types);
            if let Some(mime) = [
                "image/png",
                "image/jpeg",
                "image/webp",
                "image/gif",
                "image/bmp",
            ]
            .into_iter()
            .find(|mime| types.lines().any(|line| line.trim() == *mime))
            {
                return program(
                    "xclip",
                    &["-selection", "clipboard", "-t", mime, "-o"],
                    IMAGE_BYTES,
                    cancel,
                )
                .await
                .map(Some);
            }
        }
        if is_wsl() {
            return windows_image(cancel).await;
        }
        Ok(None)
    }
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub async fn read(session_dir: &Path, cancel: &CancellationToken) -> Result<Item, String> {
    let scratch = Scratch(session_dir.join(format!(".clipboard-{}", uuid::Uuid::new_v4())));
    crate::storage::private_dir(&scratch.0)
        .map_err(|_| "Cannot prepare clipboard image storage")?;
    if let Some(bytes) = image_bytes(cancel, &scratch.0).await? {
        let cancel = cancel.clone();
        let content =
            tokio::task::spawn_blocking(move || crate::tools::images::attachment(&bytes, &cancel))
                .await
                .map_err(|_| "Clipboard image worker failed")??;
        return Ok(Item::Image(content));
    }
    if cancel.is_cancelled() {
        return Err("Clipboard read cancelled".into());
    }
    let bytes = if cfg!(windows) || is_wsl() {
        program(
            "powershell.exe",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-STA",
                "-Command",
                WINDOWS_TEXT,
            ],
            TEXT_BYTES,
            cancel,
        )
        .await
    } else if cfg!(target_os = "macos") {
        program("/usr/bin/pbpaste", &[], TEXT_BYTES, cancel).await
    } else if wayland_helper_available("wl-paste") {
        program("wl-paste", &["--no-newline"], TEXT_BYTES, cancel).await
    } else {
        program(
            "xclip",
            &["-selection", "clipboard", "-o"],
            TEXT_BYTES,
            cancel,
        )
        .await
    }?;
    if bytes.is_empty() {
        return Ok(Item::Empty);
    }
    String::from_utf8(bytes)
        .map(Item::Text)
        .map_err(|_| "Clipboard text is not UTF-8".into())
}

/// Copy explicit user-selected text. No shell interpolation or hidden OSC52
/// terminal writes: the OS helper receives only bounded UTF-8 stdin.
pub async fn write(text: &str, cancel: &CancellationToken) -> Result<(), String> {
    if text.len() as u64 > TEXT_BYTES {
        return Err("Selection exceeds the 2 MiB clipboard limit".into());
    }
    if text.contains('\0') {
        return Err("Clipboard text cannot contain a NUL character".into());
    }
    if cancel.is_cancelled() {
        return Err("Clipboard write cancelled".into());
    }
    const SET_TEXT: &str = "Add-Type -AssemblyName System.Windows.Forms; [Console]::InputEncoding=New-Object System.Text.UTF8Encoding; $text=[Console]::In.ReadToEnd(); if($text.Length -eq 0){[System.Windows.Forms.Clipboard]::Clear()}else{[System.Windows.Forms.Clipboard]::SetText($text)}";
    let result = if cfg!(windows) || is_wsl() {
        transport(
            "powershell.exe",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-STA",
                "-Command",
                SET_TEXT,
            ],
            0,
            Some(text.as_bytes()),
            cancel,
        )
        .await
    } else if cfg!(target_os = "macos") {
        transport("/usr/bin/pbcopy", &[], 0, Some(text.as_bytes()), cancel).await
    } else if wayland_helper_available("wl-copy") {
        transport(
            "wl-copy",
            &["--type", "text/plain;charset=utf-8"],
            0,
            Some(text.as_bytes()),
            cancel,
        )
        .await
    } else {
        transport(
            "xclip",
            &["-selection", "clipboard", "-in"],
            0,
            Some(text.as_bytes()),
            cancel,
        )
        .await
    };
    result.map(|_| ()).map_err(|error| {
        if cancel.is_cancelled() {
            "Clipboard write cancelled".into()
        } else {
            error.replace("Clipboard read", "Clipboard write")
        }
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn writes_pass_literal_bounded_text_on_stdin_and_cancel_cleanly() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("clipboard");
        std::fs::write(
            &script,
            "#!/bin/bash\nIFS= read -r line; printf '%s' \"$line\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cancel = CancellationToken::new();
        let literal = "'\"; $(touch NOT_RUN); 世界🙂";
        let output = transport(&script, &[], 1024, Some(literal.as_bytes()), &cancel)
            .await
            .unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), literal);
        assert!(!temp.path().join("NOT_RUN").exists());
        assert!(
            write(&"x".repeat(TEXT_BYTES as usize + 1), &cancel)
                .await
                .unwrap_err()
                .contains("limit")
        );
        assert!(write("a\0b", &cancel).await.is_err());
        cancel.cancel();
        assert_eq!(
            write("not sent", &cancel).await.unwrap_err(),
            "Clipboard write cancelled"
        );
    }

    #[tokio::test]
    async fn readers_are_bounded_cancellable_and_redact_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("clipboard");
        std::fs::write(
            &script,
            "#!/bin/bash\nprintf abcdef\nprintf secret-diagnostic >&2\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cancel = CancellationToken::new();
        assert_eq!(program(&script, &[], 10, &cancel).await.unwrap(), b"abcdef");
        assert!(
            program(&script, &[], 2, &cancel)
                .await
                .unwrap_err()
                .contains("processing limit")
        );
        std::fs::write(&script, "#!/bin/bash\nsleep 20\n").unwrap();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            stop.cancel();
        });
        assert_eq!(
            program(&script, &[], 10, &cancel).await.unwrap_err(),
            "Clipboard read cancelled"
        );
    }
}
