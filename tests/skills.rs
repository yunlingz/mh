use std::path::{Path, PathBuf};

use mh::context::{
    ProjectSkills, Resources, SkillCreateError, SkillDraft, SkillLimits, SkillScope, SkillStore,
};

struct Fixture {
    _temp: tempfile::TempDir,
    #[cfg(unix)]
    outside: PathBuf,
    project: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new(repository: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let config = temp.path().join("mh/config");
        let outside = temp.path().join("outside");
        for directory in [&project, &config, &outside] {
            std::fs::create_dir_all(directory).unwrap();
        }
        if repository {
            std::fs::create_dir(project.join(".git")).unwrap();
        }
        Self {
            _temp: temp,
            #[cfg(unix)]
            outside,
            project,
            config,
        }
    }

    fn load(&self, cwd: &Path) -> Resources {
        Resources::load(cwd, &self.config, ProjectSkills::Load)
    }

    fn limits(&self, limits: SkillLimits) -> Resources {
        Resources::load_with_skill_limits(&self.project, &self.config, ProjectSkills::Load, limits)
    }
}

fn file(path: impl AsRef<Path>, content: &str) {
    let path = path.as_ref();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn skill(root: impl AsRef<Path>, name: &str, description: &str) {
    file(
        root.as_ref().join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: {description}\n---\nbody"),
    );
}

fn names(resources: &Resources) -> Vec<&str> {
    resources
        .skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect()
}

fn warned(resources: &Resources, text: &str) -> bool {
    resources
        .warnings
        .iter()
        .any(|warning| warning.contains(text))
}

#[test]
fn conventional_roots_are_nearest_then_mh_then_imported_then_fallback_then_global() {
    let fixture = Fixture::new(true);
    let child = fixture.project.join("child");
    std::fs::create_dir_all(&child).unwrap();
    skill(
        fixture.project.join(".mh/skills/collision"),
        "collision",
        "ancestor",
    );
    skill(
        child.join(".pi/skills/collision"),
        "collision",
        "nearest-imported",
    );
    skill(
        child.join(".mh/skills/collision"),
        "collision",
        "nearest-mh",
    );
    skill(
        fixture.config.join("skills/collision"),
        "collision",
        "global",
    );
    skill(
        fixture.project.join("workflows/collision"),
        "collision",
        "fallback",
    );
    for prefix in [".pi", ".claude", ".agents", ".codex"] {
        let name = prefix.trim_start_matches('.');
        skill(
            child.join(prefix).join("skills").join(name),
            name,
            "imported",
        );
    }
    skill(
        fixture.config.join("skills/global-only"),
        "global-only",
        "global-loser",
    );
    skill(
        fixture.project.join("misc/global-only"),
        "global-only",
        "fallback-winner",
    );
    let resources = fixture.load(&child);
    assert_eq!(
        names(&resources),
        [
            "agents",
            "claude",
            "codex",
            "collision",
            "global-only",
            "pi"
        ]
    );
    assert_eq!(
        resources
            .skills
            .iter()
            .find(|skill| skill.name == "collision")
            .unwrap()
            .description,
        "nearest-mh"
    );
    assert_eq!(
        resources
            .skills
            .iter()
            .find(|skill| skill.name == "global-only")
            .unwrap()
            .description,
        "fallback-winner"
    );
    assert!(warned(&resources, "Duplicate skill name"));
}

#[test]
fn repository_boundary_supports_worktree_git_file_and_cwd_fallback() {
    let fixture = Fixture::new(false);
    file(fixture.project.join(".git"), "gitdir: fixture-only");
    skill(
        fixture._temp.path().join(".mh/skills/outside-ancestor"),
        "outside-ancestor",
        "not applicable",
    );
    let child = fixture.project.join("child");
    std::fs::create_dir_all(&child).unwrap();
    skill(
        fixture.project.join(".agents/skills/ancestor"),
        "ancestor",
        "applicable",
    );
    assert_eq!(names(&fixture.load(&child)), ["ancestor"]);
    std::fs::remove_file(fixture.project.join(".git")).unwrap();
    assert!(fixture.load(&child).skills.is_empty());
}

#[test]
fn skip_project_skills_retains_only_mh_global() {
    let fixture = Fixture::new(true);
    skill(
        fixture.project.join(".claude/skills/project"),
        "project",
        "project",
    );
    skill(
        fixture.project.join("misc/fallback"),
        "fallback",
        "fallback",
    );
    skill(fixture.config.join("skills/global"), "global", "global");
    skill(
        fixture
            .config
            .parent()
            .unwrap()
            .join(".pi/skills/imported-global"),
        "imported-global",
        "not global",
    );
    let resources = Resources::load(&fixture.project, &fixture.config, ProjectSkills::Skip);
    assert_eq!(names(&resources), ["global"]);
    assert!(Resources::project_skills_exist(&fixture.project));
}

#[test]
fn fallback_finds_nonstandard_including_hidden_dirs_and_excludes_generated_trees() {
    let fixture = Fixture::new(true);
    skill(
        fixture.project.join("workflows/release"),
        "release",
        "fallback",
    );
    skill(
        fixture.project.join(".workflow/review"),
        "review",
        "hidden fallback",
    );
    for tree in [
        ".git",
        "node_modules",
        "vendor",
        "target",
        "build",
        "dist",
        ".cache",
        "__pycache__",
        ".venv",
    ] {
        skill(
            fixture.project.join(tree).join("excluded"),
            "excluded",
            "not loaded",
        );
    }
    file(
        fixture.project.join("README.md"),
        "---\nname: readme\ndescription: not a fallback skill\n---\nbody",
    );
    assert_eq!(
        names(&fixture.load(&fixture.project)),
        ["release", "review"]
    );
}

#[test]
fn inherited_ignore_files_apply_to_conventional_roots_and_fallback_with_negation() {
    let fixture = Fixture::new(true);
    file(
        fixture.project.join(".gitignore"),
        ".pi/skills/ignored/\n.mh/skills/*\n!.mh/skills/keep/\nworkflows/ignored/\n",
    );
    file(fixture.project.join(".ignore"), "other-ignored/\n");
    file(fixture.project.join(".fdignore"), "fd-ignored/\n");
    for (path, name) in [
        (".pi/skills/ignored", "ignored-pi"),
        (".mh/skills/drop", "drop"),
        (".mh/skills/keep", "keep"),
        ("workflows/ignored", "ignored-fallback"),
        ("other-ignored", "ignored-other"),
        ("fd-ignored", "ignored-fd"),
        ("workflows/nested/drop", "nested-drop"),
        ("workflows/nested/keep", "nested-keep"),
    ] {
        skill(fixture.project.join(path), name, "workflow");
    }
    file(
        fixture.project.join("workflows/.ignore"),
        "nested/*\n!nested/keep/\n",
    );
    assert_eq!(
        names(&fixture.load(&fixture.project)),
        ["keep", "nested-keep"]
    );
}

#[test]
fn root_stopping_and_standalone_conventional_markdown_follow_pi() {
    let fixture = Fixture::new(true);
    skill(
        fixture.project.join(".mh/skills/root"),
        "root",
        "root stops recursion",
    );
    skill(
        fixture.project.join(".mh/skills/root/nested"),
        "nested",
        "not loaded",
    );
    file(
        fixture.project.join(".pi/skills/direct.md"),
        "---\nname: direct\ndescription: direct markdown\n---\nbody",
    );
    file(
        fixture.project.join(".pi/skills/nested/not-skill.md"),
        "---\nname: not-skill\ndescription: not direct\n---\nbody",
    );
    file(fixture.project.join("workflows/bad/SKILL.md"), "invalid");
    skill(
        fixture.project.join("workflows/bad/nested"),
        "invalid-nested",
        "not loaded",
    );
    assert_eq!(names(&fixture.load(&fixture.project)), ["direct", "root"]);
}

#[test]
fn traversal_depth_count_and_catalog_bytes_are_bounded_and_deterministic() {
    let fixture = Fixture::new(true);
    skill(fixture.project.join(".mh/skills/a"), "a", "first");
    skill(fixture.project.join(".mh/skills/b"), "b", "second");
    skill(fixture.project.join("deep/deeper/last"), "last", "deep");
    let resources = fixture.limits(SkillLimits {
        max_skills: 1,
        ..SkillLimits::default()
    });
    assert_eq!(names(&resources), ["a"]);
    assert!(warned(&resources, "count budget"));
    let resources = fixture.limits(SkillLimits {
        max_catalog_bytes: 1,
        ..SkillLimits::default()
    });
    assert!(resources.skills.is_empty());
    assert!(warned(&resources, "catalog byte budget"));
    let resources = fixture.limits(SkillLimits {
        max_depth: 0,
        ..SkillLimits::default()
    });
    assert!(resources.skills.is_empty());
    assert!(warned(&resources, "depth limit"));
    let limits = SkillLimits {
        max_entries: 1,
        ..SkillLimits::default()
    };
    let resources = fixture.limits(limits);
    assert!(resources.skills.is_empty());
    assert!(warned(&resources, "traversal budget"));
    assert_eq!(resources.warnings, fixture.limits(limits).warnings);
}

#[test]
fn read_budget_is_shared_across_roots_and_ignore_files_and_bodies_are_lazy() {
    let fixture = Fixture::new(true);
    file(
        fixture.project.join(".gitignore"),
        &format!("#{}\n", "x".repeat(20)),
    );
    let content = format!(
        "---\nname: a\ndescription: first\n---\n{}",
        "x".repeat(2 * 1024 * 1024)
    );
    file(fixture.project.join(".mh/skills/a/SKILL.md"), &content);
    skill(fixture.project.join(".pi/skills/b"), "b", "second");
    let resources = fixture.limits(SkillLimits {
        max_read_bytes: 1050,
        ..SkillLimits::default()
    });
    assert_eq!(names(&resources), ["a"]);
    assert!(warned(&resources, "read budget"));
    assert!(!resources.prompt(&[]).contains(&"x".repeat(100)));
    assert!(
        resources
            .skill_request("a", "")
            .unwrap_err()
            .contains("read limit")
    );
}

#[test]
fn frontmatter_limits_types_unicode_bom_and_explicit_only_are_preserved() {
    let fixture = Fixture::new(true);
    file(
        fixture.project.join(".mh/skills/explicit/SKILL.md"),
        "\u{feff}---\r\nname: explicit\r\ndescription: |\r\n  A multiline\r\n  description\r\ndisable-model-invocation: true\r\nunknown: allowed\r\n---\r\nEXPLICIT_BODY",
    );
    file(
        fixture.project.join(".mh/skills/badbool/SKILL.md"),
        "---\nname: badbool\ndescription: workflow\ndisable-model-invocation: yes\n---\nbody",
    );
    file(
        fixture.project.join(".mh/skills/oversized/SKILL.md"),
        &format!(
            "---\nname: oversized\ndescription: {}\n---\nbody",
            "x".repeat(70 * 1024)
        ),
    );
    let resources = fixture.load(&fixture.project);
    assert_eq!(names(&resources), ["explicit"]);
    assert!(warned(&resources, "Invalid skill frontmatter"));
    assert!(warned(&resources, "64 KiB"));
    assert!(!resources.prompt(&[]).contains("multiline"));
    assert!(
        resources
            .skill_request("explicit", "argument")
            .unwrap()
            .ends_with("argument")
    );
    assert!(
        resources
            .skill_request("explicit", "")
            .unwrap()
            .contains("EXPLICIT_BODY")
    );
}

fn draft(name: &str, explicit_only: bool) -> SkillDraft {
    SkillDraft::new(
        name.into(),
        "A local workflow".into(),
        "Preserve this body.\n".into(),
        explicit_only,
    )
    .unwrap()
}

#[test]
fn creation_uses_canonical_project_and_global_destinations_without_overwrite() {
    let fixture = Fixture::new(true);
    let child = fixture.project.join("child");
    std::fs::create_dir_all(&child).unwrap();
    let store = SkillStore::new(&child, &fixture.config).unwrap();
    let project = store
        .create(SkillScope::Project, &draft("local", true))
        .unwrap();
    assert_eq!(
        project,
        std::fs::canonicalize(&fixture.project)
            .unwrap()
            .join(".mh/skills/local/SKILL.md")
    );
    let global = store
        .create(SkillScope::Global, &draft("global", false))
        .unwrap();
    assert_eq!(
        global,
        std::fs::canonicalize(&fixture.config)
            .unwrap()
            .join("skills/global/SKILL.md")
    );
    let original = std::fs::read(&project).unwrap();
    assert_eq!(
        store.create(SkillScope::Project, &draft("local", false)),
        Err(SkillCreateError::AlreadyExists)
    );
    assert_eq!(std::fs::read(&project).unwrap(), original);
    assert_eq!(
        std::fs::read_dir(project.parent().unwrap())
            .unwrap()
            .count(),
        1
    );
    let resources = fixture.load(&child);
    assert_eq!(names(&resources), ["global", "local"]);
    assert!(!resources.prompt(&[]).contains("<name>local</name>"));
    assert!(
        resources
            .skill_request("local", "")
            .unwrap()
            .contains("Preserve this body.\n")
    );
}

#[test]
fn creation_without_repository_is_cwd_local_and_yaml_metadata_cannot_inject_fields() {
    let fixture = Fixture::new(false);
    let child = fixture.project.join("child");
    std::fs::create_dir_all(&child).unwrap();
    let store = SkillStore::new(&child, &fixture.config).unwrap();
    let description =
        "Quoted \"text\"\n---\nname: injected\ndisable-model-invocation: true\nUnicode: 你好";
    let draft =
        SkillDraft::new("safe".into(), description.into(), "BODY\r\n".into(), false).unwrap();
    assert_eq!(draft.name(), "safe");
    let path = store.create(SkillScope::Project, &draft).unwrap();
    assert_eq!(
        path,
        std::fs::canonicalize(&child)
            .unwrap()
            .join(".mh/skills/safe/SKILL.md")
    );
    let resources = fixture.load(&child);
    assert_eq!(resources.skills[0].description, description);
    assert!(!resources.skills[0].disabled_model_invocation);
    assert!(
        resources
            .skill_request("safe", "")
            .unwrap()
            .contains("BODY\r\n")
    );
}

#[test]
fn drafts_reject_traversal_invalid_names_reserved_devices_empty_and_oversized_content() {
    for name in [
        "",
        ".",
        "..",
        "../escape",
        "/absolute",
        "a/b",
        "a\\b",
        "UPPER",
        "a--b",
        "-a",
        "a-",
        "con",
        "nul",
        "com1",
        "lpt9",
    ] {
        assert!(matches!(
            SkillDraft::new(name.into(), "description".into(), "body".into(), false),
            Err(SkillCreateError::InvalidName)
        ));
    }
    assert!(matches!(
        SkillDraft::new("a".repeat(65), "description".into(), "body".into(), false),
        Err(SkillCreateError::InvalidName)
    ));
    for description in [" ".into(), "x".repeat(1025)] {
        assert!(matches!(
            SkillDraft::new("name".into(), description, "body".into(), false),
            Err(SkillCreateError::InvalidDescription)
        ));
    }
    for body in [" \n".into(), "x".repeat(1024 * 1024 + 1)] {
        assert!(matches!(
            SkillDraft::new("name".into(), "description".into(), body, false),
            Err(SkillCreateError::InvalidBody)
        ));
    }
}

#[cfg(unix)]
#[test]
fn discovery_deduplicates_contained_links_skips_cycles_escapes_and_physical_dependencies() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new(true);
    skill(fixture.project.join("workflows/source"), "linked", "source");
    std::fs::create_dir_all(fixture.project.join(".mh/skills")).unwrap();
    symlink(
        fixture.project.join("workflows/source"),
        fixture.project.join(".mh/skills/alias"),
    )
    .unwrap();
    symlink(&fixture.project, fixture.project.join("workflows/cycle")).unwrap();
    skill(fixture.outside.join("escape"), "escape", "outside");
    symlink(&fixture.outside, fixture.project.join("workflows/escape")).unwrap();
    skill(
        fixture.project.join("node_modules/dependency"),
        "dependency",
        "excluded",
    );
    symlink(
        fixture.project.join("node_modules/dependency"),
        fixture.project.join("workflows/dependency-alias"),
    )
    .unwrap();
    let resources = fixture.load(&fixture.project);
    assert_eq!(names(&resources), ["linked"]);
    assert!(warned(&resources, "escapes"));
    assert!(!warned(&resources, "Duplicate"));
    assert_eq!(
        resources.skills[0].path,
        std::fs::canonicalize(fixture.project.join("workflows/source/SKILL.md")).unwrap()
    );
}

