//! Streaming inference behind a small trait, so the agent loop can be driven
//! by `genai` against LiteLLM in production and by a script in tests.

use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::{self, BoxStream};
use futures::{StreamExt, future};
use genai::adapter::AdapterKind;
use genai::chat::{
    CacheControl, ChatOptions, ChatRequest, ChatStreamEvent, MessageContent, StopReason,
};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{ModelIden, ServiceTarget};

use crate::config::{ApiFamily, Config, Credentials, expand_tilde};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ProviderError(pub String);

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Text(String),
    Reasoning(String),
    End(Completion),
}

/// The finished assistant message, as it must be replayed in later requests
/// (it carries reasoning signatures and tool calls as well as text).
#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub content: MessageContent,
    /// The response was cut off by the output token limit.
    pub truncated: bool,
}

pub type EventStream = BoxStream<'static, Result<StreamEvent, ProviderError>>;

pub trait Provider: Send + Sync {
    fn stream(&self, request: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>>;
}

pub struct GenaiProvider {
    client: genai::Client,
    api: ApiFamily,
    model: String,
    options: ChatOptions,
    api_key: String,
}

impl GenaiProvider {
    pub fn new(
        config: &Config,
        credentials: &Credentials,
        home: Option<&std::path::Path>,
    ) -> anyhow::Result<Self> {
        let kind = match config.api {
            ApiFamily::Messages => AdapterKind::Anthropic,
            ApiFamily::Responses => AdapterKind::OpenAIResp,
        };
        // genai appends `messages` / `responses` to the endpoint.
        let mut base_url = config.base_url.clone();
        if !base_url.ends_with('/') {
            base_url.push('/');
        }
        let api_key = credentials.inference.api_key.expose().to_owned();

        // Model names carry LiteLLM route prefixes (`anthropic/…`, `chatgpt/…`),
        // so the adapter is pinned by the configured API family rather than
        // inferred from the name.
        let resolver = {
            let api_key = api_key.clone();
            ServiceTargetResolver::from_resolver_fn(
                move |target: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
                    Ok(ServiceTarget {
                        endpoint: Endpoint::from_owned(base_url.clone()),
                        auth: AuthData::from_single(api_key.clone()),
                        model: ModelIden::new(kind, target.model.model_name),
                    })
                },
            )
        };
        let mut builder = genai::Client::builder().with_service_target_resolver(resolver);
        if let Some(ca_file) = &config.ca_file {
            let path = expand_tilde(ca_file, home);
            let pem = std::fs::read(&path)
                .map_err(|e| anyhow::anyhow!("cannot read ca_file {}: {e}", path.display()))?;
            let certificate = reqwest::Certificate::from_pem(&pem)
                .map_err(|e| anyhow::anyhow!("invalid ca_file {}: {e}", path.display()))?;
            let http = reqwest::Client::builder()
                .tls_certs_merge([certificate])
                .build()?;
            builder = builder.with_reqwest(http);
        }

        let mut options = ChatOptions::default()
            .with_max_tokens(config.max_tokens)
            .with_capture_content(true)
            .with_capture_tool_calls(true)
            .with_capture_reasoning_content(true)
            .with_stream_header_timeout(Duration::from_secs(120));
        if let Some(effort) = &config.reasoning_effort {
            options = options.with_reasoning_effort(effort.parse()?);
        }

        Ok(Self {
            client: builder.build()?,
            api: config.api,
            model: config.model.clone(),
            options,
            api_key,
        })
    }
}

