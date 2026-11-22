//! Small semantic palettes. New themes override colors, not layout or behavior.

use std::collections::BTreeMap;
use std::path::Path;

use ratatui::style::Color;
use schemars::{JsonSchema, r#gen::SchemaGenerator, schema::Schema};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct HexColor(String);

impl TryFrom<String> for HexColor {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let hex = value.strip_prefix('#');
        if value.is_empty()
            || hex.is_some_and(|v| {
                matches!(v.len(), 3 | 6) && v.bytes().all(|c| c.is_ascii_hexdigit())
            })
        {
            Ok(Self(value))
        } else {
            Err("expected #RGB, #RRGGBB, or an empty terminal-default color")
        }
    }
}

impl From<HexColor> for String {
    fn from(value: HexColor) -> Self {
        value.0
    }
}

impl JsonSchema for HexColor {
    fn schema_name() -> String {
        "HexColor".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let mut schema = String::json_schema(generator).into_object();
        schema.string().pattern = Some("^(#[A-Fa-f0-9]{3}|#[A-Fa-f0-9]{6})?$".into());
        schema.into()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum ColorValue {
    Indexed(u8),
    Hex(HexColor),
}

impl ColorValue {
    pub fn color(&self) -> Color {
        match self {
            Self::Indexed(index) => Color::Indexed(*index),
            Self::Hex(hex) if hex.0.is_empty() => Color::Reset,
            Self::Hex(hex) => {
                let source = &hex.0[1..];
                let expanded = if source.len() == 3 {
                    source.chars().flat_map(|c| [c, c]).collect::<String>()
                } else {
                    source.to_owned()
                };
                // HexColor guarantees exactly six valid hex digits here.
                let rgb = u32::from_str_radix(&expanded, 16).expect("validated color");
                Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
            }
        }
    }
}

macro_rules! palette {
    ($($name:ident: $r:literal, $g:literal, $b:literal;)*) => {
        #[derive(Clone, Debug)]
        pub struct Palette { $(pub $name: Color,)* }

        impl Default for Palette {
            fn default() -> Self {
                Self { $($name: Color::Rgb($r, $g, $b),)* }
            }
        }

        #[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        pub struct PaletteOverrides {
            $(#[serde(default, skip_serializing_if = "Option::is_none")]
              pub $name: Option<ColorValue>,)*
        }

        impl PaletteOverrides {
            pub fn resolve(&self) -> Palette {
                let mut palette = Palette::default();
                $(if let Some(value) = &self.$name { palette.$name = value.color(); })*
                palette
            }
        }
    };
}

// Restrained default dark palette; no custom font or icon dependency.
palette! {
    background: 36, 41, 46;
    text: 246, 248, 250;
    muted: 155, 159, 163;
    dim: 127, 131, 135;
    accent: 133, 232, 157;
    border: 83, 87, 91;
    border_accent: 133, 232, 157;
    selected_bg: 50, 54, 58;
    success: 133, 232, 157;
    error: 249, 117, 131;
    warning: 255, 234, 127;
    user_message_bg: 50, 54, 58;
    user_message_text: 246, 248, 250;
    thinking_text: 155, 159, 163;
    thinking_off: 127, 131, 135;
    thinking_minimal: 155, 159, 163;
    thinking_low: 121, 184, 255;
    thinking_medium: 255, 234, 127;
    thinking_high: 179, 146, 240;
    thinking_xhigh: 115, 227, 255;
    thinking_max: 133, 232, 157;
    tool_pending_bg: 50, 54, 58;
    tool_success_bg: 27, 36, 31;
    tool_error_bg: 48, 30, 36;
    tool_title: 121, 184, 255;
    tool_output: 246, 248, 250;
    md_heading: 121, 184, 255;
    md_code: 115, 227, 255;
    tool_diff_added: 133, 232, 157;
    tool_diff_removed: 249, 117, 131;
    tool_diff_context: 155, 159, 163;
    bash_mode: 115, 227, 255;
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThemeDefinition {
    pub colors: PaletteOverrides,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThemeFile {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default)]
    pub themes: BTreeMap<String, ThemeDefinition>,
}

impl ThemeFile {
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                crate::setup::validate_theme(&text)?;
                Self::parse(&text)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(_) => Err(format!("Cannot read theme file {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut deserializer = serde_json::Deserializer::from_str(text);
        let file: Self = serde_path_to_error::deserialize(&mut deserializer)
            .map_err(|error| format!("Invalid theme file at {}", error.path()))?;
        deserializer
            .end()
            .map_err(|_| "Trailing data in theme file")?;
        for name in file.themes.keys() {
            if name.is_empty() || name.contains('/') || name.chars().any(char::is_control) {
                return Err("Theme names must be non-empty, single-line, and contain no /".into());
            }
        }
        Ok(file)
    }

    pub fn resolve(&self, name: &str) -> Result<Palette, String> {
        if let Some(theme) = self.themes.get(name) {
            Ok(theme.colors.resolve())
        } else if name == "ci-dark" {
            Ok(Palette::default())
        } else {
            Err("Unknown theme; define it in ~/.mh/config/theme.json".into())
        }
    }
}