#[cfg(unix)]
#[test]
fn creation_rejects_symlink_components_and_existing_leaf_links_without_touching_targets() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new(true);
    let store = SkillStore::new(&fixture.project, &fixture.config).unwrap();
    symlink(&fixture.outside, fixture.project.join(".mh")).unwrap();
    assert_eq!(
        store.create(SkillScope::Project, &draft("escape", false)),
        Err(SkillCreateError::UnsafeDestination)
    );
    assert!(!fixture.outside.join("skills").exists());
    std::fs::remove_file(fixture.project.join(".mh")).unwrap();
    std::fs::create_dir_all(fixture.project.join(".mh/skills")).unwrap();
    symlink(&fixture.outside, fixture.project.join(".mh/skills/link")).unwrap();
    assert_eq!(
        store.create(SkillScope::Project, &draft("link", false)),
        Err(SkillCreateError::AlreadyExists)
    );
    assert!(!fixture.outside.join("SKILL.md").exists());
    symlink(&fixture.outside, fixture.config.join("skills")).unwrap();
    assert_eq!(
        store.create(SkillScope::Global, &draft("escape", false)),
        Err(SkillCreateError::UnsafeDestination)
    );
}

#[cfg(unix)]
#[test]
fn lazy_invocation_rejects_replaced_file_links_and_global_discovery_cannot_escape_config() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new(true);
    skill(fixture.project.join(".mh/skills/local"), "local", "local");
    let resources = fixture.load(&fixture.project);
    let path = fixture.project.join(".mh/skills/local/SKILL.md");
    std::fs::remove_file(&path).unwrap();
    skill(fixture.outside.join("outside"), "outside", "outside");
    symlink(fixture.outside.join("outside/SKILL.md"), path).unwrap();
    assert!(
        resources
            .skill_request("local", "")
            .unwrap_err()
            .contains("changed")
    );
    symlink(&fixture.outside, fixture.config.join("skills")).unwrap();
    let resources = Resources::load(&fixture.project, &fixture.config, ProjectSkills::Skip);
    assert!(resources.skills.is_empty());
    assert!(warned(&resources, "escapes"));
}

