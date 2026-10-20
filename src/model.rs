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
use std::io::ErrorKind;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::credentials::Secret;
use crate::words;

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

/// The declared tools of a run: `shell`, plus the Forgejo tools when the run's context
/// says the agent can use them.
pub fn tools_with_forgejo(forgejo: bool) -> Vec<ToolSpec> {
    let mut declared = tools();
    if forgejo {
        declared.extend(forgejo_tools());
    }
    declared
}

/// The six Forgejo tools, with the limits their descriptions state.
///
/// The schemas are the model's whole contract: a repository is `owner/name`, a number
/// is the issue or pull-request number, and no call takes a token. The descriptions
/// say what the tool can touch and which bound applies, so the model is told the same
/// limits the tool enforces.
fn forgejo_tools() -> Vec<ToolSpec> {
    let repository = || {
        serde_json::json!({
            "type": "string",
            "description": "The repository as `owner/name`, cloned into your workspace. Any other repository is refused."
        })
    };
    let number = || {
        serde_json::json!({
            "type": "integer",
            "minimum": 1,
            "description": "The issue or pull-request number in that repository."
        })
    };
    vec![
        ToolSpec {
            kind: "function",
            function: FunctionSpec {
                name: crate::forgejo_tools::LIST_ASSIGNED_ISSUES,
                description: "List the open issues assigned to your own Forgejo account in the \
                    repositories cloned into your workspace: repository, number, title, labels \
                    and when each was updated. Read only; at most 50, and a cut is declared.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            },
        },
        ToolSpec {
            kind: "function",
            function: FunctionSpec {
                name: crate::forgejo_tools::READ_ISSUE_OR_PULL,
                description: "Read one issue or pull request, with its comments, from a \
                    repository cloned into your workspace. The body is cut at 8 KiB, a \
                    comment at 2 KiB, at most 20 comments, and the result says what was cut.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "repository": repository(),
                        "number": number()
                    },
                    "required": ["repository", "number"],
                    "additionalProperties": false
                }),
            },
        },
        ToolSpec {
            kind: "function",
            function: FunctionSpec {
                name: crate::forgejo_tools::COMMENT,
                description: "Write one comment on an issue or pull request in a repository \
                    cloned into your workspace, as your own Forgejo account. This is a \
                    write: it counts against this run's bound of 8 writes. At most 8 KiB.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "repository": repository(),
                        "number": number(),
                        "body": {
                            "type": "string",
                            "description": "The comment text, non-empty and at most 8192 bytes."
                        }
                    },
                    "required": ["repository", "number", "body"],
                    "additionalProperties": false
                }),
            },
        },
        ToolSpec {
            kind: "function",
            function: FunctionSpec {
                name: crate::forgejo_tools::LIST_MY_PULL_REQUESTS,
                description: "List your own open pull requests in the repositories cloned into \
                    your workspace: repository, number, title, head -> base and web URL. Read only.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }),
            },
        },
        ToolSpec {
            kind: "function",
            function: FunctionSpec {
                name: crate::forgejo_tools::OPEN_PULL_REQUEST,
                description: "Open a pull request from the checked-out branch of a repository \
                    cloned into your workspace into that repository's default branch. The branch \
                    must not be the base, must have commits the base lacks, must already be on \
                    the remote, and must have no open pull request for the pair; otherwise \
                    nothing is written and the result names the existing request. This is a \
                    write: it counts against this run's bound of 8 writes.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "repository": repository(),
                        "title": {
                            "type": "string",
                            "description": "The pull request's title, non-empty."
                        },
                        "body": {
                            "type": "string",
                            "description": "The pull request's body, when there is one."
                        }
                    },
                    "required": ["repository", "title"],
                    "additionalProperties": false
                }),
            },
        },
        ToolSpec {
            kind: "function",
            function: FunctionSpec {
                name: crate::forgejo_tools::UPDATE_PULL_REQUEST,
                description: "Change the title and/or body of your own open pull request for the \
                    checked-out branch of a repository cloned into your workspace. Only the \
                    agent's own open request whose head is that branch, whose base is the default \
                    branch and whose repository is the same may be updated; another account's \
                    request is never changed. This is a write: it counts against this run's \
                    bound of 8 writes.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "repository": repository(),
                        "number": number(),
                        "title": {
                            "type": "string",
                            "description": "The new title, when it should change; non-empty."
                        },
                        "body": {
                            "type": "string",
                            "description": "The new body, when it should change."
                        }
                    },
                    "required": ["repository", "number"],
                    "additionalProperties": false
                }),
            },
        },
    ]
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
                format!(
                    "asked for {}",
                    words::counted(self.tool_calls.len(), "tool call", "tool calls")
                )
            }
            _ => "answered with an empty message".to_owned(),
        };
        match self.usage {
            Some(usage) => format!(
                "{text}; {} used",
                words::counted(usage.total_tokens as usize, "token", "tokens")
            ),
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
    /// The endpoint's models list answered and does not carry the model. The model id
    /// is the field to correct; the completions path already answered. This sentence
    /// states the fact only: the screen's recovery clause is the one correction.
    #[error("the endpoint's models list does not include the model {model:?} (HTTP {status})")]
    ModelNotFound {
        /// The model the agent asked for.
        model: String,
        /// The status the endpoint answered with.
        status: u16,
    },
    /// The endpoint answered 404 at the configured path and its models list did not
    /// answer at all, so the model was never disproved. Only the path is named — the
    /// product must not claim a model fact it never learned. The correction is added
    /// once by the screen, not restated here.
    #[error("the endpoint has no completions route at {endpoint} (HTTP {status})")]
    RouteNotFound {
        /// The endpoint as configured.
        endpoint: String,
        /// The status the endpoint answered with.
        status: u16,
    },
    /// The endpoint answered 404 at the configured path while its models list did
    /// answer and did carry the model. The path is the only thing wrong. The fact
    /// that the model was listed is kept; the correction is added once by the screen.
    #[error(
        "the endpoint has no completions route at {endpoint} (HTTP {status}); the endpoint's models list does contain the model {model:?}"
    )]
    RouteNotFoundListed {
        /// The endpoint as configured.
        endpoint: String,
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
    ///
    /// The message is the provider's own, reduced to one bounded line; a reply that
    /// carries no message leaves the HTTP status as the whole fact, never the raw
    /// JSON envelope.
    #[error("the endpoint answered HTTP {status}{detail}", detail = if .message.is_empty() { String::new() } else { format!(": {}", .message) })]
    Server {
        /// The status the endpoint answered with.
        status: u16,
        /// The provider's message, bounded to one line, or empty.
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
    #[error("the endpoint at {endpoint} could not be reached: {message}")]
    Transport {
        /// The endpoint the request was sent to.
        endpoint: String,
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
            ModelError::RouteNotFound { .. } | ModelError::RouteNotFoundListed { .. } => {
                "route_not_found"
            }
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
        endpoint: config.endpoint.clone(),
        message: format!("the request cannot be written as JSON: {error}"),
    })?;

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(config.timeout))
        .http_status_as_error(false)
        .build()
        .new_agent();

    // A signal delivered while a blocking connect or read is in progress may show up
    // as EINTR. That is not the endpoint failing; try the same request again while the
    // exchange still has time. Any other transport error keeps its class.
    let mut interrupted = 0u8;
    let response = loop {
        let attempt = agent
            .post(endpoint)
            .header("Content-Type", "application/json")
            .header(
                "Authorization",
                format!("Bearer {}", config.api_key.expose()),
            )
            .send(request_body.clone());
        match attempt {
            Ok(response) => break response,
            Err(error) if is_interrupted(&error) => {
                if interrupted < 2 {
                    interrupted = interrupted.saturating_add(1);
                    continue;
                }
                return Err(ModelError::Timeout {
                    secs: config.timeout.as_secs(),
                });
            }
            Err(error) => return Err(transport_error(config, error)),
        }
    };

    let status = response.status().as_u16();
    let retry_after_secs = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let text = match response.into_body().read_to_string() {
        Ok(text) => text,
        Err(error) if is_interrupted(&error) => {
            // The body arrived but its read was cut short by a signal. The exchange
            // cannot be resumed, and calling it a transport failure would name a
            // cause the endpoint did not produce.
            return Err(ModelError::Timeout {
                secs: config.timeout.as_secs(),
            });
        }
        Err(error) => {
            return Err(ModelError::Transport {
                endpoint: config.endpoint.clone(),
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
            // A 404 is not proof that the model is unknown: it is equally what a base
            // URL or a missing `/v1` produces. Only say `model_not_found` when the
            // endpoint's models list answers and does not contain the id. A list that
            // cannot answer leaves the model unproved, so the route is the correction.
            return Err(match model_list_answer(config) {
                Some(false) => ModelError::ModelNotFound {
                    model: config.model.clone(),
                    status,
                },
                Some(true) => ModelError::RouteNotFoundListed {
                    endpoint: config.endpoint.clone(),
                    model: config.model.clone(),
                    status,
                },
                None => ModelError::RouteNotFound {
                    endpoint: config.endpoint.clone(),
                    status,
                },
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
                message: provider_message(&redacted_body).unwrap_or_default(),
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

/// The models URL that belongs to a completions URL.
///
/// A base URL gets `/models` appended; a full path has its last `/chat/completions`
/// replaced. Both are the same address family the configured endpoint names, so a
/// 404 can be classified without inventing a second host.
fn models_url(endpoint: &str) -> String {
    let trimmed = endpoint.trim().trim_end_matches('/');
    match trimmed.rfind("/chat/completions") {
        Some(at) => format!("{}/models", &trimmed[..at]),
        None => format!("{trimmed}/models"),
    }
}

/// Ask the endpoint's models list whether it carries this model.
///
/// `Some(true)` / `Some(false)` mean the list answered and the id was present / absent;
/// `None` means the list did not answer in a usable way, so `model_not_found` would be
/// a claim the product cannot support. The request carries the key only in its header,
/// and its body is never recorded.
fn model_list_answer(config: &ModelConfig) -> Option<bool> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(config.timeout.min(Duration::from_secs(10))))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let response = agent
        .get(&models_url(&config.endpoint))
        .header(
            "Authorization",
            format!("Bearer {}", config.api_key.expose()),
        )
        .call()
        .ok()?;
    if !(200..=299).contains(&response.status().as_u16()) {
        return None;
    }
    let text = response.into_body().read_to_string().ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let list = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .or_else(|| value.as_array())?;
    Some(list.iter().any(|entry| {
        entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| id == config.model)
            || entry.as_str().is_some_and(|id| id == config.model)
    }))
}

/// Whether a transport failure is the operating system's `EINTR`, not the endpoint's.
fn is_interrupted(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::Io(error) => error.kind() == ErrorKind::Interrupted,
        other => other.to_string().contains("Interrupted system call"),
    }
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
            endpoint: config.endpoint.clone(),
            message: config.api_key.redact(&other.to_string()),
        },
    }
}

