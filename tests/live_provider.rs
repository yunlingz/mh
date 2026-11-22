//! Explicit opt-in live transport check. Never advertises or executes tools.

use std::path::Path;
use std::time::Duration;

use mh::config::Config;
use mh::message::{Message, OutputItem, StopReason};
use mh::providers::{Request, Responses};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn opt_in_live_stateless_reasoning_round_trip_is_tool_free() {
    let Some(path) = std::env::var_os("MH_LIVE_VERIFY_CONFIG") else {
        return;
    };
    let config = Config::load(Path::new(&path)).expect("live configuration must be valid");
    let (provider, settings, model) = config.model(&config.default_model).unwrap();
    if !model.reasoning {
        return;
    }
    let http = Responses::new().unwrap();
    let cancel = CancellationToken::new();
    let mut messages = vec![Message::user(
        "Remember the marker MH_STATELESS_FIRST. Reply with exactly MH_STATELESS_FIRST.",
    )];
    let first = tokio::time::timeout(
        Duration::from_secs(90),
        http.run(
            Request {
                provider,
                model,
                instructions: "Tools are unavailable. Preserve conversation continuity.",
                messages: &messages,
                tools: &[],
                estimated_input_tokens: 100,
                cache_key: None,
            },
            settings.resolve().expect("live environment must be set"),
            None,
            &cancel,
        ),
    )
    .await
    .expect("first live stateless turn timed out");
    assert!(first.failure.is_none());
    assert_eq!(first.response.stop, StopReason::Stop);
    assert_eq!(first.response.text().trim(), "MH_STATELESS_FIRST");
    assert!(first.response.items.iter().any(|item| matches!(item, OutputItem::Reasoning { encrypted_content: Some(content), .. } if !content.is_empty())), "provider must supply opaque reasoning for this reasoning-model check");
    messages.push(Message::Assistant {
        response: first.response,
    });
    messages.push(Message::user(
        "Reply with exactly the marker from the prior turn and no other text.",
    ));
    let second = tokio::time::timeout(
        Duration::from_secs(90),
        http.run(
            Request {
                provider,
                model,
                instructions: "Tools are unavailable. Preserve conversation continuity.",
                messages: &messages,
                tools: &[],
                estimated_input_tokens: 1000,
                cache_key: None,
            },
            settings.resolve().expect("live environment must be set"),
            None,
            &cancel,
        ),
    )
    .await
    .expect("second live stateless turn timed out");
    assert!(second.failure.is_none());
    assert_eq!(second.response.stop, StopReason::Stop);
    assert!(second.response.tool_calls().is_empty());
    assert_eq!(second.response.text().trim(), "MH_STATELESS_FIRST");
}

#[tokio::test]
async fn opt_in_live_responses_request_is_tool_free() {
    let Some(path) = std::env::var_os("MH_LIVE_VERIFY_CONFIG") else {
        return;
    };
    let config =
        Config::load(Path::new(&path)).expect("live verification configuration must be valid");
    let (provider, settings, model) = config.model(&config.default_model).unwrap();
    let messages = [Message::user(
        "Reply with exactly MH_TRANSPORT_VERIFIED and no other text.",
    )];
    let outcome = tokio::time::timeout(
        Duration::from_secs(90),
        Responses::new().unwrap().run(
            Request {
                provider,
                model,
                instructions: "Answer this transport verification prompt. Tools are unavailable.",
                messages: &messages,
                tools: &[],
                estimated_input_tokens: 100,
                cache_key: None,
            },
            settings
                .resolve()
                .expect("live verification environment variables must be set"),
            None,
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("live SSE request exceeded the verification deadline");
    assert_eq!(outcome.response.stop, StopReason::Stop);
    assert!(outcome.failure.is_none());
    assert!(outcome.response.tool_calls().is_empty());
    assert_eq!(outcome.response.text().trim(), "MH_TRANSPORT_VERIFIED");
}