#[test]
fn unreadable_or_oversized_ignore_rules_fail_closed_and_cannot_be_negated_later() {
    let fixture = Fixture::new(true);
    skill(
        fixture.project.join("workflows/hidden"),
        "hidden",
        "must remain hidden",
    );
    file(
        fixture.project.join(".gitignore"),
        &format!("#{}\n", "x".repeat(70 * 1024)),
    );
    file(fixture.project.join(".fdignore"), "!**/\n!**\n");
    let resources = fixture.load(&fixture.project);
    assert!(resources.skills.is_empty());
    assert!(warned(&resources, "ignore rules exceed read limit"));
    std::fs::remove_file(fixture.project.join(".gitignore")).unwrap();
    std::fs::create_dir(fixture.project.join(".gitignore")).unwrap();
    let resources = fixture.load(&fixture.project);
    assert!(resources.skills.is_empty());
    assert!(warned(&resources, "not a regular file"));
}

#[cfg(unix)]
#[test]
fn aliases_cannot_bypass_canonical_ignore_rules_and_escaped_ignore_files_fail_closed() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new(true);
    skill(
        fixture.project.join("private/hidden"),
        "hidden",
        "ignored physical target",
    );
    file(fixture.project.join(".gitignore"), "private/\n");
    std::fs::create_dir_all(fixture.project.join(".mh/skills")).unwrap();
    symlink(
        fixture.project.join("private/hidden"),
        fixture.project.join(".mh/skills/alias"),
    )
    .unwrap();
    assert!(fixture.load(&fixture.project).skills.is_empty());
    std::fs::remove_file(fixture.project.join(".gitignore")).unwrap();
    file(fixture.outside.join("ignore"), "private/\n");
    symlink(
        fixture.outside.join("ignore"),
        fixture.project.join(".gitignore"),
    )
    .unwrap();
    let resources = fixture.load(&fixture.project);
    assert!(resources.skills.is_empty());
    assert!(warned(&resources, "affected tree skipped"));
}

