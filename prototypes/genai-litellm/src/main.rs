//! Throwaway probe: genai against BB3's LiteLLM on the Messages and Responses routes.
//!
//! genai-proto <messages|responses> <model> <stream|tools|cancel> [--effort low|medium|high]
//!
//! Env: LITELLM_API_KEY (required), LITELLM_BASE_URL (default https://litellm.bb3.internal/v1/),
//! CA_FILE=<pem> adds a trust anchor; RAW_FRAMES=1 dumps every SSE frame to stderr; NO_FALLBACK=1 disables LiteLLM router fallbacks.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, ContentPart, MessageContent,
    ReasoningEffort, Tool, ToolResponse,
};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        bail!("usage: genai-proto <messages|responses> <model> <stream|tools|cancel> [--effort LEVEL]");
    }
    let kind = match args[1].as_str() {
        "messages" => AdapterKind::Anthropic,
        "responses" => AdapterKind::OpenAIResp,
        other => bail!("unknown api {other}"),
    };
    let model = args[2].clone();
    let effort = args
        .iter()
        .position(|a| a == "--effort")
        .and_then(|i| args.get(i + 1))
        .map(|e| match e.as_str() {
            "low" => ReasoningEffort::Low,
            "medium" => ReasoningEffort::Medium,
            "high" => ReasoningEffort::High,
            _ => ReasoningEffort::Zero,
        });

    let client = client(kind)?;
    let mut opts = ChatOptions::default()
        .with_max_tokens(2048)
        .with_capture_content(true)
        .with_capture_tool_calls(true)
        .with_capture_reasoning_content(true)
        .with_capture_usage(true);
    if let Some(e) = effort {
        opts = opts.with_reasoning_effort(e);
    }
    // LiteLLM-specific: surface the primary route's own error instead of silently falling back.
    if std::env::var("NO_FALLBACK").is_ok() {
        opts = opts.with_extra_body(json!({ "disable_fallbacks": true }));
    }
    if std::env::var("RAW_FRAMES").is_ok() {
        opts = opts.with_raw_frame_fn(|_ctx, frame| eprintln!("[raw] {frame:?}"));
    }

    eprintln!("== api={} model={model} scenario={}", args[1], args[3]);
    match args[3].as_str() {
        "stream" => stream(&client, &model, &opts).await,
        "tools" => tools(&client, &model, &opts).await,
        "cancel" => cancel(&client, &model, &opts).await,
        other => bail!("unknown scenario {other}"),
    }
}

/// Every model name, whatever its provider prefix, goes to LiteLLM on the adapter chosen by
/// the configured API family — never genai's name-based adapter inference.
fn client(kind: AdapterKind) -> Result<Client> {
    let key = std::env::var("LITELLM_API_KEY").context("LITELLM_API_KEY")?;
    let base = std::env::var("LITELLM_BASE_URL")
        .unwrap_or_else(|_| "https://litellm.bb3.internal/v1/".into());
    let resolver = ServiceTargetResolver::from_resolver_fn(
        move |t: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
            Ok(ServiceTarget {
                endpoint: Endpoint::from_owned(base.clone()),
                auth: AuthData::from_single(key.clone()),
                model: ModelIden::new(kind, t.model.model_name),
            })
        },
    );
    let mut builder = Client::builder().with_service_target_resolver(resolver);
    // Optional explicit trust anchor (e.g. ca/bb3_root_ca.crt) on top of the platform verifier.
    if let Ok(path) = std::env::var("CA_FILE") {
        let pem = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
        let http = reqwest::Client::builder()
            .tls_certs_merge([reqwest::Certificate::from_pem(&pem)?])
            .build()?;
        builder = builder.with_reqwest(http);
    }
    Ok(builder.build()?)
}

struct Turn {
    content: MessageContent,
    tool_calls: usize,
    elapsed: Duration,
}

