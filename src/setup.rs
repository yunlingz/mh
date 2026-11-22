//! Creation of user-editable configuration and authoritative generated schemas.

use std::io::Write;
use std::path::Path;

use crate::storage::{Layout, new_private_file, private_dir, write_new};

pub const CONFIG_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/schema/config.schema.json"
));
pub const THEME_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/schema/theme.schema.json"
));

const EMPTY_CONFIG: &str = "{\n  \"$schema\": \"./config.schema.json\"\n}\n";
const EMPTY_THEME: &str = "{\n  \"$schema\": \"./theme.schema.json\"\n}\n";

pub struct InitReport {
    pub config_created: bool,
    pub theme_created: bool,
}

pub fn refresh_schemas(layout: &Layout) -> Result<(), String> {
    private_dir(&layout.config_dir()).map_err(|_| "Cannot create the mh config directory")?;
    replace_private(&layout.config_schema_file(), CONFIG_SCHEMA.as_bytes())
        .map_err(|_| "Cannot refresh config.schema.json")?;
    replace_private(&layout.theme_schema_file(), THEME_SCHEMA.as_bytes())
        .map_err(|_| "Cannot refresh theme.schema.json")?;
    Ok(())
}

pub fn initialize(layout: &Layout) -> Result<InitReport, String> {
    refresh_schemas(layout)?;
    let config_created = create_if_missing(&layout.config_file(), EMPTY_CONFIG.as_bytes())
        .map_err(|_| "Cannot initialize config.json")?;
    let theme_created = create_if_missing(&layout.theme_file(), EMPTY_THEME.as_bytes())
        .map_err(|_| "Cannot initialize theme.json")?;
    Ok(InitReport {
        config_created,
        theme_created,
    })
}

pub fn validate_config(document: &str) -> Result<(), String> {
    let mut instance: serde_json::Value = serde_json::from_str(document)
        .map_err(|_| "config.json does not fulfill its schema: invalid JSON")?;
    crate::config::normalize_input(&mut instance)?;
    validate_instance(&instance, CONFIG_SCHEMA, "config.json")
}

pub fn validate_theme(document: &str) -> Result<(), String> {
    validate_document(document, THEME_SCHEMA, "theme.json")
}

fn validate_document(document: &str, schema: &str, name: &str) -> Result<(), String> {
    let instance: serde_json::Value = serde_json::from_str(document)
        .map_err(|_| format!("{name} does not fulfill its schema: invalid JSON"))?;
    validate_instance(&instance, schema, name)
}

fn validate_instance(instance: &serde_json::Value, schema: &str, name: &str) -> Result<(), String> {
    let schema: serde_json::Value =
        serde_json::from_str(schema).map_err(|_| "Compiled mh schema is invalid")?;
    let validator =
        jsonschema::validator_for(&schema).map_err(|_| "Compiled mh schema is invalid")?;
    validator.validate(instance).map_err(|error| {
        let path = error.instance_path().as_str();
        format!(
            "{name} does not fulfill its schema at {}",
            if path.is_empty() { "/" } else { path }
        )
    })
}

fn create_if_missing(path: &Path, content: &[u8]) -> std::io::Result<bool> {
    match write_new(path, content) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

fn replace_private(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Schema path has no parent",
        )
    })?;
    private_dir(parent)?;
    let temporary = parent.join(format!(".schema-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = new_private_file(&temporary)?;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        // Replacing the directory entry leaves symlink and hard-link targets
        // untouched, and readers never observe a partially written schema.
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
