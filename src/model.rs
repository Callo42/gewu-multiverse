//! The model client: one direct request to the endpoint an agent names.
//!
//! There is no relay and no proxy: a run reads its agent's endpoint, model id and
//! key, and posts one chat request to that endpoint. The shape is the widely used
//! OpenAI-compatible chat-completions shape — a `model`, a list of `messages`, and a
//! list of `tools` — because that is what a self-hosted or vendor endpoint that a
//! person can name is most likely to speak.
//!
//! **The exchange is not streamed, deliberately.** A streaming reply would arrive as
//! many partial chunks, and the transcript has to hold *the reply as received* as one
//! append-only record. One non-streaming exchange is one request and one reply, which
//! makes the record exact, the error taxonomy small, and the loop a plain sequence of
//! turns. A streamed exchange can be added later without changing the loop or the
//! record: it would fill the same reply before the turn ends.
//!
//! Every failure has a class ([`ModelError::class`]), so a screen can say *what kind*
//! of failure it was without repeating the message. No error, and no request body,
//! ever carries the key: the key travels only in the `Authorization` header, which is
//! recorded as `Bearer <redacted>`.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::credentials::Secret;

/// How long one model exchange may take, unless the run has less time left.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the "test this endpoint" action waits. Short, because a person is
/// waiting for it and a wrong endpoint should say so quickly.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// The most tokens one reply may hold.
pub const DEFAULT_MAX_TOKENS: u32 = 2048;

/// The most reply body that is kept in a transcript line.
pub const MAX_BODY_BYTES: usize = 128 * 1024;

/// The most of a tool's output that is sent back to the model, and recorded.
pub const MAX_TOOL_OUTPUT: usize = 16 * 1024;

/// The name of the one tool an agent declares.
pub const SHELL_TOOL: &str = "shell";

/// One model conversation, ready to be written as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Message {
    /// `system`, `user`, `assistant` or `tool`.
    pub role: String,
    /// The text, when there is text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// The tool calls an assistant message asks for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Which tool call a tool message is answering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    /// The system message that sets the agent up.
    pub fn system(text: impl Into<String>) -> Self {
        Self::text("system", text)
    }

    /// A message from the person (or, here, the agent's instruction).
    pub fn user(text: impl Into<String>) -> Self {
        Self::text("user", text)
    }

    /// A message from the model.
    pub fn assistant(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".to_owned(),
            content,
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            tool_call_id: None,
        }
    }

    /// The result of one tool call, answering its id.
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_owned(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
        }
    }

    fn text(role: &str, text: impl Into<String>) -> Self {
        Self {
            role: role.to_owned(),
            content: Some(text.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

/// One function the model may ask to have called.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The model's own id for this call, echoed back with the result.
    pub id: String,
    /// Always `function`; kept so the wire shape round-trips.
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    /// The function and its arguments, as JSON text.
    pub function: FunctionCall,
}

/// The name and arguments of one tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionCall {
    /// The declared tool's name.
    pub name: String,
    /// The arguments, as the model wrote them: a JSON object, in text.
    pub arguments: String,
}

fn function_kind() -> String {
    "function".to_owned()
}

/// One tool an agent declares to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolSpec {
    /// Always `function`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// What the tool is and how to call it.
    pub function: FunctionSpec,
}

/// The description of one declared tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FunctionSpec {
    /// The tool's name.
    pub name: &'static str,
    /// What the tool does, in the model's words.
    pub description: &'static str,
    /// The JSON schema of the arguments.
    pub parameters: serde_json::Value,
}

/// The declared tools of every agent: exactly one, `shell`.
pub fn tools() -> Vec<ToolSpec> {
    vec![ToolSpec {
        kind: "function",
        function: FunctionSpec {
            name: SHELL_TOOL,
            description: "Run one shell command with the agent's workspace as its working directory. \
                 The command's output is returned to you. The workspace is a directory, not \
                 a sandbox: it is not confinement, and the command runs with the user's own \
                 rights.",
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to run, as one line handed to /bin/sh -c."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
    }]
}

/// What one exchange needs: where, which model, which key, how long to wait.
#[derive(Clone)]
pub struct ModelConfig {
    /// The endpoint the agent's configuration names.
    pub endpoint: String,
    /// The model id the agent's configuration names.
    pub model: String,
    /// The key, from the credentials file and nowhere else.
    pub api_key: Secret,
    /// How long the whole exchange may take.
    pub timeout: Duration,
}

/// The tokens a reply used, when the endpoint reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens in the request.
    #[serde(default)]
    pub prompt_tokens: u64,
    /// Tokens in the reply.
    #[serde(default)]
    pub completion_tokens: u64,
    /// Tokens in both.
    #[serde(default)]
    pub total_tokens: u64,
}

