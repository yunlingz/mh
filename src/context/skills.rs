//! Pi-compatible skill roots/frontmatter, with bounded mh workspace discovery.
//!
//! Adapted from Pi's skills.ts at 780bf31fcaeb252fa9b68df3964c735eae75bde3.
//! Copyright (c) 2025 Mario Zechner. MIT license; see the retained notice in LICENSE.
//! Workspace-wide fallback, precedence, budgets and link containment are mh adaptations.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::Deserialize;

use super::{ProjectSkills, Skill};

#[path = "skills_creation.rs"]
mod creation;
pub use creation::{SkillCreateError, SkillDraft, SkillScope, SkillStore};

const ROOTS: [&str; 5] = [".mh", ".pi", ".claude", ".agents", ".codex"];
const IGNORE_FILES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];
pub(super) const MAX_FRONTMATTER: usize = 64 * 1024;
pub(super) const MAX_BODY: usize = 1024 * 1024;

/// One shared budget across conventional, global and fallback skill discovery.
/// These are resource limits, not configuration feature switches.
#[derive(Clone, Copy, Debug)]
pub struct SkillLimits {
    pub max_entries: usize,
    pub max_depth: usize,
    pub max_read_bytes: usize,
    pub max_skills: usize,
    pub max_catalog_bytes: usize,
}

impl Default for SkillLimits {
    fn default() -> Self {
        Self {
            max_entries: 20_000,
            max_depth: 64,
            max_read_bytes: 4 * 1024 * 1024,
            max_skills: 256,
            max_catalog_bytes: 256 * 1024,
        }
    }
}

pub(super) fn workspace(cwd: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    let cwd = std::fs::canonicalize(cwd)?;
    if !cwd.is_dir() {
        return Err(std::io::ErrorKind::NotADirectory.into());
    }
    let boundary = cwd
        .ancestors()
        .find(|directory| {
            std::fs::symlink_metadata(directory.join(".git"))
                .is_ok_and(|metadata| metadata.is_dir() || metadata.is_file())
        })
        .unwrap_or(&cwd)
        .to_owned();
    Ok((cwd, boundary))
}

pub(super) fn discover(
    cwd: &Path,
    global: Option<&Path>,
    project: ProjectSkills,
    limits: SkillLimits,
) -> (Vec<Skill>, Vec<String>) {
    let mut scan = Scan::new(limits);
    let workspace = match workspace(cwd) {
        Ok(workspace) => Some(workspace),
        Err(_) => {
            scan.warn("Cannot resolve skill workspace");
            None
        }
    };
    if matches!(project, ProjectSkills::Load)
        && let Some((cwd, boundary)) = &workspace
    {
        // Nearest applicable ancestor first, then mh before imported roots.
        for directory in cwd
            .ancestors()
            .take_while(|path| path.starts_with(boundary))
        {
            for prefix in ROOTS {
                let root = directory.join(prefix).join("skills");
                scan.root(&root, boundary, true);
            }
        }
    }
    if matches!(project, ProjectSkills::Load)
        && let Some((_, boundary)) = &workspace
    {
        // Explicitly requested mh adaptation. Fallback is last within project scope;
        // every project definition still takes precedence over global definitions.
        scan.root(boundary, boundary, false);
    }
    // Only mh's supplied configuration root is global; never import another home tree.
    if let Some(global) = global
        && let Ok(config) = std::fs::canonicalize(global)
    {
        scan.scope = "global";
        scan.root(&config.join("skills"), &config, true);
    }
    (scan.skills.into_values().collect(), scan.warnings)
}

pub(super) fn project_skills_exist(cwd: &Path) -> bool {
    !discover(cwd, None, ProjectSkills::Load, SkillLimits::default())
        .0
        .is_empty()
}

struct Scan {
    limits: SkillLimits,
    entries: usize,
    read_bytes: usize,
    catalog_bytes: usize,
    // Traversal semantics differ: fallback visits hidden non-cache trees, while a
    // conventional root additionally accepts direct Markdown. Deduplicate each mode.
    seen_dirs: BTreeSet<(PathBuf, bool, bool)>,
    seen_files: BTreeSet<PathBuf>,
    ignores: BTreeMap<PathBuf, Gitignore>,
    skills: BTreeMap<String, Skill>,
    origins: BTreeMap<String, String>,
    scope: &'static str,
    warnings: Vec<String>,
}

impl Scan {
    fn new(limits: SkillLimits) -> Self {
        Self {
            limits,
            entries: 0,
            read_bytes: 0,
            catalog_bytes: 0,
            seen_dirs: BTreeSet::new(),
            seen_files: BTreeSet::new(),
            ignores: BTreeMap::new(),
            skills: BTreeMap::new(),
            origins: BTreeMap::new(),
            scope: "project",
            warnings: Vec::new(),
        }
    }

