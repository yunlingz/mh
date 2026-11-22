//! Regular-file handles. Opening never truncates, and FIFO races cannot block on Unix.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

#[derive(Clone, Copy)]
enum Access {
    Read,
    Edit,
    Write,
}

pub(super) fn open_read(path: &Path) -> io::Result<File> {
    open(path, Access::Read)
}

pub(super) fn open_edit(path: &Path) -> io::Result<File> {
    open(path, Access::Edit)
}

pub(super) fn open_write(path: &Path) -> io::Result<File> {
    open(path, Access::Write)
}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "Only regular files are supported",
    )
}

fn preflight(path: &Path, access: Access) -> io::Result<()> {
    #[cfg(windows)]
    if !windows_file_path(&path.to_string_lossy()) {
        return Err(unsupported());
    }
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(unsupported()),
        Err(error)
            if matches!(access, Access::Write) && error.kind() == io::ErrorKind::NotFound =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn open(path: &Path, access: Access) -> io::Result<File> {
    preflight(path, access)?;
    open_handle(path, access)
}

fn open_handle(path: &Path, access: Access) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(matches!(access, Access::Read | Access::Edit))
        .write(matches!(access, Access::Edit | Access::Write))
        .create(matches!(access, Access::Write));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // The path can change after preflight. FIFO opens must not wait for a
        // peer, and opening a raced terminal must not acquire a controlling TTY.
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(unsupported());
    }
    Ok(file)
}

#[cfg(any(windows, test))]
fn windows_file_path(value: &str) -> bool {
    let normalized = value.replace('/', "\\");
    let mut path = normalized.as_str();
    if path.starts_with(r"\\.\") || path.starts_with(r"\??\") {
        return false;
    }
    if let Some(verbatim) = path.strip_prefix(r"\\?\") {
        if verbatim
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(r"UNC\"))
        {
            let Some(body) = windows_unc_body(&verbatim[4..]) else {
                return false;
            };
            path = body;
        } else if verbatim.as_bytes().get(1) == Some(&b':')
            && verbatim
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic)
            && verbatim.as_bytes().get(2) == Some(&b'\\')
        {
            path = verbatim;
        } else {
            // In particular, GLOBALROOT/Device/NamedPipe and device aliases are
            // not ordinary verbatim disk or UNC file paths.
            return false;
        }
    } else if let Some(unc) = path.strip_prefix(r"\\") {
        let Some(body) = windows_unc_body(unc) else {
            return false;
        };
        path = body;
    }
    !path.split('\\').any(|component| {
        let stem = component
            .split(['.', ':'])
            .next()
            .unwrap_or("")
            .trim_end_matches(' ')
            .to_ascii_uppercase();
        matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                (suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'))
                    || matches!(suffix, "¹" | "²" | "³")
            })
    })
}

#[cfg(any(windows, test))]
fn windows_unc_body(unc: &str) -> Option<&str> {
    let mut parts = unc.splitn(3, '\\');
    let server = parts.next()?;
    let share = parts.next()?;
    if server.is_empty() || share.is_empty() || share.eq_ignore_ascii_case("pipe") {
        return None;
    }
    // Server/share names are namespace components, not DOS device basenames.
    Some(parts.next().unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_device_and_pipe_paths_are_not_regular_file_inputs() {
        for path in [
            r"\\.\pipe\fixture",
            r"\\?\GLOBALROOT\Device\NamedPipe\fixture",
            r"\\server\pipe\fixture",
            r"\\?\UNC\server\PIPE\fixture",
            r"C:\work\NUL",
            r"C:\work\con.txt",
            r"C:\work\COM1 .txt",
            r"C:\work\LPT²",
            r"C:\work\CONOUT$",
            r"\??\pipe\fixture",
        ] {
            assert!(!windows_file_path(path), "{path}");
        }
        for path in [
            r"C:\work\file.txt",
            r"\\?\C:\work\file.txt",
            r"\\server\share\file",
            r"\\?\UNC\server\share\file",
            r"C:\work\console.txt",
            r"\\con\PRN\file.txt",
            r"\\?\UNC\aux\NUL\file.txt",
        ] {
            assert!(windows_file_path(path), "{path}");
        }
    }

    #[test]
    fn opening_for_writes_does_not_truncate_and_edits_keep_the_verified_handle() {
        use std::io::Write;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file");
        std::fs::write(&path, "original").unwrap();
        drop(open_write(&path).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        let mut edit = open_edit(&path).unwrap();
        let moved = temp.path().join("moved");
        std::fs::rename(&path, &moved).unwrap();
        std::fs::write(&path, "unrelated replacement").unwrap();
        edit.set_len(0).unwrap();
        edit.write_all(b"changed").unwrap();
        assert_eq!(std::fs::read_to_string(&moved).unwrap(), "changed");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "unrelated replacement"
        );
    }

    #[cfg(unix)]
    #[test]
    fn regular_path_replaced_with_fifo_before_open_is_rejected_without_waiting() {
        use std::os::unix::fs::FileTypeExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("raced");
        std::fs::write(&path, "unchanged").unwrap();
        preflight(&path, Access::Read).unwrap();
        std::fs::remove_file(&path).unwrap();
        // mkfifo is invoked only for this controlled temporary fixture; the
        // package forbids unsafe code, so use the platform utility here.
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(std::fs::metadata(&path).unwrap().file_type().is_fifo());
        for access in [Access::Read, Access::Edit, Access::Write] {
            let opened_path = path.clone();
            let (sender, receiver) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                sender
                    .send(open_handle(&opened_path, access).is_err())
                    .unwrap();
            });
            match receiver.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(rejected) => assert!(rejected),
                Err(_) => {
                    // Release a faulty blocking open before failing the fixture.
                    let peer = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&path)
                        .unwrap();
                    let _ = receiver.recv_timeout(std::time::Duration::from_secs(2));
                    drop(peer);
                    worker.join().unwrap();
                    panic!("opening a raced FIFO blocked");
                }
            }
            worker.join().unwrap();
        }
    }
}