/// One model reply, as it came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The text, when the model wrote any.
    pub content: Option<String>,
    /// The tool calls the model asked for.
    pub tool_calls: Vec<ToolCall>,
    /// The tokens the endpoint reported.
    pub usage: Option<Usage>,
    /// Why the model stopped, when the endpoint says.
    pub finish_reason: Option<String>,
    /// The model id the endpoint answered with.
    pub model: Option<String>,
    /// The reply body exactly as it arrived.
    pub body: String,
}

impl Reply {
    /// The words a screen uses to describe this reply, briefly.
    pub fn summary(&self) -> String {
        let text = match &self.content {
            Some(text) if !text.trim().is_empty() => {
                format!("replied {:?}", excerpt(text, 80))
            }
            _ if !self.tool_calls.is_empty() => {
                format!("asked for {} tool call(s)", self.tool_calls.len())
            }
            _ => "answered with an empty message".to_owned(),
        };
        match self.usage {
            Some(usage) => format!("{text}; {} token(s) used", usage.total_tokens),
            None => text,
        }
    }
}

/// One exchange: the request as sent, and the reply as received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchange {
    /// The JSON body that was sent, exactly.
    pub request_body: String,
    /// The `Authorization` header as recorded, always with the key redacted.
    pub authorization: String,
    /// What came back.
    pub reply: Reply,
}

/// Why a model exchange did not produce a reply.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// The endpoint is not a usable URL.
    #[error("the endpoint {endpoint:?} is not a usable URL: {problem}")]
    BadUrl {
        /// The endpoint as configured.
        endpoint: String,
        /// What is wrong with it.
        problem: String,
    },
    /// The credentials file records no key for this agent.
    #[error("the credentials file records no model key for this agent")]
    MissingKey,
    /// The endpoint refused the key.
    #[error("the endpoint refused the key (HTTP {status})")]
    BadKey {
        /// The status the endpoint answered with.
        status: u16,
    },
    /// The endpoint does not serve that model, or the configured address is a base
    /// rather than the completions path a run POSTs to.
    #[error(
        "the endpoint does not serve the model {model:?} (HTTP {status}); if the configured address is a base URL, it may need the full completions path (usually /chat/completions)"
    )]
    ModelNotFound {
        /// The model the agent asked for.
        model: String,
        /// The status the endpoint answered with.
        status: u16,
    },
    /// The endpoint is rate-limiting the requests.
    #[error("the endpoint rate-limited the request (HTTP {status}{retry})", retry = match .retry_after_secs {
        Some(secs) => format!(", retry after {secs} s"),
        None => String::new(),
    })]
    RateLimited {
        /// The status the endpoint answered with.
        status: u16,
        /// The `Retry-After` the endpoint named, when it named one.
        retry_after_secs: Option<u64>,
    },
    /// The endpoint answered with some other error status.
    #[error("the endpoint answered HTTP {status}: {message}")]
    Server {
        /// The status the endpoint answered with.
        status: u16,
        /// A bounded, redacted excerpt of what it said.
        message: String,
    },
    /// The endpoint answered, but not with a usable chat completion.
    #[error("the reply is not a usable chat completion: {problem}")]
    MalformedReply {
        /// What was wrong with it.
        problem: String,
    },
    /// The exchange ran out of time.
    #[error("the request timed out after {secs} s")]
    Timeout {
        /// How long the exchange was allowed.
        secs: u64,
    },
    /// The request could not be sent at all.
    #[error("the request could not be sent: {message}")]
    Transport {
        /// What the transport said.
        message: String,
    },
}

