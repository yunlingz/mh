//! Native path resolution with Pi's home, @, and screenshot-path conventions.

use std::path::{Component, Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

pub fn resolve(path: &str, cwd: &Path) -> PathBuf {
    let path = path.strip_prefix('@').unwrap_or(path);
    let normalized = path
        .chars()
        .map(|character| {
            if matches!(
                character,
                '\u{a0}' | '\u{1680}' | '\u{2000}'
                    ..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
            ) {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    #[cfg(windows)]
    let normalized = shell_path_to_windows(&normalized).unwrap_or(normalized);
    let path = if normalized.starts_with("file://") {
        url::Url::parse(&normalized)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .unwrap_or_else(|| PathBuf::from(&normalized))
    } else if normalized == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"))
    } else if let Some(suffix) = normalized
        .strip_prefix("~/")
        .or_else(|| normalized.strip_prefix("~\\"))
    {
        dirs::home_dir().map_or_else(|| PathBuf::from(&normalized), |home| home.join(suffix))
    } else {
        PathBuf::from(normalized)
    };
    let absolute = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            component => resolved.push(component.as_os_str()),
        }
    }
    resolved
}

#[cfg(any(windows, test))]
fn shell_path_to_windows(path: &str) -> Option<String> {
    let without_root = path.strip_prefix('/')?;
    let (drive, suffix) = if let Some(rest) = without_root.strip_prefix("mnt/") {
        rest.split_once('/').unwrap_or((rest, ""))
    } else if let Some(rest) = without_root.strip_prefix("cygdrive/") {
        rest.split_once('/').unwrap_or((rest, ""))
    } else {
        without_root.split_once('/').unwrap_or((without_root, ""))
    };
    if drive.len() != 1 || !drive.as_bytes()[0].is_ascii_alphabetic() {
        return None;
    }
    let drive = (drive.as_bytes()[0] as char).to_ascii_uppercase();
    let suffix = suffix.replace('/', "\\");
    Some(if suffix.is_empty() {
        format!("{drive}:\\")
    } else {
        format!("{drive}:\\{suffix}")
    })
}

fn normalize_ampm_space(path: &str) -> String {
    let mut normalized = path.to_owned();
    for suffix in ["AM.", "Am.", "aM.", "am.", "PM.", "Pm.", "pM.", "pm."] {
        normalized = normalized.replace(&format!(" {suffix}"), &format!("\u{202f}{suffix}"));
    }
    normalized
}

pub fn resolve_read(path: &str, cwd: &Path) -> PathBuf {
    let resolved = resolve(path, cwd);
    if resolved.exists() {
        return resolved;
    }
    let text = resolved.to_string_lossy();
    let nfd: String = text.nfd().collect();
    for variant in [
        normalize_ampm_space(&text),
        nfd.clone(),
        text.replace('\'', "\u{2019}"),
        nfd.replace('\'', "\u{2019}"),
    ] {
        let path = PathBuf::from(variant);
        if path.exists() {
            return path;
        }
    }
    resolved
}

pub fn io_code(error: &std::io::Error) -> String {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::NotFound => "ENOENT".into(),
        ErrorKind::PermissionDenied => "EACCES".into(),
        ErrorKind::AlreadyExists => "EEXIST".into(),
        ErrorKind::NotADirectory => "ENOTDIR".into(),
        ErrorKind::IsADirectory => "EISDIR".into(),
        _ => format!("OS error {}", error.raw_os_error().unwrap_or(0)),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{normalize_ampm_space, resolve, shell_path_to_windows};

    #[test]
    fn shell_paths_map_to_native_windows_drives() {
        assert_eq!(
            shell_path_to_windows("/c/work/file"),
            Some("C:\\work\\file".into())
        );
        assert_eq!(
            shell_path_to_windows("/mnt/d/work"),
            Some("D:\\work".into())
        );
        assert_eq!(shell_path_to_windows("/cygdrive/e"), Some("E:\\".into()));
        assert_eq!(shell_path_to_windows("/tmp/work"), None);
    }

    #[test]
    fn file_urls_unicode_spaces_and_ampm_variants_are_normalized() {
        #[cfg(unix)]
        assert_eq!(
            resolve("file:///tmp/example", Path::new("/work")),
            Path::new("/tmp/example")
        );
        assert_eq!(
            resolve("wide\u{3000}space", Path::new("base")),
            Path::new("base/wide space")
        );
        assert_eq!(
            normalize_ampm_space("Shot 1 pm.png"),
            "Shot 1\u{202f}pm.png"
        );
    }
}
