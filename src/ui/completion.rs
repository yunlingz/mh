//! Shared retained command descriptions and Pi/Claude-style completion candidates.

use crate::config::Config;
use crate::context::Resources;

pub const COMMANDS: &[(&str, &str)] = &[
    ("btw", "Ask a tool-free side question"),
    ("compact", "Summarize older context"),
    ("exit", "Exit and show the resume command (alias of /quit)"),
    ("help", "Show commands and keyboard help"),
    ("model", "Select a configured model"),
    ("new", "Start a new workspace session"),
    ("quit", "Exit and show the resume command"),
    ("resume", "Choose a saved workspace session"),
    ("skills", "Create a project or global skill"),
    ("todos", "Show the complete task list"),
];

#[derive(Clone)]
pub struct Item {
    pub value: String,
    pub label: String,
    pub description: String,
}

pub struct Menu {
    pub prefix: String,
    pub items: Vec<Item>,
    pub selected: Option<usize>,
    pub command: bool,
}

impl Menu {
    pub fn navigate(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        self.selected = Some(self.selected.map_or(0, |i| {
            (i as isize + delta).rem_euclid(self.items.len() as isize) as usize
        }));
    }

    pub fn selected(&self) -> Option<&Item> {
        self.selected.and_then(|i| self.items.get(i))
    }
}

pub fn models(config: &Config) -> Vec<Item> {
    config
        .providers
        .iter()
        .flat_map(|(provider, settings)| {
            settings.models.iter().map(move |model| {
                let value = format!("{provider}/{}", model.local_id());
                Item {
                    label: value.clone(),
                    value,
                    description: format!(
                        "{} · effort {} · {} context",
                        model.id,
                        model.effort_label(),
                        super::footer::tokens(model.context_window)
                    ),
                }
            })
        })
        .collect()
}

fn score(value: &str, query: &str) -> Option<usize> {
    let value = value.to_lowercase();
    let query = query.to_lowercase();
    if value.starts_with(&query) {
        return Some(0);
    }
    if value.contains(&query) {
        return Some(1);
    }
    let mut chars = value.chars();
    query
        .chars()
        .all(|needle| chars.by_ref().any(|c| c == needle))
        .then_some(2)
}

pub fn filter(items: Vec<Item>, query: &str) -> Vec<Item> {
    let mut items = items
        .into_iter()
        .filter_map(|item| score(&item.label, query).map(|score| (score, item)))
        .collect::<Vec<_>>();
    items.sort_by(|(a, left), (b, right)| a.cmp(b).then_with(|| left.label.cmp(&right.label)));
    items.into_iter().take(100).map(|(_, item)| item).collect()
}

pub fn menu(
    before: &str,
    config: &Config,
    resources: &Resources,
    _qualified: &str,
    files: &[String],
    force: bool,
) -> Option<Menu> {
    if let Some(command) = before.strip_prefix('/') {
        if !command.contains(char::is_whitespace) {
            let items = COMMANDS
                .iter()
                .map(|(name, description)| Item {
                    value: format!("/{name} "),
                    label: format!("/{name}"),
                    description: (*description).into(),
                })
                .chain(resources.skills.iter().map(|skill| Item {
                    value: format!("/skill:{} ", skill.name),
                    label: format!("/skill:{}", skill.name),
                    description: skill.description.clone(),
                }))
                .collect();
            let items = filter(items, before);
            let selected = items
                .first()
                .filter(|item| item.label.starts_with(before))
                .map(|_| 0);
            return Some(Menu {
                prefix: before.into(),
                items,
                selected,
                command: true,
            });
        }
        let (name, argument) = command.split_once(char::is_whitespace)?;
        let argument = argument.trim_start();
        let candidates = match name {
            "model" => models(config),
            "skills" => vec![Item {
                value: "create ".into(),
                label: "create".into(),
                description: "Create a project or global skill".into(),
            }],
            _ => Vec::new(),
        };
        if !candidates.is_empty() {
            return Some(Menu {
                prefix: argument.into(),
                items: filter(candidates, argument),
                selected: Some(0),
                command: false,
            });
        }
    }
    let token = before
        .rsplit_once(char::is_whitespace)
        .map_or(before, |(_, token)| token);
    if token.starts_with('@') || token.contains('/') || force {
        let (at, query) = token
            .strip_prefix('@')
            .map_or((false, token), |query| (true, query));
        let query = query.trim_start_matches("./").trim_matches('"');
        let items = filter(
            files
                .iter()
                .map(|file| {
                    let quoted = if file.contains(char::is_whitespace) {
                        format!("\"{file}\"")
                    } else {
                        file.clone()
                    };
                    Item {
                        value: format!(
                            "{}{quoted}{}",
                            if at { "@" } else { "" },
                            if file.ends_with('/') { "" } else { " " }
                        ),
                        label: file.clone(),
                        description: "File path".into(),
                    }
                })
                .collect(),
            query,
        );
        if items.is_empty() {
            return None;
        }
        return Some(Menu {
            prefix: token.into(),
            items,
            selected: Some(0),
            command: false,
        });
    }
    None
}

/// Runs in a blocking worker, never in the drawing task. No symlink traversal.
pub fn file_index(cwd: &std::path::Path) -> Vec<String> {
    let mut files = Vec::new();
    for entry in ignore::WalkBuilder::new(cwd)
        .hidden(true)
        .follow_links(false)
        .build()
        .filter_map(Result::ok)
        .take(10_000)
    {
        if entry.path() == cwd {
            continue;
        }
        if let Ok(relative) = entry.path().strip_prefix(cwd) {
            let name = relative.to_string_lossy();
            #[cfg(windows)]
            let name = name.replace('\\', "/");
            let suffix = if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                "/"
            } else {
                ""
            };
            files.push(format!("{name}{suffix}"));
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completion_filters_conservatively_and_quotes_paths() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config::parse(r#"{"defaultModel":"test/model","providers":{"test":{"api":"openai-responses-sse","base_url_env":"TEST_BASE","api_key_env":"TEST_KEY","models":[{"id":"model","contextWindow":64000,"maxTokens":1024,"compaction":{"reserveTokens":2048,"keepRecentTokens":4000}}]}}}"#).unwrap();
        let resources = Resources::load(
            temp.path(),
            temp.path(),
            crate::context::ProjectSkills::Skip,
        );
        let m = menu("/res", &config, &resources, "test/model", &[], false).unwrap();
        assert_eq!(m.selected().unwrap().value, "/resume ");
        let m = menu("/rsm", &config, &resources, "test/model", &[], false).unwrap();
        assert!(m.selected().is_none());
        let m = menu(
            "@my",
            &config,
            &resources,
            "test/model",
            &["my file.rs".into()],
            false,
        )
        .unwrap();
        assert_eq!(m.selected().unwrap().value, "@\"my file.rs\" ");
        assert!(menu("plain words", &config, &resources, "test/model", &[], false).is_none());
    }
}
