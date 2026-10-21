//! The model test's four provider answers, from a loopback stub.
//!
//! The suffix and the empty key are checked before a request; a provider's 404,
//! 401, unknown model and timeout are checked after one, and each line names the
//! provider's status and the first clause of its own message (W31-2, F1).

use std::time::Duration;

use gewu_multiverse::credentials::Secret;
use gewu_multiverse::model::{self, ModelConfig, ModelError};
use gewu_multiverse::test_stub::{Stub, StubReply};

fn config(endpoint: String) -> ModelConfig {
    ModelConfig {
        endpoint,
        model: "demo-model".to_owned(),
        api_key: Secret::new("sk-test-key"),
        timeout: Duration::from_secs(2),
    }
}

#[test]
fn a_missing_completions_suffix_is_refused_before_any_request() {
    let stub = Stub::start(vec![StubReply::json("{}")]);
    let base = stub.url().replace("/v1/chat/completions", "");
    let error = model::chat(&config(base), &[model::Message::user("hi")], &[], None)
        .expect_err("a path without /chat/completions is refused");
    assert_eq!(error.class(), "bad_address");
    assert_eq!(
        model::failure_words(&error, "demo-model"),
        "wrong address — it should end in /chat/completions"
    );
    assert_eq!(stub.requests().len(), 0, "a request was sent");
}

#[test]
fn a_provider_404_shows_its_status_and_message_and_names_the_v1_correction() {
    let stub = Stub::start(vec![
        StubReply::status(404, r#"{"error":{"message":"Add /v1 to the base URL."}}"#),
        StubReply::status(404, "no models route"),
    ]);
    // The address ends in /chat/completions but has no /v1, the mistake the walk
    // reproduced; the provider's own 404 body is the fact on screen.
    let endpoint = stub
        .url()
        .replace("/v1/chat/completions", "/chat/completions");
    let error = model::chat(&config(endpoint), &[model::Message::user("hi")], &[], None)
        .expect_err("the provider refuses the path");
    assert_eq!(error.class(), "route_not_found");
    let words = model::failure_words(&error, "demo-model");
    assert!(words.contains("HTTP 404"), "{words}");
    assert!(words.contains("Add /v1 to the base URL"), "{words}");
    // The repair is first: at 80x24 the end of this line is what the form loses
    // (review B1).
    assert!(words.starts_with("add /v1 — "), "{words}");
    assert_eq!(stub.requests().len(), 2, "the completion and models calls");
    assert_eq!(stub.requests()[0].method, "POST");
    assert_eq!(stub.requests()[0].path, "/chat/completions");
}

#[test]
fn a_refused_key_names_the_provider_status() {
    let stub = Stub::start(vec![StubReply::status(401, r#"{"error":"invalid key"}"#)]);
    let error = model::chat(
        &config(stub.url()),
        &[model::Message::user("hi")],
        &[],
        None,
    )
    .expect_err("the key is refused");
    assert_eq!(error.class(), "bad_key");
    let words = model::failure_words(&error, "demo-model");
    assert!(words.contains("HTTP 401"), "{words}");
    assert!(words.contains("API key"), "{words}");
}

#[test]
fn an_unknown_model_names_the_model_and_the_status() {
    let stub = Stub::start(vec![
        StubReply::status(404, r#"{"error":"no such model"}"#),
        StubReply::json(r#"{"data":[{"id":"another-model"}]}"#),
    ]);
    let error = model::chat(
        &config(stub.url()),
        &[model::Message::user("hi")],
        &[],
        None,
    )
    .expect_err("the model is refused");
    assert_eq!(error.class(), "model_not_found");
    let words = model::failure_words(&error, "demo-model");
    assert!(words.contains("no model called demo-model"), "{words}");
    assert!(words.contains("HTTP 404"), "{words}");
}

#[test]
fn a_timeout_says_how_long_it_waited() {
    let stub = Stub::start(vec![StubReply {
        status: 200,
        body: "{}".to_owned(),
        retry_after_secs: None,
        delay: Duration::from_secs(3),
    }]);
    let mut config = config(stub.url());
    config.timeout = Duration::from_secs(1);
    let error = model::chat(&config, &[model::Message::user("hi")], &[], None)
        .expect_err("the provider does not answer in time");
    assert_eq!(error.class(), "timeout");
    assert_eq!(
        model::failure_words(&error, "demo-model"),
        "no answer within 1 s"
    );
}

#[test]
fn an_empty_key_is_its_own_line_and_sends_nothing() {
    let stub = Stub::start(vec![StubReply::json("{}")]);
    let mut config = config(stub.url());
    config.api_key = Secret::empty();
    let error = model::chat(&config, &[model::Message::user("hi")], &[], None)
        .expect_err("the key is empty");
    assert_eq!(error.class(), "missing_key");
    assert_eq!(
        error_class_line(&error),
        "the API key is empty",
        "an empty key is not a provider refusal"
    );
    assert_eq!(stub.requests().len(), 0);
}

fn error_class_line(error: &ModelError) -> String {
    model::failure_words(error, "demo-model")
}