#[test]
fn collision_diagnostics_identify_winner_and_loser_without_absolute_machine_paths() {
    let fixture = Fixture::new(true);
    skill(fixture.project.join(".mh/skills/local"), "same", "project");
    skill(fixture.config.join("skills/global"), "same", "global");
    let resources = fixture.load(&fixture.project);
    let warning = resources
        .warnings
        .iter()
        .find(|warning| warning.contains("Duplicate"))
        .unwrap();
    assert!(warning.contains("'same'"));
    assert!(warning.contains("kept project:"));
    assert!(warning.contains("skipped global:"));
    assert!(!warning.contains(&fixture._temp.path().to_string_lossy().to_string()));
}

#[cfg(unix)]
#[test]
fn canonical_dedupe_keeps_distinct_conventional_root_and_fallback_traversal_semantics() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new(true);
    std::fs::create_dir_all(fixture.project.join(".mh/skills")).unwrap();
    std::fs::create_dir_all(fixture.project.join(".pi/skills")).unwrap();
    file(
        fixture.project.join(".pi/skills/direct.md"),
        "---\nname: direct\ndescription: root markdown\n---\nbody",
    );
    symlink(
        fixture.project.join(".pi/skills"),
        fixture.project.join(".mh/skills/imported-alias"),
    )
    .unwrap();
    symlink(
        &fixture.project,
        fixture.project.join(".mh/skills/workspace-cycle"),
    )
    .unwrap();
    skill(
        fixture.project.join(".workflow/hidden"),
        "hidden",
        "fallback-only",
    );
    let resources = fixture.load(&fixture.project);
    assert_eq!(names(&resources), ["direct", "hidden"]);
    assert!(!warned(&resources, "Duplicate"));
    assert!(!warned(&resources, "traversal budget"));
}
