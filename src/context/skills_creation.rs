//! Typed, local-only skill publication. No model or executable filesystem tools.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::{MAX_BODY, MAX_FRONTMATTER, valid_name, workspace};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillScope {
    Project,
    Global,
}

/// Validated at the UI/API boundary; its contents cannot select a filesystem path.
#[derive(Clone, Debug)]
pub struct SkillDraft {
    name: String,
    description: String,
    body: String,
    explicit_only: bool,
}

impl SkillDraft {
    pub fn new(
        name: String,
        description: String,
        body: String,
        explicit_only: bool,
    ) -> Result<Self, SkillCreateError> {
        if !valid_name(&name) || reserved_name(&name) {
            return Err(SkillCreateError::InvalidName);
        }
        if description.trim().is_empty() || description.chars().count() > 1024 {
            return Err(SkillCreateError::InvalidDescription);
        }
        if body.trim().is_empty() || body.len() > MAX_BODY {
            return Err(SkillCreateError::InvalidBody);
        }
        Ok(Self {
            name,
            description,
            body,
            explicit_only,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn markdown(&self) -> String {
        // JSON strings are YAML double-quoted scalars; delimiters/control characters cannot
        // inject extra frontmatter fields. Keep the actual body unchanged.
        let description = serde_json::to_string(&self.description).expect("string serialization");
        format!(
            "---\nname: {}\ndescription: {}\ndisable-model-invocation: {}\n---\n{}",
            self.name, description, self.explicit_only, self.body
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SkillCreateError {
    #[error(
        "Skill name must be 1–64 lowercase letters, digits or single internal hyphens, and portable across supported platforms"
    )]
    InvalidName,
    #[error("Skill description must contain 1–1024 characters")]
    InvalidDescription,
    #[error("Skill body must be nonempty and at most 1 MiB")]
    InvalidBody,
    #[error("Cannot resolve the skill destination")]
    DestinationUnavailable,
    #[error("Skill destination contains a link or has changed")]
    UnsafeDestination,
    #[error("A skill with this destination already exists; nothing was overwritten")]
    AlreadyExists,
    #[error("Cannot publish the skill")]
    PublicationFailed,
}

/// Bound to existing canonical workspace/configuration directories. Construct with the
/// runtime's mh `Layout::config_dir()`; no global Pi/Claude directories are consulted.
pub struct SkillStore {
    project: PathBuf,
    global: PathBuf,
}

impl SkillStore {
    pub fn new(cwd: &Path, global_config: &Path) -> Result<Self, SkillCreateError> {
        let (_, project) = workspace(cwd).map_err(|_| SkillCreateError::DestinationUnavailable)?;
        let global = std::fs::canonicalize(global_config)
            .map_err(|_| SkillCreateError::DestinationUnavailable)?;
        if !global.is_dir() {
            return Err(SkillCreateError::DestinationUnavailable);
        }
        Ok(Self { project, global })
    }

    /// Canonical destination root, without creating any directories.
    pub fn root(&self, scope: SkillScope) -> PathBuf {
        match scope {
            SkillScope::Project => self.project.join(".mh/skills"),
            SkillScope::Global => self.global.join("skills"),
        }
    }

    /// Reserve a new skill directory, fully write/sync a private staging file, then
    /// publish SKILL.md by a no-replace hard link. Existing files/links are never opened
    /// for writing. Descendant links are rejected and parents rechecked before each step.
    /// Like other portable std filesystem operations, this is not a sandbox against
    /// hostile concurrent directory replacement by another process.
    pub fn create(
        &self,
        scope: SkillScope,
        draft: &SkillDraft,
    ) -> Result<PathBuf, SkillCreateError> {
        let boundary = match scope {
            SkillScope::Project => &self.project,
            SkillScope::Global => &self.global,
        };
        verify_directory(boundary)?;
        let root = self.root(scope);
        let mut parent = boundary.clone();
        for component in root
            .strip_prefix(boundary)
            .expect("typed destination")
            .components()
        {
            verify_directory(&parent)?;
            parent.push(component);
            ensure_directory(&parent)?;
        }
        verify_chain(boundary, &root)?;
        let directory = root.join(&draft.name);
        match std::fs::create_dir(&directory) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(SkillCreateError::AlreadyExists);
            }
            Err(_) => return Err(SkillCreateError::PublicationFailed),
        }
        let destination = directory.join("SKILL.md");
        let temporary = directory.join(format!(".publish-{}.tmp", uuid::Uuid::new_v4()));
        let publish = || {
            verify_chain(boundary, &directory)?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|_| SkillCreateError::PublicationFailed)?;
            let content = draft.markdown();
            debug_assert!(content.len() <= MAX_BODY + MAX_FRONTMATTER);
            file.write_all(content.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|_| SkillCreateError::PublicationFailed)?;
            drop(file);
            verify_chain(boundary, &directory)?;
            std::fs::hard_link(&temporary, &destination).map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    SkillCreateError::AlreadyExists
                } else {
                    SkillCreateError::PublicationFailed
                }
            })?;
            Ok(destination.clone())
        };
        let result = publish();
        // Only remove our own staging name in the unchanged directory. Never recursively
        // remove user data; failed publication may leave an empty reserved directory.
        if verify_chain(boundary, &directory).is_ok() {
            let _ = std::fs::remove_file(&temporary);
            if result.is_err() {
                let _ = std::fs::remove_dir(&directory);
            }
        }
        result
    }
}

fn reserved_name(name: &str) -> bool {
    matches!(name, "con" | "prn" | "aux" | "nul")
        || (name.len() == 4
            && (name.starts_with("com") || name.starts_with("lpt"))
            && matches!(name.as_bytes()[3], b'1'..=b'9'))
}

fn verify_directory(path: &Path) -> Result<(), SkillCreateError> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| SkillCreateError::DestinationUnavailable)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || std::fs::canonicalize(path).map_err(|_| SkillCreateError::UnsafeDestination)? != path
    {
        return Err(SkillCreateError::UnsafeDestination);
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<(), SkillCreateError> {
    match std::fs::create_dir(path) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(_) => return Err(SkillCreateError::PublicationFailed),
    }
    verify_directory(path)
}

fn verify_chain(boundary: &Path, directory: &Path) -> Result<(), SkillCreateError> {
    verify_directory(boundary)?;
    let mut current = boundary.to_owned();
    for component in directory
        .strip_prefix(boundary)
        .map_err(|_| SkillCreateError::UnsafeDestination)?
        .components()
    {
        current.push(component);
        verify_directory(&current)?;
    }
    Ok(())
}