impl Provider for GenaiProvider {
    fn stream(
        &self,
        mut request: ChatRequest,
    ) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        // Anthropic only caches up to explicit breakpoints; mark the newest
        // message so each request reuses the previous one's prefix.
        if self.api == ApiFamily::Messages
            && let Some(last) = request.messages.last_mut()
        {
            last.options = Some(CacheControl::Ephemeral.into());
        }
        Box::pin(async move {
            let key = self.api_key.clone();
            let response = self
                .client
                .exec_chat_stream(&self.model, request, Some(&self.options))
                .await
                .map_err(|e| ProviderError(redact(&describe(&e), &key)))?;

            let mut failed = false;
            let events = response
                .stream
                // genai keeps polling after most errors; the first one ends the turn.
                .take_while(move |event| {
                    let go = !failed;
                    failed |= event.is_err();
                    future::ready(go)
                })
                .filter_map(move |event| {
                    future::ready(match event {
                        Ok(ChatStreamEvent::Chunk(chunk)) => {
                            Some(Ok(StreamEvent::Text(chunk.content)))
                        }
                        Ok(ChatStreamEvent::ReasoningChunk(chunk)) => {
                            Some(Ok(StreamEvent::Reasoning(chunk.content)))
                        }
                        Ok(ChatStreamEvent::End(end)) => Some(Ok(StreamEvent::End(Completion {
                            content: end.captured_content.unwrap_or_default(),
                            truncated: matches!(
                                end.captured_stop_reason,
                                Some(StopReason::MaxTokens(_))
                            ),
                        }))),
                        Ok(_) => None,
                        Err(e) => Some(Err(ProviderError(redact(&describe(&e), &key)))),
                    })
                });
            Ok(events.boxed())
        })
    }
}

fn redact(message: &str, key: &str) -> String {
    if key.len() >= 8 {
        message.replace(key, "<redacted>")
    } else {
        message.to_owned()
    }
}

/// Renders a genai error as a short, readable message with a hint where the
/// cause is likely in the user's configuration.
pub fn describe(err: &genai::Error) -> String {
    use genai::Error as E;
    match err {
        E::WebStream { error, cause, .. } => {
            if let Some(inner) = error.downcast_ref::<genai::Error>() {
                return describe(inner);
            }
            match error.downcast_ref::<reqwest::Error>() {
                Some(e) => describe_transport(e),
                None => format!("Inference stream failed: {}", clip(cause, 500)),
            }
        }
        E::HttpError { status, body, .. } => describe_status(*status, body),
        E::WebModelCall { webc_error, .. } | E::WebAdapterCall { webc_error, .. } => {
            match webc_error {
                genai::webc::Error::ResponseFailedStatus { status, body, .. } => {
                    describe_status(*status, body)
                }
                genai::webc::Error::Reqwest(e) => describe_transport(e),
                other => format!(
                    "Inference request failed: {}",
                    clip(&other.to_string(), 500)
                ),
            }
        }
        E::ChatResponse { body, .. } => format!(
            "The model returned an error: {}",
            clip(&error_message(&body.to_string()), 500)
        ),
        other => format!("Inference failed: {}", clip(&other.to_string(), 500)),
    }
}

fn describe_transport(err: &reqwest::Error) -> String {
    let mut message = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    let hint = if message.contains("certificate") || message.contains("UnknownIssuer") {
        " (is the endpoint's CA trusted? see `ca_file` in config.toml)"
    } else if err.is_connect() || err.is_timeout() {
        " (check `base_url` in config.toml and your network)"
    } else {
        ""
    };
    format!(
        "Cannot reach the inference endpoint: {}{hint}",
        clip(&message, 500)
    )
}

fn describe_status(status: reqwest::StatusCode, body: &str) -> String {
    let message = clip(&error_message(body), 500);
    let hint = match status.as_u16() {
        401 | 403 => " (check `[inference] api_key` in credentials.toml)",
        404 => " (check `base_url` and `model` in config.toml)",
        400 if message.to_lowercase().contains("model") => " (check `model` in config.toml)",
        429 => " (rate limited; with an anthropic/* model, make sure `api = \"messages\"`)",
        _ => "",
    };
    format!("Inference request failed with HTTP {status}: {message}{hint}")
}

/// Pulls `error.message` (or `message`) out of a JSON error body.
fn error_message(body: &str) -> String {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.trim().to_owned();
    };
    let error = json.get("error").unwrap_or(&json);
    error
        .get("message")
        .and_then(|m| m.as_str())
        .or_else(|| error.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| body.trim().to_owned())
}