impl ModelError {
    /// The class of failure, as one word a screen or a transcript can hold.
    pub fn class(&self) -> &'static str {
        match self {
            ModelError::BadUrl { .. } => "bad_url",
            ModelError::MissingKey => "missing_key",
            ModelError::BadKey { .. } => "bad_key",
            ModelError::ModelNotFound { .. } => "model_not_found",
            ModelError::RateLimited { .. } => "rate_limited",
            ModelError::Server { .. } => "server_error",
            ModelError::MalformedReply { .. } => "malformed_reply",
            ModelError::Timeout { .. } => "timeout",
            ModelError::Transport { .. } => "transport",
        }
    }
}

/// Make one non-streamed chat exchange.
pub fn chat(
    config: &ModelConfig,
    messages: &[Message],
    tools: &[ToolSpec],
    max_tokens: Option<u32>,
) -> Result<Exchange, ModelError> {
    let endpoint = config.endpoint.trim();
    if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
        return Err(ModelError::BadUrl {
            endpoint: endpoint.to_owned(),
            problem: "it must start with http:// or https://".to_owned(),
        });
    }
    if config.api_key.is_empty() {
        return Err(ModelError::MissingKey);
    }

    let mut body = serde_json::json!({
        "model": config.model,
        "messages": messages,
        "stream": false,
    });
    if let Some(object) = body.as_object_mut() {
        if !tools.is_empty() {
            object.insert("tools".to_owned(), serde_json::json!(tools));
        }
        if let Some(max_tokens) = max_tokens {
            object.insert("max_tokens".to_owned(), serde_json::json!(max_tokens));
        }
    }
    let request_body = serde_json::to_string(&body).map_err(|error| ModelError::Transport {
        message: format!("the request cannot be written as JSON: {error}"),
    })?;

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(config.timeout))
        .http_status_as_error(false)
        .build()
        .new_agent();

    let response = agent
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header(
            "Authorization",
            format!("Bearer {}", config.api_key.expose()),
        )
        .send(request_body.clone());

    let response = match response {
        Ok(response) => response,
        Err(error) => return Err(transport_error(config, error)),
    };

    let status = response.status().as_u16();
    let retry_after_secs = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let text = match response.into_body().read_to_string() {
        Ok(text) => text,
        Err(error) => {
            return Err(ModelError::Transport {
                message: config
                    .api_key
                    .redact(&format!("the reply body cannot be read: {error}")),
            });
        }
    };
    let redacted_body = config.api_key.redact(&text);

    match status {
        401 | 403 => {
            return Err(ModelError::BadKey { status });
        }
        404 => {
            return Err(ModelError::ModelNotFound {
                model: config.model.clone(),
                status,
            });
        }
        429 => {
            return Err(ModelError::RateLimited {
                status,
                retry_after_secs,
            });
        }
        status if !(200..=299).contains(&status) => {
            return Err(ModelError::Server {
                status,
                message: excerpt(&redacted_body, 400),
            });
        }
        _ => {}
    }

    let wire: WireReply =
        serde_json::from_str(&text).map_err(|error| ModelError::MalformedReply {
            problem: format!("the body is not JSON of the expected shape: {error}"),
        })?;
    let Some(choice) = wire.choices.into_iter().next() else {
        return Err(ModelError::MalformedReply {
            problem: "the reply holds no choices".to_owned(),
        });
    };
    let content = config
        .api_key
        .redact(choice.message.content.as_deref().unwrap_or(""));
    let content = if choice.message.content.is_some() {
        Some(content)
    } else {
        None
    };
    let tool_calls = choice
        .message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|mut call| {
            call.function.arguments = config.api_key.redact(&call.function.arguments);
            call
        })
        .collect();

    Ok(Exchange {
        request_body: config.api_key.redact(&request_body),
        authorization: format!("Bearer {}", crate::credentials::REDACTED),
        reply: Reply {
            content,
            tool_calls,
            usage: wire.usage,
            finish_reason: choice.finish_reason,
            model: wire.model,
            body: redacted_body,
        },
    })
}

/// Make the one minimal call the "test this endpoint" action asks for.
///
/// It is a real call to the configured endpoint, with the smallest possible request:
/// one short user message, no tools, and a handful of reply tokens. The failure class
/// is what makes a wrong endpoint or a wrong key plain.
pub fn probe(config: &ModelConfig) -> Result<Reply, ModelError> {
    let messages = vec![Message::user("Reply with the single word: ok")];
    let exchange = chat(config, &messages, &[], Some(4))?;
    Ok(exchange.reply)
}

