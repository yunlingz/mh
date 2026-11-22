//! Native shell selection, separate from the retained `bash` tool's process executor.
//!
//! Discovery and Windows PowerShell exit handling are adapted from Pi
//! `utils/shell.ts` and `core/tools/powershell.ts` at
//! 780bf31fcaeb252fa9b68df3964c735eae75bde3.
//! Copyright (c) 2025 Mario Zechner. MIT license; see the repository LICENSE.
//! Pi exposes a separate `powershell` tool; mh intentionally keeps one `bash`
//! name and selects the native platform's syntax without translating commands.

use std::path::{Path, PathBuf};

#[cfg(any(windows, test))]
use base64::Engine;
use tokio::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnixShell {
    Bash,
    Sh,
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerShell {
    Modern,
    Windows,
}

#[derive(Clone, Debug)]
pub enum NativeShell {
    Unix {
        executable: PathBuf,
        kind: UnixShell,
    },
    #[cfg(any(windows, test))]
    PowerShell {
        executable: PathBuf,
        kind: PowerShell,
    },
}

impl NativeShell {
    pub fn discover() -> Result<Self, String> {
        #[cfg(unix)]
        {
            let executable = if is_executable(Path::new("/bin/bash")) {
                Some(PathBuf::from("/bin/bash"))
            } else {
                executable_on_path("bash")
            };
            if let Some(executable) = executable {
                return Ok(Self::Unix {
                    executable,
                    kind: UnixShell::Bash,
                });
            }
            let executable = executable_on_path("sh")
                .or_else(|| is_executable(Path::new("/bin/sh")).then(|| PathBuf::from("/bin/sh")))
                .ok_or("No Unix shell executable found. Bash or sh must be available.")?;
            Ok(Self::Unix {
                executable,
                kind: UnixShell::Sh,
            })
        }
        #[cfg(windows)]
        {
            discover_powershell(
                executable_on_path,
                std::env::var_os("ProgramFiles").map(PathBuf::from),
                std::env::var_os("SystemRoot").map(PathBuf::from),
                Path::is_file,
            )
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Unix {
                kind: UnixShell::Bash,
                ..
            } => "Bash",
            Self::Unix {
                kind: UnixShell::Sh,
                ..
            } => "sh",
            #[cfg(any(windows, test))]
            Self::PowerShell {
                kind: PowerShell::Modern,
                ..
            } => "PowerShell (pwsh)",
            #[cfg(any(windows, test))]
            Self::PowerShell {
                kind: PowerShell::Windows,
                ..
            } => "Windows PowerShell",
        }
    }

    pub(crate) fn command(&self, script: &str) -> Result<Command, String> {
        match self {
            Self::Unix { executable, .. } => {
                let mut command = Command::new(executable);
                command.arg("-c").arg(script);
                Ok(command)
            }
            #[cfg(any(windows, test))]
            Self::PowerShell { executable, kind } => {
                let mut command = Command::new(executable);
                // UTF-16LE EncodedCommand avoids Windows command-line quoting and
                // code-page loss, while parsing the entire multiline script before
                // executing it. `-Command -` would execute incomplete input piecemeal.
                // Text prevents Windows PowerShell's encoded-command CLIXML errors.
                // Unlike Pi, mh does NOT pass ExecutionPolicy Bypass or change policy.
                let script = powershell_script(script, *kind);
                validate_powershell_transport(executable, &script)?;
                command.args(POWERSHELL_ARGS);
                command.arg(encode_powershell(&script));
                Ok(command)
            }
        }
    }
}

/// Syntax guidance is platform-specific, even though the public name remains `bash`.
pub fn syntax_description() -> &'static str {
    if cfg!(windows) {
        "PowerShell (pwsh preferred, Windows PowerShell fallback); use PowerShell syntax, not Bash."
    } else {
        "Unix shell (Bash when available, otherwise sh); use Unix shell syntax."
    }
}