    fn warn(&mut self, message: &str) {
        // Machine-neutral, bounded diagnostics. Never expose parser/OS errors or file contents.
        if self.warnings.iter().any(|warning| warning == message) {
            return;
        }
        if self.warnings.len() < 64 {
            self.warnings.push(message.to_owned());
        } else if self.warnings.len() == 64 {
            self.warnings
                .push("Further skill diagnostics omitted".to_owned());
        }
    }

    fn entry(&mut self) -> bool {
        if self.entries >= self.limits.max_entries {
            self.warn("Skill traversal budget exhausted; discovery is incomplete");
            return false;
        }
        self.entries += 1;
        true
    }

    fn contained(&mut self, path: &Path, boundary: &Path) -> Option<PathBuf> {
        match std::fs::canonicalize(path) {
            Ok(identity) if identity.starts_with(boundary) => {
                // A contained link must not bypass excluded physical trees either.
                if identity
                    .strip_prefix(boundary)
                    .ok()?
                    .components()
                    .any(|part| excluded(&part.as_os_str().to_string_lossy()))
                {
                    return None;
                }
                Some(identity)
            }
            Ok(_) => {
                self.warn("Skill link escapes its allowed root; skipped");
                None
            }
            Err(_) => {
                self.warn("Cannot resolve skill resource; skipped");
                None
            }
        }
    }

    fn ignore(&mut self, directory: &Path, boundary: &Path) -> Gitignore {
        if let Some(matcher) = self.ignores.get(directory) {
            return matcher.clone();
        }
        let mut builder = GitignoreBuilder::new(directory);
        for name in IGNORE_FILES {
            let path = directory.join(name);
            if std::fs::symlink_metadata(&path).is_err() {
                continue;
            }
            let Some(identity) = self.contained(&path, boundary) else {
                return self.block_ignore(
                    directory,
                    "Cannot resolve skill ignore rules; affected tree skipped",
                );
            };
            if !identity.is_file() {
                return self.block_ignore(
                    directory,
                    "Skill ignore resource is not a regular file; affected tree skipped",
                );
            }
            let remaining = self.limits.max_read_bytes.saturating_sub(self.read_bytes);
            let cap = remaining.min(MAX_FRONTMATTER + 1);
            if cap == 0 {
                return self.block_ignore(
                    directory,
                    "Skill read budget exhausted; discovery is incomplete",
                );
            }
            let mut bytes = Vec::new();
            let result = crate::regular_file::open_read(&identity)
                .and_then(|file| file.take(cap as u64).read_to_end(&mut bytes));
            self.read_bytes += bytes.len();
            if result.is_err() {
                return self.block_ignore(
                    directory,
                    "Cannot read skill ignore rules; affected tree skipped",
                );
            }
            if bytes.len() == cap {
                if cap < MAX_FRONTMATTER + 1 {
                    self.warn("Skill read budget exhausted; discovery is incomplete");
                }
                return self.block_ignore(
                    directory,
                    "Skill ignore rules exceed read limit; affected tree skipped",
                );
            }
            let Ok(content) = std::str::from_utf8(&bytes) else {
                return self.block_ignore(
                    directory,
                    "Invalid skill ignore rules; affected tree skipped",
                );
            };
            for line in content.lines() {
                if builder.add_line(Some(path.clone()), line).is_err() {
                    return self.block_ignore(
                        directory,
                        "Invalid skill ignore pattern; affected tree skipped",
                    );
                }
            }
        }
        let matcher = match builder.build() {
            Ok(matcher) => matcher,
            Err(_) => {
                return self.block_ignore(
                    directory,
                    "Cannot compile skill ignore rules; affected tree skipped",
                );
            }
        };
        self.ignores.insert(directory.to_owned(), matcher.clone());
        matcher
    }

    fn block_ignore(&mut self, directory: &Path, message: &str) -> Gitignore {
        self.warn(message);
        let mut builder = GitignoreBuilder::new(directory);
        builder.add_line(None, "**").expect("fixed ignore pattern");
        let matcher = builder.build().expect("fixed ignore matcher");
        self.ignores.insert(directory.to_owned(), matcher.clone());
        matcher
    }

    fn canonical_ignored(&mut self, path: &Path, boundary: &Path, is_dir: bool) -> bool {
        let mut matchers = Vec::new();
        let mut parents = path
            .parent()
            .into_iter()
            .flat_map(Path::ancestors)
            .take_while(|parent| parent.starts_with(boundary))
            .collect::<Vec<_>>();
        parents.reverse();
        for directory in parents {
            if ignored(&matchers, directory, true) {
                return true;
            }
            matchers.push(self.ignore(directory, boundary));
        }
        ignored(&matchers, path, is_dir)
    }

