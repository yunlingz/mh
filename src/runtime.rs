//! Startup assembly shared by print and fullscreen modes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::agent::tools::ToolEvent;
use crate::agent::{Agent, Event};
use crate::config::Config;
use crate::context::{ProjectSkills, Resources};
use crate::providers::Responses;
use crate::session::{Entry, ModelChoice, Session};
use crate::storage::Layout;
use crate::theme::{Palette, ThemeFile};

pub enum SessionSelection {
    New,
    Continue,
    Exact(String),
}

pub struct Prepared {
    pub cwd: PathBuf,
    pub layout: Layout,
    pub config: Arc<Config>,
    pub palette: Palette,
}

pub struct Harness {
    pub agent: Arc<Agent>,
    pub config: Arc<Config>,
    pub palette: Palette,
    pub layout: Layout,
    pub cwd: PathBuf,
}

pub struct Options {
    pub selection: SessionSelection,
    pub model: Option<String>,
    pub interactive: bool,
    pub events: Option<mpsc::Sender<Event>>,
    pub tool_events: Option<mpsc::Sender<ToolEvent>>,
}

impl Harness {
    pub async fn load(cwd: &Path, options: Options) -> Result<Self, String> {
        Self::load_with_layout(cwd, Layout::user()?, options).await
    }

    pub(crate) async fn load_with_layout(
        cwd: &Path,
        layout: Layout,
        options: Options,
    ) -> Result<Self, String> {
        Self::load_prepared(Self::prepare(cwd, layout)?, options).await
    }

    pub fn prepare(cwd: &Path, layout: Layout) -> Result<Prepared, String> {
        let cwd = std::fs::canonicalize(cwd)
            .map_err(|_| "Cannot access the current working directory".to_owned())?;
        crate::setup::refresh_schemas(&layout)?;
        let config = Arc::new(Config::load(&layout.config_file())?);
        let themes = ThemeFile::load(&layout.theme_file())?;
        let palette = themes.resolve(&config.theme)?;
        Ok(Prepared {
            cwd,
            layout,
            config,
            palette,
        })
    }

    pub async fn load_prepared(prepared: Prepared, options: Options) -> Result<Self, String> {
        let Prepared {
            cwd,
            layout,
            config,
            palette,
        } = prepared;
        let session = Arc::new(
            match options.selection {
                SessionSelection::New => Session::create(&layout, &cwd),
                SessionSelection::Continue => {
                    match Session::latest(&layout, &cwd).map_err(|error| error.to_string())? {
                        Some(info) => Session::open(&layout, &cwd, &info.id),
                        None => Session::create(&layout, &cwd),
                    }
                }
                SessionSelection::Exact(id) => Session::open(&layout, &cwd, &id),
            }
            .map_err(|error| error.to_string())?,
        );
        // An explicit CLI entry rescues removed/ambiguous saved selections without
        // first validating the old choice. Agent::new normalizes resumed choices.
        if let Some(target) = options.model {
            config.model(&target)?;
            session
                .append(Entry::Model {
                    choice: ModelChoice::new(target),
                })
                .await
                .map_err(|error| error.to_string())?;
        }

        let resources = Resources::load(&cwd, &layout.config_dir(), ProjectSkills::Load);
        let http = Responses::new()?;
        let agent = Arc::new(
            Agent::new(
                config.clone(),
                session.clone(),
                resources,
                http,
                options.events,
                options.tool_events,
                options.interactive,
            )
            .await?,
        );

        Ok(Self {
            agent,
            config,
            palette,
            layout,
            cwd,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Harness, Options, SessionSelection};
    use crate::session::{Entry, ModelChoice, Session};
    use crate::storage::Layout;

    #[tokio::test]
    async fn explicit_choices_rescue_incompatible_saved_sessions() {
        for (saved_model, model) in [("removed/model", Some("test/model")), ("test/model", None)] {
            let temp = tempfile::tempdir().unwrap();
            let cwd = temp.path().join("project");
            std::fs::create_dir_all(&cwd).unwrap();
            let layout = Layout {
                root: temp.path().join("mh"),
            };
            std::fs::create_dir_all(layout.config_dir()).unwrap();
            std::fs::write(
                layout.config_file(),
                json!({
                    "defaultModel":"test/model",
                    "providers":{"test":{
                        "api":"openai-responses-sse",
                        "base_url_env":"MH_RUNTIME_TEST_BASE",
                        "api_key_env":"MH_RUNTIME_TEST_KEY",
                        "models":[{"id":"model","contextWindow":64000,"maxTokens":1024,
                            "compaction":{"reserveTokens":2048,"keepRecentTokens":4000}}]
                    }}
                })
                .to_string(),
            )
            .unwrap();
            let session = Session::create(&layout, &cwd).unwrap();
            session
                .append(Entry::Model {
                    choice: ModelChoice::new(saved_model),
                })
                .await
                .unwrap();
            let id = session.id().to_owned();
            let file = session.file();
            drop(session);
            let history = std::fs::read_to_string(&file).unwrap().replace(
                &format!(r#""qualified":"{saved_model}""#),
                &format!(r#""qualified":"{saved_model}","thinking":"high""#),
            );
            std::fs::write(&file, &history).unwrap();
            let harness = Harness::load_with_layout(
                &cwd,
                layout,
                Options {
                    selection: SessionSelection::Exact(id),
                    model: model.map(str::to_owned),
                    interactive: false,
                    events: None,
                    tool_events: None,
                },
            )
            .await
            .unwrap();
            let choice = harness.agent.session.snapshot().choice.clone().unwrap();
            assert_eq!(choice.qualified, "test/model");
            assert!(
                std::fs::read_to_string(&file)
                    .unwrap()
                    .starts_with(&history)
            );
            assert_eq!(
                harness
                    .agent
                    .resources()
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("normalized")),
                model.is_none(),
            );
            assert_eq!(
                serde_json::to_value(choice).unwrap(),
                json!({"qualified":"test/model"}),
            );
        }
    }
}