pub(crate) fn tool_description() -> &'static str {
    if cfg!(windows) {
        "Execute a PowerShell command in the working directory (the tool name is bash). Use PowerShell syntax, not Bash. Prefers pwsh, otherwise Windows PowerShell; no profile or execution-policy override. Returns stdout and stderr, limited to the last 2000 lines or 50 KiB. Full truncated output is retained. Timeout is optional."
    } else {
        "Execute a Unix shell command in the working directory, using Bash when available or sh otherwise. Returns stdout and stderr, limited to the last 2000 lines or 50 KiB. Full truncated output is retained. Timeout is optional."
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
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
}

fn executable_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            // Resolve relative PATH entries now, before the child switches cwd.
            .find_map(|candidate| {
                std::fs::canonicalize(candidate)
                    .ok()
                    .filter(|path| is_executable(path))
            })
    })
}

#[cfg(any(windows, test))]
fn discover_powershell(
    mut on_path: impl FnMut(&str) -> Option<PathBuf>,
    program_files: Option<PathBuf>,
    system_root: Option<PathBuf>,
    is_file: impl Fn(&Path) -> bool,
) -> Result<NativeShell, String> {
    let modern = on_path("pwsh.exe").or_else(|| {
        program_files
            .map(|root| root.join("PowerShell/7/pwsh.exe"))
            .filter(|path| is_file(path))
    });
    if let Some(executable) = modern {
        return Ok(NativeShell::PowerShell {
            executable,
            kind: PowerShell::Modern,
        });
    }
    let windows = on_path("powershell.exe").or_else(|| {
        system_root
            .map(|root| root.join("System32/WindowsPowerShell/v1.0/powershell.exe"))
            .filter(|path| is_file(path))
    });
    windows
        .map(|executable| NativeShell::PowerShell {
            executable,
            kind: PowerShell::Windows,
        })
        .ok_or_else(|| {
            "No native PowerShell executable found. Make pwsh.exe or powershell.exe available."
                .into()
        })
}

#[cfg(any(windows, test))]
const POWERSHELL_ARGS: [&str; 5] = [
    "-NoProfile",
    "-NonInteractive",
    "-OutputFormat",
    "Text",
    "-EncodedCommand",
];

