//! Retained `bash` tool contract with a native shell and owned process groups/jobs.
//!
//! Shared executor behavior is adapted from Pi core/tools/bash.ts at
//! 780bf31fcaeb252fa9b68df3964c735eae75bde3.
//! Copyright (c) 2025 Mario Zechner. MIT license; see the repository LICENSE.

use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::message::ToolResult;
use crate::storage::output_name;

use super::output::OutputAccumulator;
use super::shell::NativeShell;
use super::{Execution, ToolUpdate};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashArgs {
    /// Shell command to execute in the current working directory.
    pub command: String,
    /// Optional timeout in seconds. There is no default timeout.
    pub timeout: Option<f64>,
}

struct OwnedChild {
    child: Box<dyn ChildWrapper>,
    finished: bool,
}

impl OwnedChild {
    fn stop(&mut self) {
        #[cfg(target_os = "macos")]
        let _ = self.child.signal(libc::SIGSTOP);
        let _ = self.child.start_kill();
    }

    fn mark_finished(&mut self) {
        self.finished = true;
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.finished {
            self.stop();
        }
    }
}

pub async fn execute(
    args: BashArgs,
    cwd: &Path,
    outputs: &Path,
    execution: &Execution,
    lifetime: &CancellationToken,
) -> Result<ToolResult, String> {
    if execution.cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }
    let timeout = match args.timeout {
        Some(seconds) if seconds.is_finite() && seconds > 0.0 && seconds <= 2_147_483.647 => {
            Some(Duration::from_secs_f64(seconds))
        }
        Some(_) => {
            return Err("Invalid timeout: use positive finite seconds, at most 2147483.647".into());
        }
        None => None,
    };
    let shell = NativeShell::discover()?;
    let shell_name = shell.name();
    let mut native_command = shell.command(&args.command)?;
    native_command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(&execution.metadata);
    let mut command = CommandWrap::from(native_command);
    command.wrap(KillOnDrop);
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(process_wrap::tokio::JobObject);
    let child = command
        .spawn()
        .map_err(|_| "Could not start the native shell in the working directory")?;
    let mut child = OwnedChild {
        child,
        finished: false,
    };
    let stdout = child
        .child
        .stdout()
        .take()
        .ok_or("Shell stdout unavailable")?;
    let stderr = child
        .child
        .stderr()
        .take()
        .ok_or("Shell stderr unavailable")?;
    let (sender, mut chunks) = mpsc::channel(16);
    let stdout_task = tokio::spawn(read_pipe(stdout, sender.clone()));
    let stderr_task = tokio::spawn(read_pipe(stderr, sender));
    let started = Instant::now();
    let deadline = timeout.map(|timeout| tokio::time::Instant::now() + timeout);
    let mut output = OutputAccumulator::new(outputs.join(output_name(&execution.call_id)));
    let mut status = None;
    let mut shell_exited_after: Option<Duration> = None;
    let mut pipes_closed = false;
    let mut idle = None;
    let mut cleanup = None;
    let mut failure = None;
    let mut last_update = Instant::now();
    loop {
        if status.is_some() && pipes_closed {
            break;
        }
        tokio::select! {
            _ = execution.cancel.cancelled(), if failure.is_none() => {
                failure = Some("Command aborted".to_owned());
                child.stop();
                cleanup = Some(tokio::time::Instant::now() + Duration::from_secs(2));
            }
            _ = wait_deadline(deadline), if failure.is_none() => {
                failure = Some(if let (Some(status), Some(elapsed)) = (&status, shell_exited_after) {
                    format!(
                        "{shell_name} exited with code {} after {:.1} seconds, but inherited output pipes remained open until the {} second timeout",
                        exit_code(*status), elapsed.as_secs_f64(), args.timeout.unwrap_or(0.0)
                    )
                } else {
                    format!("Command timed out after {} seconds", args.timeout.unwrap_or(0.0))
                });
                child.stop();
                cleanup = Some(tokio::time::Instant::now() + Duration::from_secs(2));
            }
            _ = wait_deadline(cleanup), if failure.is_some() => break,
            // An explicit timeout covers inherited pipes too. The no-timeout
            // idle grace must not turn a still-running descendant into success.
            _ = wait_deadline(idle), if status.is_some() && deadline.is_none() && failure.is_none() => break,
            result = child.child.inner_mut().wait(), if status.is_none() => {
                status = Some(result.map_err(|_| "Cannot wait for the shell")?);
                shell_exited_after = Some(started.elapsed());
                idle = Some(tokio::time::Instant::now() + Duration::from_millis(100));
            }
            chunk = chunks.recv(), if !pipes_closed => {
                if let Some(chunk) = chunk {
                    let bytes = chunk.map_err(|_| "Cannot read shell output")?;
                    output.append(&bytes).await.map_err(|_| "Cannot retain shell output")?;
                    if status.is_some() {
                        idle = Some(tokio::time::Instant::now() + Duration::from_millis(100));
                    }
                    if last_update.elapsed() >= Duration::from_millis(50) {
                        if let Some(sender) = &execution.updates {
                            let _ = sender.try_send(ToolUpdate { call_id: execution.call_id.clone(), text: output.formatted() });
                        }
                        last_update = Instant::now();
                    }
                } else {
                    pipes_closed = true;
                }
            }
        }
    }
    stdout_task.abort();
    stderr_task.abort();
    if failure.is_none() {
        #[cfg(unix)]
        {
            child.mark_finished();
        }
        #[cfg(windows)]
        {
            // Retain the job handle for successful background commands until the session ends.
            let lifetime = lifetime.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = child.child.wait() => { child.mark_finished(); }
                    _ = lifetime.cancelled() => { child.stop(); }
                }
            });
        }
    }
    #[cfg(not(windows))]
    let _ = lifetime;
    output
        .finish()
        .await
        .map_err(|_| "Cannot finish shell output")?;
    let exit = status.map_or(1, exit_code);
    let mut text = output.formatted();
    if let Some(failure) = &failure {
        text.push_str(&format!("\n\n{failure}"));
    } else if exit != 0 {
        text.push_str(&format!("\n\nCommand exited with code {exit}"));
    }
    let (full, truncated) = output
        .structured_output()
        .await
        .map_err(|_| "Cannot read retained output")?;
    let path = output
        .path()
        .map(|path| path.to_string_lossy().into_owned());
    Ok(ToolResult {
        content: vec![crate::message::Content::text(text)],
        details: json!({"truncation":output.snapshot(), "fullOutputPath":path}),
        is_error: failure.is_some() || exit != 0,
        structured_content: Some(json!({
            "output":full, "truncated":truncated, "full_output_path":path,
            "exit_code":exit, "wall_time_seconds":(started.elapsed().as_secs_f64() * 10.0).round() / 10.0
        })),
    })
}