    fn root(&mut self, root: &Path, boundary: &Path, conventional: bool) {
        if !self.entry() || std::fs::symlink_metadata(root).is_err() {
            return;
        }
        let mut matchers = Vec::new();
        let mut ancestors = root
            .ancestors()
            .take_while(|path| path.starts_with(boundary))
            .collect::<Vec<_>>();
        ancestors.reverse();
        for directory in ancestors {
            if ignored(&matchers, directory, true) {
                return;
            }
            matchers.push(self.ignore(directory, boundary));
        }
        self.walk(root, boundary, conventional, &matchers, 0, true);
    }

    fn walk(
        &mut self,
        directory: &Path,
        boundary: &Path,
        conventional: bool,
        matchers: &[Gitignore],
        depth: usize,
        is_root: bool,
    ) {
        if depth > self.limits.max_depth {
            self.warn("Skill depth limit reached; discovery is incomplete");
            return;
        }
        if self.entries >= self.limits.max_entries {
            self.warn("Skill traversal budget exhausted; discovery is incomplete");
            return;
        }
        let Some(identity) = self.contained(directory, boundary) else {
            return;
        };
        if identity != directory && self.canonical_ignored(&identity, boundary, true) {
            return;
        }
        if !self
            .seen_dirs
            .insert((identity, conventional, conventional && is_root))
        {
            return;
        }
        let mut matchers = matchers.to_vec();
        if !is_root {
            matchers.push(self.ignore(directory, boundary));
        }
        let declared = directory.join("SKILL.md");
        if declared.is_file() && !ignored(&matchers, &declared, false) {
            self.add(&declared, boundary);
            // Pi's root stopping applies even if a declared skill is invalid.
            return;
        }
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(_) => {
                self.warn("Cannot traverse skill directory; discovery is incomplete");
                return;
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            if !self.entry() {
                // Do not select an OS-order-dependent partial directory.
                return;
            }
            match entry {
                Ok(entry) => paths.push(entry.path()),
                Err(_) => self.warn("Cannot read skill directory entry; discovery is incomplete"),
            }
        }
        paths.sort();
        for path in paths {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if excluded(&name) || (conventional && name.starts_with('.')) {
                continue;
            }
            let is_dir = path.is_dir();
            if ignored(&matchers, &path, is_dir) {
                continue;
            }
            if is_dir {
                self.walk(&path, boundary, conventional, &matchers, depth + 1, false);
            } else if conventional
                && is_root
                && path.extension().is_some_and(|extension| extension == "md")
                && path.is_file()
            {
                self.add(&path, boundary);
            }
        }
    }