#[cfg(any(windows, test))]
fn validate_powershell_transport(executable: &Path, script: &str) -> Result<(), String> {
    #[cfg(windows)]
    let executable_units = {
        use std::os::windows::ffi::OsStrExt;
        executable.as_os_str().encode_wide().count()
    };
    #[cfg(not(windows))]
    let executable_units = executable.to_string_lossy().encode_utf16().count();
    // CreateProcess permits 32767 UTF-16 units including the terminating NUL.
    // Conservatively budget executable quoting/escaping, fixed arguments, and
    // base64 expansion before allocating the encoded argument. This boundary
    // rejects oversized commands without leaking executable paths or source.
    let fixed_units = executable_units
        .saturating_mul(2)
        .saturating_add(2)
        .saturating_add(
            POWERSHELL_ARGS
                .iter()
                .map(|arg| arg.len() + 1)
                .sum::<usize>(),
        )
        .saturating_add(1);
    let encoded_units = script
        .encode_utf16()
        .count()
        .saturating_mul(2)
        .div_ceil(3)
        .saturating_mul(4);
    if fixed_units.saturating_add(encoded_units) > 32_766 {
        return Err("PowerShell command exceeds the native Windows command-line limit".into());
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn powershell_script(command: &str, kind: PowerShell) -> String {
    // Preserve Pi's final-statement exit semantics and 5.1 native-stderr correction.
    // The lone semicolon prevents an unfinished pipeline/expression from continuing
    // into the suffix. A trailing backtick must remain a parse error, not escape it.
    const UTF8_PREFIX: &str = "try { [Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false); $OutputEncoding=[Console]::OutputEncoding } catch {}\n";
    const WINDOWS_EXIT_SUFFIX: &str = "\n;\nif ($?) { exit 0 }\nif (($LASTEXITCODE -eq 0) -and (\"$($Error[0].FullyQualifiedErrorId)\" -like \"NativeCommandError*\")) { exit 0 }\nexit 1";
    let suffix = if kind == PowerShell::Windows && !command.ends_with('`') {
        WINDOWS_EXIT_SUFFIX
    } else {
        ""
    };
    format!("{UTF8_PREFIX}{command}{suffix}")
}

#[cfg(any(windows, test))]
fn encode_powershell(script: &str) -> String {
    let utf16 = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    base64::engine::general_purpose::STANDARD.encode(utf16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_bash_and_sh_keep_native_script_and_exit_behavior() {
        let fixture = tempfile::tempdir().unwrap();
        for (executable, kind) in [("/bin/bash", UnixShell::Bash), ("/bin/sh", UnixShell::Sh)] {
            if !Path::new(executable).is_file() {
                continue;
            }
            let shell = NativeShell::Unix {
                executable: PathBuf::from(executable),
                kind,
            };
            let output = shell.command("value='héllo € 🙂';\nprintf '%s\\n' \"$value\"; printf 'stderr héllo € 🙂\\n' >&2; exit 7").unwrap().current_dir(fixture.path()).stdin(std::process::Stdio::null()).output().await.unwrap();
            assert_eq!(output.status.code(), Some(7));
            assert!(
                String::from_utf8(output.stdout)
                    .unwrap()
                    .contains("héllo € 🙂")
            );
            assert!(
                String::from_utf8(output.stderr)
                    .unwrap()
                    .contains("stderr héllo € 🙂")
            );
        }
    }

    #[test]
    fn powershell_discovery_prefers_modern_then_native_fallback() {
        let modern_path = PathBuf::from("tools/pwsh.exe");
        let legacy_path = PathBuf::from("tools/powershell.exe");
        let program_files = PathBuf::from("applications");
        let system_root = PathBuf::from("system");
        for (modern_on_path, modern_installed, legacy_on_path, legacy_installed, expected) in [
            (
                true,
                true,
                true,
                true,
                Some((modern_path.clone(), PowerShell::Modern)),
            ),
            (
                false,
                true,
                true,
                true,
                Some((
                    program_files.join("PowerShell/7/pwsh.exe"),
                    PowerShell::Modern,
                )),
            ),
            (
                false,
                false,
                true,
                true,
                Some((legacy_path.clone(), PowerShell::Windows)),
            ),
            (
                false,
                false,
                false,
                true,
                Some((
                    system_root.join("System32/WindowsPowerShell/v1.0/powershell.exe"),
                    PowerShell::Windows,
                )),
            ),
            (false, false, false, false, None),
        ] {
            let result = discover_powershell(
                |name| match name {
                    "pwsh.exe" if modern_on_path => Some(modern_path.clone()),
                    "powershell.exe" if legacy_on_path => Some(legacy_path.clone()),
                    _ => None,
                },
                Some(program_files.clone()),
                Some(system_root.clone()),
                |path| {
                    (modern_installed && path == program_files.join("PowerShell/7/pwsh.exe"))
                        || (legacy_installed
                            && path
                                == system_root
                                    .join("System32/WindowsPowerShell/v1.0/powershell.exe"))
                },
            );
            match expected {
                Some((path, edition)) => assert!(
                    matches!(result.unwrap(), NativeShell::PowerShell { executable, kind } if executable == path && kind == edition)
                ),
                None => assert!(result.is_err()),
            }
        }
    }

    #[test]
    fn powershell_transport_preserves_unicode_quotes_and_multiline_without_policy_override() {
        let script = "$x = 'héllo € 🙂 ''quoted'''\nWrite-Output $x";
        for kind in [PowerShell::Modern, PowerShell::Windows] {
            let shell = NativeShell::PowerShell {
                executable: PathBuf::from("powershell.exe"),
                kind,
            };
            let command = shell.command(script).unwrap();
            let args = command
                .as_std()
                .get_args()
                .map(|arg| arg.to_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                &args[..5],
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-OutputFormat",
                    "Text",
                    "-EncodedCommand"
                ]
            );
            assert_eq!(args.len(), 6);
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(args[5])
                .unwrap();
            let utf16 = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect::<Vec<_>>();
            assert_eq!(
                String::from_utf16(&utf16).unwrap(),
                powershell_script(script, kind)
            );
            assert!(
                !args
                    .iter()
                    .any(|arg| arg.to_lowercase().contains("executionpolicy") || *arg == "Bypass")
            );
        }
    }

    #[test]
    fn powershell_transport_rejects_oversized_commands_without_exposing_source() {
        let shell = NativeShell::PowerShell {
            executable: PathBuf::from("private-location/pwsh.exe"),
            kind: PowerShell::Modern,
        };
        assert!(shell.command(&"x".repeat(10_000)).is_ok());
        let error = shell.command(&"private-source".repeat(2_000)).unwrap_err();
        assert_eq!(
            error,
            "PowerShell command exceeds the native Windows command-line limit"
        );
        assert!(shell.command(&"🙂".repeat(7_000)).is_err());
    }

    #[test]
    fn legacy_suffix_does_not_complete_malformed_commands() {
        for command in [
            "Get-ChildItem |",
            "$true -and",
            "Write-Output a,",
            "$x =",
            "Write-Output (1 +",
            "Write-Output \"unclosed",
            "if ($true) {",
        ] {
            assert!(
                powershell_script(command, PowerShell::Windows)
                    .contains(&format!("{command}\n;\n"))
            );
        }
        let trailing_backtick = powershell_script("Write-Output done `", PowerShell::Windows);
        assert!(trailing_backtick.ends_with("done `"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn installed_powershell_editions_preserve_pi_exit_and_parse_semantics() {
        // All child execution stays in a controlled temporary fixture. The regular
        // integration suite covers shared streaming, limits, timeout and job cleanup.
        let fixture = tempfile::tempdir().unwrap();
        let modern = discover_powershell(
            executable_on_path,
            std::env::var_os("ProgramFiles").map(PathBuf::from),
            None,
            Path::is_file,
        )
        .ok();
        let legacy = discover_powershell(
            |name| {
                if name == "powershell.exe" {
                    executable_on_path(name)
                } else {
                    None
                }
            },
            None,
            std::env::var_os("SystemRoot").map(PathBuf::from),
            Path::is_file,
        )
        .ok();
        assert!(
            modern.is_some() || legacy.is_some(),
            "native PowerShell must be available"
        );
        for shell in modern.into_iter().chain(legacy) {
            for (script, expected) in [
                (
                    "cmd /c \"echo progress 1>&2 & exit 0\" 2>&1 | Select-Object -Last 3",
                    0,
                ),
                ("cmd /c \"echo progress 1>&2 & exit 0\" 2>&1", 0),
                ("$output = cmd /c \"echo progress 1>&2 & exit 0\" 2>&1", 0),
                (
                    "Set-StrictMode -Version Latest; cmd /c \"echo progress 1>&2 & exit 0\" 2>&1",
                    0,
                ),
                (
                    "cmd /c \"echo progress 1>&2 & exit 7\" 2>&1 | Select-Object -Last 3",
                    1,
                ),
                ("cmd /c \"exit 3\"", 1),
                ("Get-Item missing-fixture-file", 1),
                ("Write-Error 'failed héllo €'", 1),
                ("throw 'failed héllo €'", 1),
                ("cmd /c \"exit 7\"; Write-Output done", 0),
                ("Write-Output done # comment", 0),
                ("exit 5", 5),
            ] {
                let output = shell
                    .command(script)
                    .unwrap()
                    .current_dir(fixture.path())
                    .stdin(std::process::Stdio::null())
                    .output()
                    .await
                    .unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(expected),
                    "{}: {script}",
                    shell.name()
                );
                if script.contains("héllo") {
                    assert!(
                        String::from_utf8(output.stderr)
                            .unwrap()
                            .contains("héllo €")
                    );
                }
            }
            for incomplete in [
                "Write-Output done `",
                "Get-ChildItem |",
                "$true -and",
                "Write-Output a,",
                "$x =",
                "Write-Output (1 +",
                "Write-Output \"unclosed",
                "if ($true) {",
            ] {
                let script = format!("Set-Content must-not-exist 'bad'; {incomplete}");
                let output = shell
                    .command(&script)
                    .unwrap()
                    .current_dir(fixture.path())
                    .stdin(std::process::Stdio::null())
                    .output()
                    .await
                    .unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(1),
                    "{}: {incomplete}",
                    shell.name()
                );
                assert!(!fixture.path().join("must-not-exist").exists());
            }
        }
    }
}
