//! Validated configuration. Endpoint and credential values never live in JSON.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use schemars::{JsonSchema, r#gen::SchemaGenerator, schema::Schema};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct EnvName(String);

impl TryFrom<String> for EnvName {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let mut bytes = value.bytes();
        let first = bytes
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_');
        if first && bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_') {
            Ok(Self(value))
        } else {
            Err("expected an environment variable name")
        }
    }
}

impl From<EnvName> for String {
    fn from(value: EnvName) -> Self {
        value.0
    }
}

impl JsonSchema for EnvName {
    fn schema_name() -> String {
        "EnvironmentVariableName".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let mut schema = String::json_schema(generator).into_object();
        schema.string().pattern = Some("^[A-Za-z_][A-Za-z0-9_]*$".into());
        schema.into()
    }
}

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InputModality {
    Text,
    Image,
}

fn text_input() -> Vec<InputModality> {
    vec![InputModality::Text]
}

fn default_theme() -> String {
    "ci-dark".into()
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CompactionConfig {
    #[schemars(range(min = 1))]
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ModelConfig {
    /// Provider wire ID; never replaced by the local selection alias.
    pub id: String,
    /// Unique within the provider. Omission retains the wire ID as the local name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default = "text_input")]
    pub input: Vec<InputModality>,
    #[serde(default)]
    pub reasoning: bool,
    /// Fixed provider reasoning effort. Null or omission uses the provider default;
    /// an explicit "none" effort remains distinct from that default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 32), regex(pattern = "^[A-Za-z0-9_-]+$"))]
    pub reasoning_effort: Option<String>,
    #[schemars(range(min = 1))]
    pub context_window: u64,
    #[schemars(range(min = 16))]
    pub max_tokens: u64,
    pub compaction: CompactionConfig,
}

impl ModelConfig {
    pub fn local_id(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.id)
    }

    pub fn effort_label(&self) -> &str {
        if !self.reasoning {
            "off"
        } else {
            self.reasoning_effort.as_deref().unwrap_or("default")
        }
    }
}

// Raw typed parsing retains duplicate-field rejection even after the separate
// schema view has normalized legacy JSON. No deprecated field survives this input.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ModelInput {
    id: String,
    #[serde(default)]
    alias: Option<String>,
    #[serde(default = "text_input")]
    input: Vec<InputModality>,
    #[serde(default)]
    reasoning: bool,
    #[serde(default, deserialize_with = "present")]
    reasoning_effort: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    thinking_level: Option<ThinkingLevel>,
    #[serde(default, deserialize_with = "present")]
    thinking_level_map: Option<BTreeMap<ThinkingLevel, Option<String>>>,
    context_window: u64,
    max_tokens: u64,
    compaction: CompactionConfig,
}

fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

impl<'de> Deserialize<'de> for ModelConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let input = ModelInput::deserialize(deserializer)?;
        let legacy = input.thinking_level.is_some() || input.thinking_level_map.is_some();
        let reasoning_effort = match input.reasoning_effort {
            Some(_) if legacy => {
                return Err(serde::de::Error::custom(
                    "reasoningEffort must not be mixed with deprecated thinking fields",
                ));
            }
            Some(effort) => effort,
            None if legacy => LegacyReasoning {
                reasoning: input.reasoning,
                thinking_level: input.thinking_level.unwrap_or_default(),
                thinking_level_map: input.thinking_level_map.unwrap_or_default(),
            }
            .into_effort()
            .map_err(serde::de::Error::custom)?,
            None => None,
        };
        Ok(Self {
            id: input.id,
            alias: input.alias,
            input: input.input,
            reasoning: input.reasoning,
            reasoning_effort,
            context_window: input.context_window,
            max_tokens: input.max_tokens,
            compaction: input.compaction,
        })
    }
}

fn valid_effort(effort: &str) -> bool {
    !effort.is_empty()
        && effort.len() <= 32
        && effort
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

// Deprecated fields exist only at the JSON boundary, never in ModelConfig.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyReasoning {
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    thinking_level: ThinkingLevel,
    #[serde(default)]
    thinking_level_map: BTreeMap<ThinkingLevel, Option<String>>,
}

impl LegacyReasoning {
    fn into_effort(self) -> Result<Option<String>, &'static str> {
        if !self.reasoning {
            return if self.thinking_level == ThinkingLevel::Off
                && self.thinking_level_map.is_empty()
            {
                Ok(None)
            } else {
                Err("legacy thinking settings require reasoning capability")
            };
        }
        if self
            .thinking_level_map
            .values()
            .flatten()
            .any(|effort| !valid_effort(effort))
        {
            return Err("legacy reasoning efforts must be short ASCII identifiers");
        }
        self.thinking_level_map
            .get(&self.thinking_level)
            .cloned()
            .ok_or("legacy thinkingLevelMap must include the selected thinkingLevel")
    }
}