fn clip(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

/// A provider that plays back canned responses, for tests.
pub mod scripted {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use genai::chat::{ContentPart, ToolCall};
    use serde_json::Value;

    use super::*;

    #[derive(Debug, Clone)]
    pub enum Step {
        Text(String),
        ToolCall {
            id: String,
            name: String,
            args: Value,
        },
        /// Fails the stream at this point.
        Fail(String),
        /// Never produces another event.
        Hang,
    }

    pub fn text(s: &str) -> Step {
        Step::Text(s.to_owned())
    }

    pub fn tool_call(id: &str, name: &str, args: Value) -> Step {
        Step::ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            args,
        }
    }

    type Script = Result<Vec<Step>, ProviderError>;

    /// One scripted response per request; requests are recorded for inspection.
    #[derive(Clone, Default)]
    pub struct ScriptedProvider {
        responses: Arc<Mutex<VecDeque<Script>>>,
        pub requests: Arc<Mutex<Vec<ChatRequest>>>,
    }

    impl ScriptedProvider {
        pub fn new(responses: impl IntoIterator<Item = Vec<Step>>) -> Self {
            let provider = Self::default();
            for steps in responses {
                provider.push(steps);
            }
            provider
        }

        pub fn push(&self, steps: Vec<Step>) {
            self.responses.lock().unwrap().push_back(Ok(steps));
        }

        /// Makes the next request fail before streaming starts.
        pub fn push_error(&self, message: &str) {
            self.responses
                .lock()
                .unwrap()
                .push_back(Err(ProviderError(message.to_owned())));
        }
    }

    impl Provider for ScriptedProvider {
        fn stream(
            &self,
            request: ChatRequest,
        ) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
            self.requests.lock().unwrap().push(request);
            let next = self.responses.lock().unwrap().pop_front();
            Box::pin(async move {
                let steps =
                    next.unwrap_or_else(|| Err(ProviderError("no scripted response left".into())))?;
                let mut events = Vec::new();
                let mut parts = Vec::new();
                let mut text = String::new();
                let mut open = false;
                for step in steps {
                    match step {
                        Step::Text(t) => {
                            text.push_str(&t);
                            events.push(Ok(StreamEvent::Text(t)));
                        }
                        Step::ToolCall { id, name, args } => {
                            parts.push(ContentPart::ToolCall(ToolCall {
                                call_id: id,
                                fn_name: name,
                                fn_arguments: args,
                                thought_signatures: None,
                            }))
                        }
                        Step::Fail(message) => {
                            events.push(Err(ProviderError(message)));
                            open = true;
                            break;
                        }
                        Step::Hang => {
                            open = true;
                            break;
                        }
                    }
                }
                let hang = open && !events.last().is_some_and(|e| e.is_err());
                if !open {
                    let mut content = MessageContent::default();
                    if !text.is_empty() {
                        content.push(ContentPart::Text(text));
                    }
                    for part in parts {
                        content.push(part);
                    }
                    events.push(Ok(StreamEvent::End(Completion {
                        content,
                        truncated: false,
                    })));
                }
                let events = stream::iter(events);
                Ok(if hang {
                    events.chain(stream::pending()).boxed()
                } else {
                    events.boxed()
                })
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_errors_are_readable_with_hints() {
        let body = r#"{"error":{"message":"Authentication Error, Invalid proxy server token passed.","type":"auth_error","code":"401"}}"#;
        assert_eq!(
            describe_status(reqwest::StatusCode::UNAUTHORIZED, body),
            "Inference request failed with HTTP 401 Unauthorized: Authentication Error, Invalid proxy server token passed. (check `[inference] api_key` in credentials.toml)"
        );
        let body = r#"{"error":{"message":"Invalid model name passed in model=nope"}}"#;
        assert!(
            describe_status(reqwest::StatusCode::BAD_REQUEST, body)
                .ends_with("(check `model` in config.toml)")
        );
        assert_eq!(
            describe_status(reqwest::StatusCode::BAD_GATEWAY, "upstream down"),
            "Inference request failed with HTTP 502 Bad Gateway: upstream down"
        );
    }

    #[test]
    fn http_error_inside_stream_error_is_unwrapped() {
        let inner = genai::Error::HttpError {
            status: reqwest::StatusCode::NOT_FOUND,
            canonical_reason: "Not Found".into(),
            body: r#"{"message":"no route"}"#.into(),
            headers: Box::default(),
        };
        let err = genai::Error::WebStream {
            model_iden: ModelIden::new(AdapterKind::Anthropic, "m"),
            cause: inner.to_string(),
            error: Box::new(inner),
        };
        assert_eq!(
            describe(&err),
            "Inference request failed with HTTP 404 Not Found: no route (check `base_url` and `model` in config.toml)"
        );
    }

    #[test]
    fn keys_are_redacted() {
        assert_eq!(
            redact("bad key sk-12345678 here", "sk-12345678"),
            "bad key <redacted> here"
        );
    }
}