    fn add(&mut self, path: &Path, boundary: &Path) {
        let Some(identity) = self.contained(path, boundary) else {
            return;
        };
        if identity != path && self.canonical_ignored(&identity, boundary, false) {
            return;
        }
        if !identity.is_file() || !self.seen_files.insert(identity.clone()) {
            return;
        }
        if self.skills.len() >= self.limits.max_skills {
            self.warn("Skill count budget exhausted; catalog is incomplete");
            return;
        }
        let remaining = self.limits.max_read_bytes.saturating_sub(self.read_bytes);
        if remaining == 0 {
            self.warn("Skill read budget exhausted; catalog is incomplete");
            return;
        }
        let cap = remaining.min(MAX_FRONTMATTER + 1);
        let file = match crate::regular_file::open_read(&identity) {
            Ok(file) => file,
            Err(_) => {
                self.warn("Cannot read skill file");
                return;
            }
        };
        let mut reader = BufReader::with_capacity(1024, file.take(cap as u64));
        let front = read_frontmatter(&mut reader);
        self.read_bytes += cap - reader.get_ref().limit() as usize;
        let front = match front {
            Ok(front) => front,
            Err(message) => {
                if cap < MAX_FRONTMATTER + 1 && reader.get_ref().limit() == 0 {
                    self.warn("Skill read budget exhausted; catalog is incomplete");
                } else {
                    self.warn(message);
                }
                return;
            }
        };
        let Some(description) = front.description.filter(|text| !text.trim().is_empty()) else {
            self.warn("Skill description is missing");
            return;
        };
        let name = front
            .name
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| {
                path.parent()
                    .and_then(Path::file_name)
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        if !valid_name(&name) {
            self.warn("Skill name does not follow Agent Skills conventions");
        }
        if description.chars().count() > 1024 {
            self.warn("Skill description exceeds 1024 characters");
        }
        let origin = format!(
            "{}:{}",
            self.scope,
            diagnostic_text(
                &path
                    .strip_prefix(boundary)
                    .unwrap_or(path)
                    .to_string_lossy()
            )
        );
        if let Some(winner) = self.origins.get(&name) {
            let message = format!(
                "Duplicate skill name '{}': kept {}; skipped {}",
                diagnostic_text(&name),
                winner,
                origin
            );
            self.warn(&message);
            return;
        }
        // Conservative bound includes worst-case XML escaping of metadata and location.
        let size = name
            .len()
            .saturating_add(description.len())
            .saturating_add(identity.as_os_str().len())
            .saturating_mul(6)
            .saturating_add(128);
        if size
            > self
                .limits
                .max_catalog_bytes
                .saturating_sub(self.catalog_bytes)
        {
            self.warn("Skill catalog byte budget exhausted; catalog is incomplete");
            return;
        }
        self.catalog_bytes += size;
        self.origins.insert(name.clone(), origin);
        self.skills.insert(
            name.clone(),
            Skill {
                name,
                description,
                path: identity,
                disabled_model_invocation: front.disabled,
            },
        );
    }
}

fn diagnostic_text(text: &str) -> String {
    text.chars()
        .take(160)
        .map(|ch| if ch.is_control() { '�' } else { ch })
        .collect()
}

fn ignored(matchers: &[Gitignore], path: &Path, is_dir: bool) -> bool {
    for matcher in matchers.iter().rev() {
        let result = matcher.matched_path_or_any_parents(path, is_dir);
        if !result.is_none() {
            return result.is_ignore();
        }
    }
    false
}

fn excluded(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".hg"
            | ".svn"
            | "node_modules"
            | "vendor"
            | "target"
            | "build"
            | "dist"
            | "coverage"
            | "__pycache__"
            | ".cache"
            | ".venv"
            | "venv"
            | ".tox"
            | ".mypy_cache"
            | ".pytest_cache"
            | ".ruff_cache"
            | ".next"
            | ".nuxt"
            | ".gradle"
            | ".cargo"
    )
}

pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .bytes()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'-')
}

#[derive(Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(default, rename = "disable-model-invocation")]
    disabled: bool,
}

fn read_frontmatter(reader: &mut impl BufRead) -> Result<Frontmatter, &'static str> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|_| "Cannot read skill file")?;
    if line.trim_start_matches('\u{feff}').trim() != "---" {
        return Err("Skill is missing YAML frontmatter");
    }
    let mut yaml = String::new();
    let mut total = line.len();
    loop {
        line.clear();
        let count = reader
            .read_line(&mut line)
            .map_err(|_| "Cannot read skill file")?;
        total += count;
        if total > MAX_FRONTMATTER {
            return Err("Skill frontmatter exceeds 64 KiB");
        }
        if count == 0 {
            return Err("Unclosed skill frontmatter");
        }
        if line.trim() == "---" {
            break;
        }
        yaml.push_str(&line);
    }
    let mut options = serde_saphyr::Options::default();
    options.strict_booleans = true;
    serde_saphyr::from_str_with_options(&yaml, options).map_err(|_| "Invalid skill frontmatter")
}

pub(super) fn body(content: &str) -> Option<&str> {
    let content = content.trim_start_matches('\u{feff}');
    let mut lines = content.split_inclusive('\n');
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut offset = content.find('\n').map_or(content.len(), |index| index + 1);
    for line in lines {
        offset += line.len();
        if offset > MAX_FRONTMATTER {
            return None;
        }
        if line.trim() == "---" {
            return Some(content[offset..].trim_start_matches(['\r', '\n']));
        }
    }
    None
}

pub(super) fn read_body(path: &Path) -> Result<String, &'static str> {
    // The registry stores canonical paths. Reject a replaced directory/file link on lazy reads.
    let identity = std::fs::canonicalize(path).map_err(|_| "Cannot read skill instructions")?;
    if identity != path || !identity.is_file() {
        return Err("Skill resource changed; refresh resources before invocation");
    }
    let mut content = String::new();
    crate::regular_file::open_read(&identity)
        .and_then(|file| {
            file.take((MAX_BODY + MAX_FRONTMATTER + 1) as u64)
                .read_to_string(&mut content)
        })
        .map_err(|_| "Cannot read skill instructions")?;
    if content.len() > MAX_BODY + MAX_FRONTMATTER {
        return Err("Skill instructions exceed read limit");
    }
    let body = body(&content).ok_or("Invalid skill instructions")?;
    if body.len() > MAX_BODY {
        return Err("Skill body exceeds read limit");
    }
    Ok(body.to_owned())
}