/// The transport's own words, in the product's words where the meaning is known.
///
/// `io:` is the transport library's prefix and `(os error 111)` its errno; neither is
/// a word the product taught a zero-knowledge user (O-4). A screen reads a reason
/// through this; the run log and the transcript keep the transport's exact sentence,
/// which is what a person auditing the failure afterwards reads.
pub fn in_product_words(text: &str) -> String {
    // A run that reaches a missing model key records the credentials file and the
    // `[model_keys]` section in its audit trail; a screen a person acts from must not
    // (A-2, E-6, P-4). The product has its own key for exactly this, so the screen
    // sentence names the screen and the key, and the agent whose key is gone. The
    // run's own log and transcript keep the file's sentence: they are the audit trail.
    if let Some(name) = missing_model_key_agent(text) {
        return format!(
            "the model key for `{name}` is gone — press 5 and then k on the Credentials screen to add one"
        );
    }
    if text.contains("Connection refused") {
        "the endpoint refused the connection".to_owned()
    } else if text.contains("Connection reset") || text.contains("Connection aborted") {
        "the connection dropped".to_owned()
    } else if text.contains("lookup address information")
        || text.contains("Name or service not known")
    {
        "the address could not be resolved".to_owned()
    } else if text.contains("Peer disconnected") || text.contains("Broken pipe") {
        // S-4: the peer closed after the request was read. `the request could not be
        // sent` was false and `io:` is the transport library's prefix, so the whole
        // sentence is replaced with the product's own account of what happened.
        "the request was sent, but the endpoint closed the connection".to_owned()
    } else if text.contains("io: ") {
        // Any other `io:` cause keeps the transport's words but loses the library's
        // prefix, so no raw `io:` reaches a screen (S-4's class).
        text.replace("io: ", "")
    } else {
        text.to_owned()
    }
}

