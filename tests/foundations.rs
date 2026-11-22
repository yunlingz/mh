use mh::config::Config;
use mh::setup::{
    CONFIG_SCHEMA, THEME_SCHEMA, initialize, refresh_schemas, validate_config, validate_theme,
};
use mh::storage::{Layout, output_name, write_new};
use mh::theme::ThemeFile;
use ratatui::style::Color;
use serde_json::{Value, json};

fn config_value() -> Value {
    json!({
        "defaultModel": "work/model",
        "providers": { "work": {
            "api": "openai-responses-sse",
            "base_url_env": "TEST_BASE", "api_key_env": "TEST_KEY",
            "models": [{
                "id": "model", "contextWindow": 128000, "maxTokens": 16384,
                "compaction": { "reserveTokens": 16384, "keepRecentTokens": 20000 }
            }]
        }}
    })
}

#[test]
fn theme_defaults_to_ci_dark() {
    let config = Config::parse(&config_value().to_string()).unwrap();
    assert_eq!(config.theme, "ci-dark");
    assert_eq!(config.model_ids(), vec!["work/model"]);
}

#[test]
fn aliases_select_direct_fixed_efforts_sharing_one_wire_id() {
    let mut value = config_value();
    let mut low = value["providers"]["work"]["models"][0].clone();
    low["alias"] = json!("fast");
    low["reasoning"] = json!(true);
    low["reasoningEffort"] = json!("low");
    let mut high = low.clone();
    high["alias"] = json!("deep");
    high["reasoningEffort"] = json!("high");
    value["defaultModel"] = json!("work/fast");
    value["providers"]["work"]["models"] = json!([low, high]);
    let text = value.to_string();
    validate_config(&text).unwrap();
    let config = Config::parse(&text).unwrap();
    assert_eq!(config.model_ids(), ["work/fast", "work/deep"]);
    assert!(config.model("work/model").is_err());
    let low = config.model("work/fast").unwrap().2;
    let high = config.model("work/deep").unwrap().2;
    assert_eq!(low.id, high.id);
    assert_eq!(low.reasoning_effort.as_deref(), Some("low"));
    assert_eq!(high.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(low.effort_label(), "low");
    assert_eq!(high.effort_label(), "high");
    let saved: mh::session::ModelChoice = serde_json::from_value(json!({
        "qualified":"work/model", "thinking":"high"
    }))
    .unwrap();
    let (normalized, migrated) = saved.normalize(&config).unwrap();
    assert!(migrated);
    assert_eq!(normalized.qualified, "work/deep");
    assert_eq!(
        serde_json::to_value(normalized).unwrap(),
        json!({"qualified":"work/deep"})
    );
    // Canonical entry identity is authoritative even if a former effort changed.
    let selected = mh::session::ModelChoice::new("work/fast");
    assert_eq!(
        selected.normalize(&config).unwrap(),
        (selected.clone(), false)
    );
    let canonical = serde_json::to_value(&config).unwrap();
    assert_eq!(
        canonical["providers"]["work"]["models"][0]["reasoningEffort"],
        "low"
    );
    assert!(!canonical.to_string().contains("thinkingLevel"));
    assert_eq!(
        value.to_string(),
        text,
        "normalization must not rewrite configuration"
    );
    value["providers"]["work"]["models"][1]["alias"] = json!("fast");
    assert!(Config::parse(&value.to_string()).is_err());
    for alias in ["", "   ", "bad\nname"] {
        value["providers"]["work"]["models"][1]["alias"] = json!(alias);
        assert!(Config::parse(&value.to_string()).is_err());
    }
}

#[test]
fn direct_effort_defaults_explicit_none_and_boundary_validation_are_distinct() {
    let mut value = config_value();
    value["providers"]["work"]["models"][0]["reasoning"] = json!(true);
    let config = Config::parse(&value.to_string()).unwrap();
    assert_eq!(
        config.model("work/model").unwrap().2.effort_label(),
        "default"
    );
    for effort in [Value::Null, json!("none"), json!("xhigh")] {
        value["providers"]["work"]["models"][0]["reasoningEffort"] = effort.clone();
        validate_config(&value.to_string()).unwrap();
        let config = Config::parse(&value.to_string()).unwrap();
        let model = config.model("work/model").unwrap().2;
        assert_eq!(model.reasoning_effort.as_deref(), effort.as_str());
        assert_eq!(model.effort_label(), effort.as_str().unwrap_or("default"));
    }
    let plain = Config::parse(&config_value().to_string()).unwrap();
    assert_eq!(plain.model("work/model").unwrap().2.effort_label(), "off");
    for effort in ["", " ", "bad/value", "日本", &"x".repeat(33)] {
        value["providers"]["work"]["models"][0]["reasoningEffort"] = json!(effort);
        assert!(Config::parse(&value.to_string()).is_err());
        assert!(validate_config(&value.to_string()).is_err());
    }
    value["providers"]["work"]["models"][0]["reasoningEffort"] = json!("high");
    value["providers"]["work"]["models"][0]["reasoning"] = json!(false);
    assert!(Config::parse(&value.to_string()).is_err());
}

#[test]
fn direct_effort_input_retains_duplicate_field_rejection_after_schema_normalization() {
    let mut value = config_value();
    value["providers"]["work"]["models"][0]["reasoning"] = json!(true);
    value["providers"]["work"]["models"][0]["reasoningEffort"] = json!("high");
    let text = value.to_string().replace(
        r#""reasoningEffort":"high""#,
        r#""reasoningEffort":null,"reasoningEffort":"high""#,
    );
    assert!(Config::parse(&text).is_err());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    std::fs::write(&path, &text).unwrap();
    assert!(Config::load(&path).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
}

#[test]
fn deprecated_effort_json_normalizes_before_modern_schema_validation_without_rewriting_files() {
    let temp = tempfile::tempdir().unwrap();
    let layout = Layout {
        root: temp.path().join("mh"),
    };
    refresh_schemas(&layout).unwrap();
    assert!(!CONFIG_SCHEMA.contains("thinkingLevel"));
    for (level, mapping, expected) in [
        (
            "high",
            json!({"off":"none","high":"xhigh","low":"low"}),
            Some("xhigh"),
        ),
        ("low", json!({"off":null,"low":null}), None),
        ("off", json!({"off":"none"}), Some("none")),
    ] {
        let mut value = config_value();
        let model = &mut value["providers"]["work"]["models"][0];
        model["reasoning"] = json!(true);
        model["thinkingLevel"] = json!(level);
        model["thinkingLevelMap"] = mapping;
        // Deprecated fields disappear at typed JSON deserialization; they never
        // survive in runtime ModelConfig or its canonical serialization.
        let normalized: mh::config::ModelConfig = serde_json::from_value(model.clone()).unwrap();
        assert!(
            !serde_json::to_value(normalized)
                .unwrap()
                .to_string()
                .contains("thinkingLevel")
        );
        let original = value.to_string();
        std::fs::write(layout.config_file(), &original).unwrap();
        validate_config(&original).unwrap();
        let config = Config::load(&layout.config_file()).unwrap();
        assert_eq!(
            config
                .model("work/model")
                .unwrap()
                .2
                .reasoning_effort
                .as_deref(),
            expected
        );
        assert_eq!(
            std::fs::read_to_string(layout.config_file()).unwrap(),
            original
        );
        let canonical = serde_json::to_value(&config).unwrap();
        assert!(!canonical.to_string().contains("thinkingLevel"));
        validate_config(&canonical.to_string()).unwrap();
        for effort in [Value::Null, json!("high")] {
            let mut mixed = value.clone();
            mixed["providers"]["work"]["models"][0]["reasoningEffort"] = effort;
            assert!(Config::parse(&mixed.to_string()).is_err());
            assert!(
                validate_config(&mixed.to_string())
                    .unwrap_err()
                    .contains("must not be mixed")
            );
        }
    }
    // Saved legacy off selects explicit none, not the provider-default entry.
    let mut value = config_value();
    let mut default = value["providers"]["work"]["models"][0].clone();
    default["alias"] = json!("default");
    default["reasoning"] = json!(true);
    default["reasoningEffort"] = Value::Null;
    let mut none = default.clone();
    none["alias"] = json!("none");
    none["reasoningEffort"] = json!("none");
    value["defaultModel"] = json!("work/default");
    value["providers"]["work"]["models"] = json!([default, none]);
    let config = Config::parse(&value.to_string()).unwrap();
    let saved: mh::session::ModelChoice =
        serde_json::from_value(json!({"qualified":"work/model","thinking":"off"})).unwrap();
    assert_eq!(saved.normalize(&config).unwrap().0.qualified, "work/none");
}

#[test]
fn saved_wire_choices_do_not_guess_between_ambiguous_renamed_entries() {
    let mut value = config_value();
    let model = value["providers"]["work"]["models"][0].clone();
    let mut first = model.clone();
    first["alias"] = json!("one");
    let mut second = model;
    second["alias"] = json!("two");
    let mut unrelated = first.clone();
    unrelated["id"] = json!("other");
    unrelated["alias"] = json!("default");
    value["defaultModel"] = json!("work/default");
    value["providers"]["work"]["models"] = json!([first, second, unrelated]);
    let config = Config::parse(&value.to_string()).unwrap();
    let saved: mh::session::ModelChoice = serde_json::from_value(json!({
        "qualified":"work/model", "thinking":"off"
    }))
    .unwrap();
    assert!(saved.normalize(&config).unwrap_err().contains("ambiguous"));
}

#[test]
fn init_creates_editable_files_and_refreshes_authoritative_schemas() {
    let temp = tempfile::tempdir().unwrap();
    let layout = Layout {
        root: temp.path().join("mh"),
    };
    let report = initialize(&layout).unwrap();
    assert!(report.config_created && report.theme_created);
    assert_eq!(
        std::fs::read_to_string(layout.config_schema_file()).unwrap(),
        CONFIG_SCHEMA
    );
    assert_eq!(
        std::fs::read_to_string(layout.theme_schema_file()).unwrap(),
        THEME_SCHEMA
    );
    let empty_config = std::fs::read_to_string(layout.config_file()).unwrap();
    let empty_theme = std::fs::read_to_string(layout.theme_file()).unwrap();
    assert!(validate_config(&empty_config).is_err());
    assert!(validate_theme(&empty_theme).is_ok());
    assert!(Config::load(&layout.config_file()).is_err());
    assert!(ThemeFile::load(&layout.theme_file()).is_ok());
    assert!(validate_theme(r#"{"themes":[]}"#).is_err());

    let config = serde_json::to_string_pretty(&config_value()).unwrap() + "\n";
    let theme = "{\n  \"$schema\": \"./theme.schema.json\",\n  \"themes\": {}\n}\n";
    std::fs::write(layout.config_file(), &config).unwrap();
    std::fs::write(layout.theme_file(), theme).unwrap();
    std::fs::write(layout.config_schema_file(), "modified").unwrap();
    std::fs::write(layout.theme_schema_file(), "modified").unwrap();

    let report = initialize(&layout).unwrap();
    assert!(!report.config_created && !report.theme_created);
    assert_eq!(
        std::fs::read_to_string(layout.config_file()).unwrap(),
        config
    );
    assert_eq!(std::fs::read_to_string(layout.theme_file()).unwrap(), theme);
    assert_eq!(
        std::fs::read_to_string(layout.config_schema_file()).unwrap(),
        CONFIG_SCHEMA
    );
    assert_eq!(
        std::fs::read_to_string(layout.theme_schema_file()).unwrap(),
        THEME_SCHEMA
    );

    std::fs::write(layout.config_schema_file(), "modified again").unwrap();
    refresh_schemas(&layout).unwrap();
    assert_eq!(
        std::fs::read_to_string(layout.config_schema_file()).unwrap(),
        CONFIG_SCHEMA
    );
    assert!(Config::load(&layout.config_file()).is_ok());
    assert!(ThemeFile::load(&layout.theme_file()).is_ok());

    let linked = temp.path().join("hard-linked-schema");
    std::fs::write(&linked, "hard-linked content must stay unchanged").unwrap();
    std::fs::remove_file(layout.config_schema_file()).unwrap();
    std::fs::hard_link(&linked, layout.config_schema_file()).unwrap();
    refresh_schemas(&layout).unwrap();
    assert_eq!(
        std::fs::read_to_string(&linked).unwrap(),
        "hard-linked content must stay unchanged"
    );
    assert_eq!(
        std::fs::read_to_string(layout.config_schema_file()).unwrap(),
        CONFIG_SCHEMA
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let outside = temp.path().join("outside-schema");
        std::fs::write(&outside, "must stay unchanged").unwrap();
        std::fs::remove_file(layout.config_schema_file()).unwrap();
        symlink(&outside, layout.config_schema_file()).unwrap();
        refresh_schemas(&layout).unwrap();
        assert_eq!(
            std::fs::read_to_string(outside).unwrap(),
            "must stay unchanged"
        );
        assert_eq!(
            std::fs::read_to_string(layout.config_schema_file()).unwrap(),
            CONFIG_SCHEMA
        );
        assert!(
            !std::fs::symlink_metadata(layout.config_schema_file())
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}

#[test]
fn provider_api_is_explicit_and_sse_only() {
    let config = Config::parse(&config_value().to_string()).unwrap();
    assert_eq!(config.providers["work"].api.label(), "openai-responses-sse");
    for api in ["openai-responses", "openai-completions", "websocket"] {
        let mut value = config_value();
        value["providers"]["work"]["api"] = json!(api);
        assert!(Config::parse(&value.to_string()).is_err());
    }
    let mut value = config_value();
    value["providers"]["work"]
        .as_object_mut()
        .unwrap()
        .remove("api");
    assert!(Config::parse(&value.to_string()).is_err());
}

#[test]
fn configuration_rejects_literal_secrets_and_feature_toggles() {
    let mut value = config_value();
    value["providers"]["work"]["api_key"] = json!("do-not-echo-this-value");
    let error = Config::parse(&value.to_string()).unwrap_err();
    assert!(!error.contains("do-not-echo-this-value"));
    let mut value = config_value();
    value["todoEnabled"] = json!(false);
    assert!(Config::parse(&value.to_string()).is_err());
    let mut value = config_value();
    value["providers"]["work"]["models"][0]["compaction"]["enabled"] = json!(false);
    assert!(Config::parse(&value.to_string()).is_err());
}

#[test]
fn environment_names_and_model_budgets_are_validated() {
    let mut value = config_value();
    value["providers"]["work"]["base_url_env"] = json!("https://not-a-variable.invalid");
    assert!(Config::parse(&value.to_string()).is_err());
    let mut value = config_value();
    value["providers"]["work"]["models"][0]["compaction"]["reserveTokens"] = json!(128000);
    assert!(Config::parse(&value.to_string()).is_err());
    let mut value = config_value();
    value["providers"]["work"]["models"][0]["thinkingLevel"] = json!("high");
    assert!(Config::parse(&value.to_string()).is_err());

    let mut value = config_value();
    let model = &mut value["providers"]["work"]["models"][0];
    model["reasoning"] = json!(true);
    model["thinkingLevel"] = json!("high");
    model["thinkingLevelMap"] = json!({"off":"none","low":"low"});
    assert!(Config::parse(&value.to_string()).is_err());

    let mut value = config_value();
    let model = &mut value["providers"]["work"]["models"][0];
    model["reasoning"] = json!(true);
    model["thinkingLevel"] = json!("high");
    model["thinkingLevelMap"] = json!({"off":"none","high":"high"});
    assert!(Config::parse(&value.to_string()).is_ok());
}

#[test]
fn resolve_env_credentials_without_mutating_process_environment() {
    let config = Config::parse(&config_value().to_string()).unwrap();
    let provider = &config.providers["work"];
    let credentials = provider
        .resolve_with(|name| match name {
            "TEST_BASE" => Some("https://example.invalid/v1/".into()),
            "TEST_KEY" => Some("test-key".into()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        credentials.endpoint.as_str(),
        "https://example.invalid/v1/responses"
    );
    assert!(provider.resolve_with(|_| None).is_err());
    assert!(provider.resolve_with(|_| Some(String::new())).is_err());
}

#[test]
fn custom_theme_overrides_inherit_the_builtin_palette() {
    let themes =
        ThemeFile::parse(r##"{"themes":{"custom":{"colors":{"accent":"#abc","text":15}}}}"##)
            .unwrap();
    let palette = themes.resolve("custom").unwrap();
    assert_eq!(palette.accent, Color::Rgb(170, 187, 204));
    assert_eq!(palette.text, Color::Indexed(15));
    assert_eq!(palette.background, Color::Rgb(36, 41, 46));
    assert!(themes.resolve("unknown").is_err());
    assert!(ThemeFile::parse(r#"{"themes":{"bad":{"colors":{"accent":"not-a-color"}}}}"#).is_err());
}

#[test]
fn output_names_are_bounded_portable_and_cannot_escape_the_session() {
    let temp = tempfile::tempdir().unwrap();
    let outputs = temp.path().join("session/outputs");
    let mut names = std::collections::BTreeSet::new();
    for id in [
        "call_123".to_owned(),
        "../../secret".to_owned(),
        String::new(),
        "CON".to_owned(),
        "con".to_owned(),
        "NUL".to_owned(),
        "COM1".to_owned(),
        "LPT9".to_owned(),
        "trailing. ".to_owned(),
        "CaseID".to_owned(),
        "caseid".to_owned(),
        "X".repeat(252),
        "x".repeat(1024),
        "日本".to_owned(),
    ] {
        let name = output_name(&id);
        assert_eq!(name, output_name(&id));
        assert!(name.starts_with("output_") && name.ends_with(".log"));
        assert_eq!(name.len(), 75);
        assert!(name.bytes().all(|byte| byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'_' | b'.')));
        assert!(
            names.insert(name.to_ascii_lowercase()),
            "distinct IDs must not collide on a case-insensitive filesystem"
        );
        let path = outputs.join(&name);
        assert_eq!(path.parent(), Some(outputs.as_path()));
        write_new(&path, id.as_bytes()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), id.as_bytes());
    }
    assert_eq!(std::fs::read_dir(&outputs).unwrap().count(), names.len());
}

#[test]
fn private_storage_preserves_existing_files() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state/config.json");
    write_new(&path, b"first").unwrap();
    assert!(write_new(&path, b"second").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"first");
    let layout = Layout {
        root: temp.path().join("mh"),
    };
    assert_eq!(
        layout.config_file(),
        temp.path().join("mh/config/config.json")
    );
    assert_eq!(
        layout.workspace_dir(temp.path()).unwrap(),
        layout.workspace_dir(temp.path()).unwrap()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