async fn wait_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn read_pipe(
    mut pipe: impl AsyncRead + Unpin,
    sender: mpsc::Sender<std::io::Result<Vec<u8>>>,
) {
    let mut buffer = [0; 64 * 1024];
    // Each OS pipe is its own byte stream. Mixing incomplete UTF-8 scalars
    // from stdout and stderr before decoding corrupts otherwise valid output.
    let mut decoder = crate::text::Utf8Decoder::default();
    loop {
        match pipe.read(&mut buffer).await {
            Ok(0) => {
                let tail = decoder.finish();
                if !tail.is_empty() {
                    let _ = sender.send(Ok(tail.into_bytes())).await;
                }
                return;
            }
            Ok(count) => {
                let text = decoder.push(&buffer[..count]);
                if !text.is_empty() && sender.send(Ok(text.into_bytes())).await.is_err() {
                    return;
                }
            }
            Err(error) => {
                let _ = sender.send(Err(error)).await;
                return;
            }
        }
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
    }
    #[cfg(windows)]
    {
        status.code().unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn interleaved_pipes_never_combine_incomplete_utf8_scalars() {
        let (stdout, mut stdout_write) = tokio::io::duplex(16);
        let (stderr, mut stderr_write) = tokio::io::duplex(16);
        let (sender, mut receiver) = mpsc::channel(4);
        let out = tokio::spawn(read_pipe(stdout, sender.clone()));
        let err = tokio::spawn(read_pipe(stderr, sender));
        stdout_write.write_all(&[0xe4]).await.unwrap();
        stderr_write.write_all(b"diagnostic").await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first, b"diagnostic");
        stdout_write.write_all(&[0xb8, 0xad]).await.unwrap();
        stdout_write.shutdown().await.unwrap();
        stderr_write.shutdown().await.unwrap();
        out.await.unwrap();
        err.await.unwrap();
        assert_eq!(
            String::from_utf8(receiver.recv().await.unwrap().unwrap()).unwrap(),
            "中"
        );
        assert!(receiver.recv().await.is_none());
    }
}