/// Normalize deprecated configuration JSON in memory before schema/typed parsing.
/// Keep all unrelated fields intact so authoritative validation still sees them.
pub(crate) fn normalize_input(value: &mut serde_json::Value) -> Result<(), String> {
    let Some(providers) = value
        .get_mut("providers")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok(());
    };
    for (provider, definition) in providers {
        let Some(models) = definition
            .get_mut("models")
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        for (index, value) in models.iter_mut().enumerate() {
            let Some(model) = value.as_object_mut() else {
                continue;
            };
            if !model.contains_key("thinkingLevel") && !model.contains_key("thinkingLevelMap") {
                continue;
            }
            let field = format!("providers.{provider}.models[{index}]");
            if model.contains_key("reasoningEffort") {
                return Err(format!(
                    "{field}: reasoningEffort must not be mixed with deprecated thinking fields"
                ));
            }
            let legacy: LegacyReasoning =
                serde_json::from_value(serde_json::Value::Object(model.clone()))
                    .map_err(|_| format!("Invalid legacy reasoning configuration at {field}"))?;
            let effort = legacy
                .into_effort()
                .map_err(|error| format!("{field}: {error}"))?;
            model.remove("thinkingLevel");
            model.remove("thinkingLevelMap");
            model.insert("reasoningEffort".into(), serde_json::json!(effort));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum Api {
    #[serde(rename = "openai-responses-sse")]
    OpenAiResponsesSse,
}

impl Api {
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAiResponsesSse => "openai-responses-sse",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub api: Api,
    pub base_url_env: EnvName,
    pub api_key_env: EnvName,
    pub models: Vec<ModelConfig>,
}

// Intentionally not Debug: callers must not log resolved endpoint or key values.
pub struct Credentials {
    pub endpoint: url::Url,
    pub api_key: String,
}

impl ProviderConfig {
    pub fn resolve(&self) -> Result<Credentials, String> {
        self.resolve_with(|name| std::env::var(name).ok())
    }

    pub fn resolve_with(
        &self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Credentials, String> {
        let resolve = |name: &EnvName| {
            lookup(&name.0)
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| format!("Missing or empty environment variable {}", name.0))
        };
        let base = resolve(&self.base_url_env)?;
        let api_key = resolve(&self.api_key_env)?;
        let mut endpoint = url::Url::parse(&base)
            .map_err(|_| format!("Invalid endpoint in {}", self.base_url_env.0))?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(format!("Invalid endpoint in {}", self.base_url_env.0));
        }
        let path = format!("{}/responses", endpoint.path().trim_end_matches('/'));
        endpoint.set_path(&path);
        Ok(Credentials { endpoint, api_key })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default = "default_theme")]
    pub theme: String,
    pub default_model: String,
    pub providers: BTreeMap<String, ProviderConfig>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|_| format!("Cannot read configuration {}", path.display()))?;
        crate::setup::validate_config(&text)?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut deserializer = serde_json::Deserializer::from_str(text);
        let config: Self = serde_path_to_error::deserialize(&mut deserializer)
            .map_err(|error| format!("Invalid configuration at {}", error.path()))?;
        deserializer
            .end()
            .map_err(|_| "Trailing data in configuration")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.theme.is_empty() || self.providers.is_empty() {
            return Err("Theme and providers must not be empty".into());
        }
        for (provider, definition) in &self.providers {
            if provider.is_empty()
                || provider != provider.trim()
                || provider.chars().any(char::is_control)
                || provider.contains('/')
                || definition.models.is_empty()
            {
                return Err(
                    "Provider names must be non-empty without /, controls, or surrounding whitespace; models must not be empty".into(),
                );
            }
            let mut aliases = BTreeSet::new();
            for (index, model) in definition.models.iter().enumerate() {
                let field = format!("providers.{provider}.models[{index}]");
                if model.id.trim().is_empty()
                    || model.id.chars().any(char::is_control)
                    || model.local_id().trim().is_empty()
                    || model.local_id() != model.local_id().trim()
                    || model.local_id().chars().any(char::is_control)
                    || !aliases.insert(model.local_id())
                    || model.input.is_empty()
                {
                    return Err(format!(
                        "{field}: model IDs and input must not be empty; local aliases must be unique without controls or surrounding whitespace"
                    ));
                }
                let reserve = model.compaction.reserve_tokens;
                if model.context_window == 0
                    || model.max_tokens < 16
                    || model.max_tokens >= model.context_window
                    || reserve == 0
                    || reserve >= model.context_window
                    || model.compaction.keep_recent_tokens >= model.context_window - reserve
                {
                    return Err(format!("{field}: token budgets do not fit contextWindow"));
                }
                if let Some(effort) = &model.reasoning_effort {
                    if !model.reasoning {
                        return Err(format!(
                            "{field}: reasoningEffort requires reasoning capability"
                        ));
                    }
                    if !valid_effort(effort) {
                        return Err(format!(
                            "{field}: reasoningEffort must be a short ASCII identifier"
                        ));
                    }
                }
            }
        }
        self.model(&self.default_model)?;
        Ok(())
    }

    pub fn model(&self, qualified: &str) -> Result<(&str, &ProviderConfig, &ModelConfig), String> {
        for (provider, definition) in &self.providers {
            for model in &definition.models {
                if qualified == format!("{provider}/{}", model.local_id()) {
                    return Ok((provider, definition, model));
                }
            }
        }
        Err("Unknown model; use a configured provider/model alias".into())
    }

    pub fn model_ids(&self) -> Vec<String> {
        self.providers
            .iter()
            .flat_map(|(name, provider)| {
                provider
                    .models
                    .iter()
                    .map(move |model| format!("{name}/{}", model.local_id()))
            })
            .collect()
    }
}