/// The wire shape of a reply, as far as this client understands it.
#[derive(Debug, Deserialize)]
struct WireReply {
    #[serde(default)]
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
    message: WireMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
}

/// Turn a transport failure into a class, without ever naming the key.
fn transport_error(config: &ModelConfig, error: ureq::Error) -> ModelError {
    match error {
        ureq::Error::Timeout(_) => ModelError::Timeout {
            secs: config.timeout.as_secs(),
        },
        ureq::Error::BadUri(problem) => ModelError::BadUrl {
            endpoint: config.endpoint.clone(),
            problem,
        },
        ureq::Error::RequireHttpsOnly(_) => ModelError::BadUrl {
            endpoint: config.endpoint.clone(),
            problem: "only https:// is accepted".to_owned(),
        },
        ureq::Error::StatusCode(status) => ModelError::Server {
            status,
            message: "the endpoint answered with an error status".to_owned(),
        },
        other => ModelError::Transport {
            message: config.api_key.redact(&other.to_string()),
        },
    }
}

/// A bounded excerpt of some text, with a marker when it was cut.
pub fn excerpt(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} of {} bytes shown)", &text[..end], end, text.len())
}

impl fmt::Debug for ModelConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelConfig")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("api_key", &self.api_key)
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_stub::{Stub, StubReply};

    fn config(endpoint: &str) -> ModelConfig {
        ModelConfig {
            endpoint: endpoint.to_owned(),
            model: "example-model".to_owned(),
            api_key: Secret::new("sk-test-key-1234567890"),
            timeout: Duration::from_secs(5),
        }
    }

    const COMPLETION: &str = r#"{"id":"c1","model":"example-model",
        "choices":[{"index":0,"finish_reason":"stop",
        "message":{"role":"assistant","content":"done"}}],
        "usage":{"prompt_tokens":11,"completion_tokens":2,"total_tokens":13}}"#;

    #[test]
    fn a_normal_exchange_returns_the_reply_and_records_the_redacted_request() {
        let stub = Stub::start(vec![StubReply::json(COMPLETION)]);
        let messages = vec![Message::system("you are an agent"), Message::user("do it")];
        let exchange =
            chat(&config(&stub.url()), &messages, &tools(), Some(8)).expect("the stub answers");
        assert_eq!(exchange.reply.content.as_deref(), Some("done"));
        assert!(exchange.reply.tool_calls.is_empty());
        assert_eq!(exchange.reply.usage.map(|u| u.total_tokens), Some(13));
        assert_eq!(exchange.authorization, "Bearer <redacted>");
        assert!(
            exchange
                .request_body
                .contains("\"model\":\"example-model\"")
        );
        assert!(exchange.request_body.contains("you are an agent"));
        assert!(!exchange.request_body.contains("sk-test-key"));

        // The stub saw the real header, and the body exactly as recorded.
        let requests = stub.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].header("authorization").as_deref(),
            Some("Bearer sk-test-key-1234567890")
        );
        assert_eq!(requests[0].body, exchange.request_body);
        assert!(requests[0].body.contains("\"stream\":false"));
        assert!(requests[0].body.contains("\"max_tokens\":8"));
    }

    #[test]
    fn a_tool_call_round_trips_with_its_id_and_arguments() {
        let body = r#"{"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant",
            "content":null,"tool_calls":[{"id":"call_7","type":"function",
            "function":{"name":"shell","arguments":"{\"command\":\"ls\"}"}}]}}]}"#;
        let stub = Stub::start(vec![StubReply::json(body)]);
        let exchange = chat(
            &config(&stub.url()),
            &[Message::user("look around")],
            &tools(),
            None,
        )
        .expect("the stub answers");
        assert_eq!(exchange.reply.tool_calls.len(), 1);
        let call = &exchange.reply.tool_calls[0];
        assert_eq!(call.id, "call_7");
        assert_eq!(call.function.name, SHELL_TOOL);
        assert_eq!(call.function.arguments, "{\"command\":\"ls\"}");
        assert_eq!(exchange.reply.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn a_reply_that_is_not_json_is_a_malformed_reply() {
        let stub = Stub::start(vec![StubReply::json("this is not JSON at all")]);
        let error = chat(&config(&stub.url()), &[Message::user("hi")], &[], None)
            .expect_err("the reply is refused");
        assert_eq!(error.class(), "malformed_reply");
    }

    #[test]
    fn a_reply_with_no_choices_is_a_malformed_reply() {
        let stub = Stub::start(vec![StubReply::json(r#"{"choices":[]}"#)]);
        let error = chat(&config(&stub.url()), &[Message::user("hi")], &[], None)
            .expect_err("the reply is refused");
        assert_eq!(error.class(), "malformed_reply");
    }

    #[test]
    fn a_refused_key_is_a_bad_key() {
        let stub = Stub::start(vec![StubReply::status(401, r#"{"error":"invalid key"}"#)]);
        let error = chat(&config(&stub.url()), &[Message::user("hi")], &[], None)
            .expect_err("the key is refused");
        assert_eq!(error.class(), "bad_key");
        assert!(error.to_string().contains("401"));
    }

    #[test]
    fn an_unknown_model_is_a_model_not_found() {
        let stub = Stub::start(vec![StubReply::status(404, r#"{"error":"no such model"}"#)]);
        let error = chat(&config(&stub.url()), &[Message::user("hi")], &[], None)
            .expect_err("the model is refused");
        assert_eq!(error.class(), "model_not_found");
        assert!(error.to_string().contains("example-model"));
        // D-3: a 404 is also what a base URL produces, so the sentence must say the
        // path may be the problem, not only the model.
        assert!(error.to_string().contains("/chat/completions"), "{error}");
    }

    #[test]
    fn a_rate_limited_endpoint_is_reported_with_its_retry_after() {
        let stub = Stub::start(vec![StubReply {
            status: 429,
            body: r#"{"error":"slow down"}"#.to_owned(),
            retry_after_secs: Some(7),
            delay: Duration::ZERO,
        }]);
        let error = chat(&config(&stub.url()), &[Message::user("hi")], &[], None)
            .expect_err("the request is refused");
        assert_eq!(error.class(), "rate_limited");
        assert!(error.to_string().contains("7 s"), "{error}");
    }

    #[test]
    fn a_server_error_carries_its_status_and_a_bounded_message() {
        let stub = Stub::start(vec![StubReply::status(500, "the server fell over")]);
        let error = chat(&config(&stub.url()), &[Message::user("hi")], &[], None)
            .expect_err("the request fails");
        assert_eq!(error.class(), "server_error");
        assert!(error.to_string().contains("500"));
        assert!(error.to_string().contains("the server fell over"));
    }

    #[test]
    fn a_slow_endpoint_times_out_and_says_how_long_it_waited() {
        let stub = Stub::start(vec![StubReply {
            status: 200,
            body: COMPLETION.to_owned(),
            retry_after_secs: None,
            delay: Duration::from_secs(30),
        }]);
        let mut config = config(&stub.url());
        config.timeout = Duration::from_millis(750);
        let error =
            chat(&config, &[Message::user("hi")], &[], None).expect_err("the request times out");
        assert_eq!(error.class(), "timeout");
    }

    #[test]
    fn an_endpoint_that_is_not_a_url_is_a_bad_url() {
        let error = chat(
            &config("model.example.org/v1"),
            &[Message::user("hi")],
            &[],
            None,
        )
        .expect_err("the endpoint is refused");
        assert_eq!(error.class(), "bad_url");
        assert!(!error.to_string().contains("sk-test"));
    }

    #[test]
    fn an_empty_key_is_refused_before_any_request_is_sent() {
        let stub = Stub::start(vec![StubReply::json(COMPLETION)]);
        let mut config = config(&stub.url());
        config.api_key = Secret::empty();
        let error =
            chat(&config, &[Message::user("hi")], &[], None).expect_err("the empty key is refused");
        assert_eq!(error.class(), "missing_key");
        assert_eq!(stub.requests().len(), 0, "nothing was sent");
    }

    #[test]
    fn the_key_is_never_in_an_error_message() {
        for reply in [
            StubReply::status(401, "no"),
            StubReply::status(500, "no"),
            StubReply::json("not json"),
        ] {
            let stub = Stub::start(vec![reply]);
            let error = chat(
                &config(&stub.url()),
                &[Message::user("sk-test-key-1234567890")],
                &[],
                None,
            )
            .expect_err("the request fails");
            let message = format!("{error} {error:?}");
            assert!(!message.contains("sk-test-key"), "{message}");
        }
    }
}
