//! Generate distribution schemas from the runtime's configuration types.

use std::path::Path;

use mh::{config::Config, theme::ThemeFile};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let check = std::env::args().skip(1).any(|arg| arg == "--check");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("schema");
    let schemas = [
        (
            "config.schema.json",
            serde_json::to_value(schemars::schema_for!(Config))?,
        ),
        (
            "theme.schema.json",
            serde_json::to_value(schemars::schema_for!(ThemeFile))?,
        ),
    ];
    for (name, schema) in schemas {
        let path = root.join(name);
        let text = format!("{}\n", serde_json::to_string_pretty(&schema)?);
        if check {
            if std::fs::read_to_string(&path)? != text {
                return Err(format!("Schema is stale: {}", path.display()).into());
            }
        } else {
            std::fs::create_dir_all(&root)?;
            std::fs::write(&path, text)?;
        }
    }
    Ok(())
}