/// The agent a run's missing-model-key reason names, when it names one.
///
/// The credentials error reads `… records no model key for the agent `NAME`; add one
/// under [model_keys]`; the name is the part the screen's own sentence needs.
fn missing_model_key_agent(text: &str) -> Option<&str> {
    let marker = "no model key for the agent `";
    let start = text.find(marker)? + marker.len();
    let rest = &text[start..];
    let end = rest.find('`')?;
    Some(&rest[..end])
}

/// The provider's own message from an error reply, as one bounded line.
///
/// A JSON error envelope is read for `error.message`, `message` or a string
/// `error`; the envelope itself is never shown. A plain-text body is its own
/// message. A JSON body with no message is no message at all, so the screen keeps
/// the HTTP status as the whole fact.
fn provider_message(body: &str) -> Option<String> {
    let message = match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| value.get("message").and_then(serde_json::Value::as_str))
            .or_else(|| value.get("error").and_then(serde_json::Value::as_str))
            .map(str::to_owned)?,
        Err(_) => body.to_owned(),
    };
    let one_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        None
    } else {
        Some(excerpt(&one_line, 200))
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
    fn an_unknown_model_is_a_model_not_found_only_when_the_models_list_answers_without_it() {
        let stub = Stub::start(vec![
            StubReply::status(404, r#"{"error":"no such model"}"#),
            StubReply::json(r#"{"data":[{"id":"another-model"}]}"#),
        ]);
        let error = chat(&config(&stub.url()), &[Message::user("hi")], &[], None)
            .expect_err("the model is refused");
        assert_eq!(error.class(), "model_not_found");
        assert!(error.to_string().contains("example-model"));
        assert!(
            error.to_string().contains("does not include the model"),
            "the model failure does not state the model fact: {error}"
        );
        // The correction belongs to the screen once, not to the stored failure fact.
        assert!(
            !error.to_string().contains("the correction is"),
            "the model failure states its correction twice: {error}"
        );
        assert!(
            !error.to_string().contains("full completions path"),
            "the model failure still tells the person to change the endpoint path: {error}"
        );
        let requests = stub.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].method, "GET");
        assert!(requests[1].path.ends_with("/models"));
    }

    #[test]
    fn a_404_at_the_path_is_a_route_not_found_when_the_models_list_has_the_model() {
        let stub = Stub::start(vec![
            StubReply::status(404, r#"{"error":"no route"}"#),
            StubReply::json(r#"{"data":[{"id":"example-model"}]}"#),
        ]);
        let error = chat(&config(&stub.url()), &[Message::user("hi")], &[], None)
            .expect_err("the path is wrong");
        assert_eq!(error.class(), "route_not_found");
        assert!(
            error.to_string().contains("/v1/chat/completions"),
            "{error}"
        );
        assert!(
            !error.to_string().contains("full completions path"),
            "the route failure states the screen's correction itself: {error}"
        );
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
    fn the_transport_gutter_is_translated_for_the_screens_only() {
        // O-4: `io:` and `(os error N)` are the transport library's words, not the
        // product's. A screen reads the sentence through `in_product_words`; the
        // stored record keeps the transport's own words for an audit afterwards.
        assert_eq!(
            in_product_words("io: Connection refused (os error 111)"),
            "the endpoint refused the connection"
        );
        assert_eq!(
            in_product_words("io: Connection reset by peer (os error 104)"),
            "the connection dropped"
        );
        assert_eq!(
            in_product_words("io: failed to lookup address information: Name or service not known"),
            "the address could not be resolved"
        );
        // Anything the product does not know keeps the transport's sentence.
        assert_eq!(
            in_product_words("a frobnicator failed"),
            "a frobnicator failed"
        );
        // S-4: a dropped connection is the product's sentence, not the transport's.
        assert_eq!(
            in_product_words("the request could not be sent: io: Peer disconnected"),
            "the request was sent, but the endpoint closed the connection"
        );
        assert!(!in_product_words("an io: Peer disconnected").contains("io:"));
        assert_eq!(
            in_product_words("io: some other transport cause"),
            "some other transport cause"
        );
    }

    #[test]
    fn a_runs_missing_model_key_reads_as_the_products_own_key() {
        // E-6, P-4: the run's reason carries the file and the TOML section; every
        // screen that shows it must render the product's own key instead, and name
        // the agent whose key is gone.
        let reason = "the model key cannot be read: the credentials file /tmp/example/credentials.toml records no model key for the agent `beta`; add one under [model_keys]";
        let shown = in_product_words(reason);
        assert_eq!(
            shown,
            "the model key for `beta` is gone — press 5 and then k on the Credentials screen to add one"
        );
        assert!(!shown.contains("[model_keys]"), "{shown}");
        assert!(!shown.contains("credentials.toml"), "{shown}");
    }

    #[test]
    fn a_reply_summary_counts_with_the_right_noun() {
        // C-2: the record drew `asked for 1 tool call(s)` and `5 token(s) used`.
        let one = Reply {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call-1".to_owned(),
                kind: "function".to_owned(),
                function: FunctionCall {
                    name: "shell".to_owned(),
                    arguments: "{}".to_owned(),
                },
            }],
            usage: Some(Usage {
                prompt_tokens: 3,
                completion_tokens: 2,
                total_tokens: 5,
            }),
            finish_reason: Some("tool_calls".to_owned()),
            model: None,
            body: String::new(),
        };
        assert_eq!(one.summary(), "asked for 1 tool call; 5 tokens used");
        let two = Reply {
            tool_calls: vec![one.tool_calls[0].clone(), one.tool_calls[0].clone()],
            usage: Some(Usage {
                prompt_tokens: 1,
                completion_tokens: 0,
                total_tokens: 1,
            }),
            ..one.clone()
        };
        assert_eq!(two.summary(), "asked for 2 tool calls; 1 token used");
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
