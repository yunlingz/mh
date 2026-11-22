//! Private state paths. Every session owns its transcript and output files.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn user() -> Result<Self, String> {
        #[cfg(windows)]
        let home = std::env::var_os("USERPROFILE")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .or_else(dirs::home_dir);
        #[cfg(not(windows))]
        let home = dirs::home_dir();
        let home = home.ok_or("Cannot locate the user home directory")?;
        Ok(Self {
            root: home.join(".mh"),
        })
    }

    pub fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir().join("config.json")
    }

    pub fn theme_file(&self) -> PathBuf {
        self.config_dir().join("theme.json")
    }

    pub fn config_schema_file(&self) -> PathBuf {
        self.config_dir().join("config.schema.json")
    }

    pub fn theme_schema_file(&self) -> PathBuf {
        self.config_dir().join("theme.schema.json")
    }

    pub fn workspace_dir(&self, cwd: &Path) -> io::Result<PathBuf> {
        let canonical = std::fs::canonicalize(cwd)?;
        #[cfg(unix)]
        let identity = {
            use std::os::unix::ffi::OsStrExt;
            canonical.as_os_str().as_bytes().to_vec()
        };
        #[cfg(windows)]
        let identity = {
            use std::os::windows::ffi::OsStrExt;
            canonical
                .as_os_str()
                .encode_wide()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        };
        let hash = format!("{:x}", Sha256::digest(&identity));
        Ok(self.root.join("sessions").join(hash))
    }
}

pub fn private_dir(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

pub fn new_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub fn write_new(path: &Path, content: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    let mut file = new_private_file(path)?;
    file.write_all(content)?;
    file.sync_all()
}

pub fn output_name(tool_call_id: &str) -> String {
    // Always hash the complete, case-sensitive ID: even ASCII IDs can exceed
    // filename limits, name Windows devices, or collide on case-insensitive disks.
    format!("output_{:x}.log", Sha256::digest(tool_call_id.as_bytes()))
}

pub fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