async fn run_turn(client: &Client, model: &str, req: ChatRequest, opts: &ChatOptions) -> Result<Turn> {
    let t0 = Instant::now();
    let mut res = client.exec_chat_stream(model, req, Some(opts)).await?;
    let mut first = None;
    let mut out = std::io::stdout();
    while let Some(ev) = res.stream.next().await {
        match ev? {
            ChatStreamEvent::Start => {}
            ChatStreamEvent::Chunk(c) => {
                first.get_or_insert(t0.elapsed());
                print!("{}", c.content);
                out.flush()?;
            }
            ChatStreamEvent::ReasoningChunk(c) => {
                first.get_or_insert(t0.elapsed());
                print!("\x1b[2m{}\x1b[0m", c.content);
                out.flush()?;
            }
            ChatStreamEvent::ThoughtSignatureChunk(c) => {
                eprintln!("\n  [thought-signature chunk, {} bytes]", c.content.len());
            }
            ChatStreamEvent::ToolCallChunk(c) => {
                eprintln!(
                    "\n  [tool-call chunk id={} fn={} args={}]",
                    c.tool_call.call_id, c.tool_call.fn_name, c.tool_call.fn_arguments
                );
            }
            ChatStreamEvent::Heartbeat => {}
            ChatStreamEvent::End(end) => {
                println!();
                eprintln!(
                    "  [end: stop={:?} response_id={:?} first_token={:?} total={:?}]",
                    end.captured_stop_reason,
                    end.captured_response_id,
                    first,
                    t0.elapsed()
                );
                if let Some(u) = &end.captured_usage {
                    eprintln!(
                        "  [usage: prompt={:?} completion={:?} reasoning={:?}]",
                        u.prompt_tokens,
                        u.completion_tokens,
                        u.completion_tokens_details.as_ref().and_then(|d| d.reasoning_tokens)
                    );
                }
                let content = end.captured_content.unwrap_or_default();
                let tool_calls = content.tool_calls().len();
                let kinds: Vec<&str> = content.parts().iter().map(part_kind).collect();
                eprintln!("  [captured parts: {kinds:?}]");
                return Ok(Turn { content, tool_calls, elapsed: t0.elapsed() });
            }
        }
    }
    bail!("stream ended without End event")
}

fn part_kind(p: &ContentPart) -> &'static str {
    match p {
        ContentPart::Text(_) => "text",
        ContentPart::ToolCall(_) => "tool_call",
        ContentPart::ToolResponse(_) => "tool_response",
        ContentPart::ThoughtSignature(_) => "thought_signature",
        ContentPart::ReasoningContent(_) => "reasoning",
        _ => "other",
    }
}

async fn stream(client: &Client, model: &str, opts: &ChatOptions) -> Result<()> {
    let req = ChatRequest::new(vec![ChatMessage::user(
        "In three short sentences: why can a borrow checker reject code that would be memory safe?",
    )])
    .with_system("You are terse.");
    let turn = run_turn(client, model, req, opts).await?;
    eprintln!("OK stream in {:?}", turn.elapsed);
    Ok(())
}

fn read_file_tool() -> Tool {
    Tool::new("read_file")
        .with_description("Read a file from the user's project, relative to the project root.")
        .with_schema(json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }))
}

const FAKE_CARGO_TOML: &str = "[package]\nname = \"saddle\"\nversion = \"0.0.1\"\nedition = \"2024\"\n\n[dependencies]\nratatui = \"0.30\"\ntokio = { version = \"1\", features = [\"full\"] }\n";

async fn tools(client: &Client, model: &str, opts: &ChatOptions) -> Result<()> {
    let mut req = ChatRequest::new(vec![ChatMessage::user(
        "Which Rust edition and which dependencies does this project use? Look at Cargo.toml with the tool; don't guess.",
    )])
    .with_system("You are a read-only code assistant. Use tools to inspect files.")
    .with_tools(vec![read_file_tool()]);

    let mut rounds = 0;
    loop {
        rounds += 1;
        if rounds > 4 {
            bail!("too many tool rounds");
        }
        let turn = run_turn(client, model, req.clone(), opts).await?;
        if turn.tool_calls == 0 {
            eprintln!("OK tools: final answer after {} model call(s)", rounds);
            return Ok(());
        }
        let calls: Vec<_> = turn.content.tool_calls().into_iter().cloned().collect();
        req = req.append_message(ChatMessage::assistant(turn.content));
        for call in calls {
            let path = call.fn_arguments.get("path").and_then(|p| p.as_str()).unwrap_or("");
            eprintln!("  -> executing read_file({path}) for call_id={}", call.call_id);
            let body = if path.trim_start_matches("./") == "Cargo.toml" {
                FAKE_CARGO_TOML.to_string()
            } else {
                format!("error: no such file: {path}")
            };
            req = req.append_message(ToolResponse::new(call.call_id, body));
        }
    }
}

/// Drops the stream after the first ~40 visible chunks, then proves the client is still usable.
async fn cancel(client: &Client, model: &str, opts: &ChatOptions) -> Result<()> {
    let req = ChatRequest::new(vec![ChatMessage::user(
        "Count from 1 to 400, one number per line, no other text.",
    )]);
    let t0 = Instant::now();
    let mut res = client.exec_chat_stream(model, req, Some(opts)).await?;
    let mut chunks = 0;
    while let Some(ev) = res.stream.next().await {
        if let ChatStreamEvent::Chunk(c) = ev? {
            print!("{}", c.content.replace('\n', " "));
            std::io::stdout().flush()?;
            chunks += 1;
            if chunks >= 40 {
                break;
            }
        }
    }
    drop(res);
    println!();
    eprintln!("  [cancelled after {chunks} chunks at {:?}]", t0.elapsed());

    let follow = ChatRequest::new(vec![ChatMessage::user("Reply with just the word: pong")]);
    let turn = run_turn(client, model, follow, opts).await?;
    eprintln!("OK cancel: follow-up succeeded in {:?}", turn.elapsed);
    Ok(())
}
