//! The conversation and its turn loop: stream a model response, run the tool
//! calls it makes, feed the results back, and repeat until the model stops.

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use genai::chat::{ChatMessage, ChatRequest, MessageContent, ToolResponse};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::events::{AgentEvent, TurnOutcome};
use crate::provider::{Provider, StreamEvent};
use crate::tools::Registry;

/// Model responses per turn before the loop gives up and hands back to the user.
pub const MAX_ROUNDS: usize = 50;

/// How long an interruptible tool may take to wrap up after an interrupt.
const INTERRUPT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

const INTERRUPTED_TOOL: &str = "Interrupted: the user stopped this turn before the tool finished.";

pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: Registry,
    system: String,
    messages: Vec<ChatMessage>,
    events: mpsc::UnboundedSender<AgentEvent>,
    session: Option<crate::session::Session>,
    context: crate::context::ContextBudget,
    storage_failed: bool,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Registry,
        system: String,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> Self {
        Self {
            provider,
            tools,
            system,
            messages: Vec::new(),
            events,
            session: None,
            context: crate::context::ContextBudget::new(128_000),
            storage_failed: false,
        }
    }

    pub fn with_session(
        mut self,
        session: crate::session::Session,
        messages: Vec<ChatMessage>,
    ) -> Self {
        self.session = Some(session);
        self.messages = messages;
        self
    }

    pub fn with_context_window(mut self, tokens: usize) -> Self {
        self.context = crate::context::ContextBudget::new(tokens);
        self
    }

    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    fn reset(&mut self) {
        self.messages.clear();
        self.context.reset();
        self.storage_failed = false;
    }

    fn emit(&self, event: AgentEvent) {
        let _ = self.events.send(event);
    }

    fn persist(&mut self, event: crate::session::LogEvent) -> bool {
        if self.storage_failed {
            return false;
        }
        if let Some(session) = &self.session
            && let Err(error) = session.append(event)
        {
            self.storage_failed = true;
            self.emit(AgentEvent::Error(format!(
                "Cannot save conversation: {error}"
            )));
            return false;
        }
        true
    }

    fn append(&mut self, message: ChatMessage) -> bool {
        if !self.persist(crate::session::LogEvent::Model(message.clone())) {
            return false;
        }
        self.messages.push(message);
        true
    }

    pub async fn submit(&mut self, text: String, cancel: &CancellationToken) -> TurnOutcome {
        self.emit(AgentEvent::TurnStarted);
        let outcome = if self.storage_failed {
            self.emit(AgentEvent::Error(
                "Conversation storage failed; restart before continuing.".into(),
            ));
            TurnOutcome::Failed
        } else if self.append(ChatMessage::user(text)) {
            self.run_turn(cancel).await
        } else {
            TurnOutcome::Failed
        };
        self.emit(AgentEvent::TurnFinished(outcome));
        outcome
    }

    async fn run_turn(&mut self, cancel: &CancellationToken) -> TurnOutcome {
        let mut retried = false;
        let mut force_truncate = false;
        for _ in 0..MAX_ROUNDS {
            let completion = 'request: loop {
                if cancel.is_cancelled() {
                    return TurnOutcome::Interrupted;
                }
                let mut request = ChatRequest::new(self.messages.clone())
                    .with_system(self.system.clone())
                    .with_tools(self.tools.definitions());
                if self.context.prepare(&mut request, force_truncate) {
                    self.emit(AgentEvent::Notice("Context shortened: older messages omitted from this request; full history is saved.".into()));
                }
                force_truncate = false;
                let stream = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return TurnOutcome::Interrupted,
                    stream = self.provider.stream(request) => stream,
                };
                let mut stream = match stream {
                    Ok(stream) => stream,
                    Err(e) if e.is_context_length() && !retried => {
                        retried = true;
                        force_truncate = true;
                        continue 'request;
                    }
                    Err(e) => {
                        self.emit(AgentEvent::Error(e.0));
                        return TurnOutcome::Failed;
                    }
                };
                let mut text = String::new();
                let mut thinking = false;
                loop {
                    let event = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            if !self.keep_partial(text) { return TurnOutcome::Failed; }
                            self.emit(AgentEvent::AssistantDone { interrupted: true });
                            return TurnOutcome::Interrupted;
                        }
                        event = stream.next() => event,
                    };
                    match event {
                        Some(Ok(StreamEvent::Text(delta))) => {
                            if !self
                                .persist(crate::session::LogEvent::AssistantDelta(delta.clone()))
                            {
                                return TurnOutcome::Failed;
                            }
                            text.push_str(&delta);
                            self.emit(AgentEvent::AssistantDelta(delta));
                        }
                        Some(Ok(StreamEvent::Reasoning(_))) => {
                            if !thinking {
                                thinking = true;
                                self.emit(AgentEvent::Thinking);
                            }
                        }
                        Some(Ok(StreamEvent::End(completion))) => break 'request completion,
                        Some(Err(e))
                            if e.is_context_length()
                                && !retried
                                && text.is_empty()
                                && !thinking =>
                        {
                            retried = true;
                            force_truncate = true;
                            continue 'request;
                        }
                        Some(Err(e)) => {
                            self.keep_partial(text);
                            self.emit(AgentEvent::Error(e.0));
                            return TurnOutcome::Failed;
                        }
                        None => {
                            self.keep_partial(text);
                            self.emit(AgentEvent::Error(
                                "The inference stream ended without finishing the response.".into(),
                            ));
                            return TurnOutcome::Failed;
                        }
                    }
                }
            };
            self.emit(AgentEvent::AssistantDone { interrupted: false });
            if let Some(prompt_tokens) = completion.prompt_tokens {
                self.context.observe(prompt_tokens);
                self.emit(AgentEvent::Usage { prompt_tokens });
            }
            if completion.truncated {
                self.emit(AgentEvent::Notice(
                    "The response hit the output token limit (`max_tokens` in config.toml).".into(),
                ));
            }
            let calls: Vec<_> = completion
                .content
                .tool_calls()
                .into_iter()
                .cloned()
                .collect();
            if !completion.content.is_empty()
                && !self.append(ChatMessage::assistant(completion.content))
            {
                return TurnOutcome::Failed;
            }
            if calls.is_empty() {
                return TurnOutcome::Completed;
            }

            for (index, call) in calls.iter().enumerate() {
                if cancel.is_cancelled() {
                    for pending in &calls[index..] {
                        if !self.push_tool_response(ToolResponse::new(
                            &pending.call_id,
                            INTERRUPTED_TOOL,
                        )) {
                            return TurnOutcome::Failed;
                        }
                    }
                    return TurnOutcome::Interrupted;
                }
                self.emit(AgentEvent::ToolCall {
                    id: call.call_id.clone(),
                    name: call.fn_name.clone(),
                    summary: self.tools.summary(&call.fn_name, &call.fn_arguments),
                });
                let run = self.tools.start(
                    &call.fn_name,
                    call.fn_arguments.clone(),
                    Some(cancel.clone()),
                );
                let output = if run.interruptible {
                    run_interruptible(run.future, cancel).await
                } else {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => None,
                        output = run.future => Some(output),
                    }
                };
                let (content, display, is_error) = match output {
                    Some(output) => {
                        let content = if output.is_error {
                            format!("Error: {}", output.content)
                        } else {
                            output.content.clone()
                        };
                        (content, output.content, output.is_error)
                    }
                    None => (INTERRUPTED_TOOL.into(), "interrupted".into(), true),
                };
                if !self.push_tool_response(ToolResponse::new(&call.call_id, content)) {
                    return TurnOutcome::Failed;
                }
                self.emit(AgentEvent::ToolResult {
                    call_id: call.call_id.clone(),
                    output: display,
                    is_error,
                });
                if cancel.is_cancelled() {
                    for pending in &calls[index + 1..] {
                        if !self.push_tool_response(ToolResponse::new(
                            &pending.call_id,
                            INTERRUPTED_TOOL,
                        )) {
                            return TurnOutcome::Failed;
                        }
                    }
                    return TurnOutcome::Interrupted;
                }
            }
        }
        self.emit(AgentEvent::Notice(format!(
            "Stopped after {MAX_ROUNDS} model responses in one turn; send a message to continue."
        )));
        TurnOutcome::Exhausted
    }

    fn keep_partial(&mut self, text: String) -> bool {
        text.trim().is_empty() || self.append(ChatMessage::assistant(text))
    }

    fn push_tool_response(&mut self, response: ToolResponse) -> bool {
        self.append(ChatMessage::tool(MessageContent::from_tool_responses(
            vec![response],
        )))
    }
}

/// Awaits a tool call that handles interrupts itself, dropping it only if it
/// fails to return within [`INTERRUPT_GRACE`] of the interrupt.
async fn run_interruptible(
    future: futures::future::BoxFuture<'static, crate::tools::ToolOutput>,
    cancel: &CancellationToken,
) -> Option<crate::tools::ToolOutput> {
    tokio::pin!(future);
    tokio::select! {
        output = &mut future => return Some(output),
        _ = cancel.cancelled() => {}
    }
    tokio::time::timeout(INTERRUPT_GRACE, future).await.ok()
}

/// Talks to an [`Agent`] running on its own task.
enum Command {
    Submit(String, CancellationToken),
    Reset,
    Shutdown(tokio::sync::oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct AgentHandle {
    commands: mpsc::UnboundedSender<Command>,
    cancel: Arc<Mutex<CancellationToken>>,
}

impl AgentHandle {
    pub fn spawn(mut agent: Agent) -> Self {
        let (commands, mut queue) = mpsc::unbounded_channel::<Command>();
        tokio::spawn(async move {
            while let Some(command) = queue.recv().await {
                match command {
                    Command::Submit(text, cancel) => {
                        agent.submit(text, &cancel).await;
                    }
                    Command::Reset => agent.reset(),
                    Command::Shutdown(done) => {
                        let _ = done.send(());
                        break;
                    }
                }
            }
        });
        Self {
            commands,
            cancel: Arc::default(),
        }
    }

    /// Queues a user message; it runs once any current turn has finished.
    pub fn send(&self, text: String) {
        let cancel = self.cancel.lock().unwrap().clone();
        let _ = self.commands.send(Command::Submit(text, cancel));
    }

    pub fn reset(&self) {
        let _ = self.commands.send(Command::Reset);
    }

    pub async fn shutdown(&self) {
        self.interrupt();
        let (done, wait) = tokio::sync::oneshot::channel();
        if self.commands.send(Command::Shutdown(done)).is_ok() {
            let _ = wait.await;
        }
    }

    /// Stops the running turn and any queued before this call: the in-flight
    /// stream or tool call is dropped and partial output is kept, marked as
    /// interrupted. Messages sent afterwards are unaffected.
    pub fn interrupt(&self) {
        let mut cancel = self.cancel.lock().unwrap();
        cancel.cancel();
        *cancel = CancellationToken::new();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::future::BoxFuture;
    use genai::chat::{ChatRole, ContentPart};
    use serde_json::{Value, json};

    use super::*;
    use crate::provider::scripted::{ScriptedProvider, Step, text, tool_call};
    use crate::tools::{ProjectFs, Tool, ToolOutput};
    use crate::watch::Observed;

    struct Harness {
        agent: Agent,
        provider: ScriptedProvider,
        events: mpsc::UnboundedReceiver<AgentEvent>,
        _dir: tempfile::TempDir,
    }

    fn harness(responses: Vec<Vec<Step>>) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hi there\n").unwrap();
        let mut tools =
            Registry::read_only(ProjectFs::new(dir.path()).unwrap(), Observed::default());
        tools.register(Slow);
        tools.register(Watcher);
        let provider = ScriptedProvider::new(responses);
        let (tx, events) = mpsc::unbounded_channel();
        let agent = Agent::new(Arc::new(provider.clone()), tools, "system".into(), tx);
        Harness {
            agent,
            provider,
            events,
            _dir: dir,
        }
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    fn roles(agent: &Agent) -> Vec<ChatRole> {
        agent.messages().iter().map(|m| m.role.clone()).collect()
    }

    struct Slow;
    impl Tool for Slow {
        fn name(&self) -> &'static str {
            "slow"
        }
        fn description(&self) -> &'static str {
            "never finishes"
        }
        fn schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn summary(&self, _: &Value) -> String {
            String::new()
        }
        fn call(&self, _: Value) -> BoxFuture<'static, ToolOutput> {
            Box::pin(futures::future::pending())
        }
    }

    /// Waits until interrupted, then reports what it saw.
    struct Watcher;
    impl Tool for Watcher {
        fn name(&self) -> &'static str {
            "watcher"
        }
        fn description(&self) -> &'static str {
            "returns partial results on interrupt"
        }
        fn schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn summary(&self, _: &Value) -> String {
            String::new()
        }
        fn call(&self, _: Value) -> BoxFuture<'static, ToolOutput> {
            Box::pin(futures::future::pending())
        }
        fn call_interruptible(
            &self,
            _: Value,
            interrupt: CancellationToken,
        ) -> Option<BoxFuture<'static, ToolOutput>> {
            Some(Box::pin(async move {
                interrupt.cancelled().await;
                ToolOutput::ok("partial: still running")
            }))
        }
    }

    #[tokio::test]
    async fn context_retry_trims_request_without_changing_history() {
        let mut h = harness(vec![vec![text("old response")]]);
        h.agent
            .submit("old input".into(), &CancellationToken::new())
            .await;
        h.provider.push_error("maximum context length exceeded");
        h.provider.push(vec![text("new response"), Step::Usage(42)]);
        assert_eq!(
            h.agent
                .submit("latest".into(), &CancellationToken::new())
                .await,
            TurnOutcome::Completed
        );
        assert_eq!(h.agent.messages().len(), 4);
        let requests = h.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].messages.len(), 3);
        assert_eq!(requests[2].messages.len(), 1);
        assert_eq!(requests[2].system.as_deref(), Some("system"));
        let events = drain(&mut h.events);
        assert!(events.contains(&AgentEvent::Usage { prompt_tokens: 42 }));
        assert_eq!(events.iter().filter(|event| matches!(event, AgentEvent::Notice(message) if message.contains("Context shortened"))).count(), 1);
    }

    #[tokio::test]
    async fn context_errors_retry_only_once_and_never_after_partial_output() {
        let mut h = harness(vec![vec![Step::Fail("prompt is too long".into())]]);
        h.provider.push_error("context_length_exceeded");
        h.provider.push(vec![text("must not request")]);
        assert_eq!(
            h.agent
                .submit("hello".into(), &CancellationToken::new())
                .await,
            TurnOutcome::Failed
        );
        assert_eq!(h.provider.requests.lock().unwrap().len(), 2);

        let mut h = harness(vec![vec![
            text("partial"),
            Step::Fail("context_length_exceeded".into()),
        ]]);
        assert_eq!(
            h.agent
                .submit("hello".into(), &CancellationToken::new())
                .await,
            TurnOutcome::Failed
        );
        assert_eq!(h.provider.requests.lock().unwrap().len(), 1);
        assert_eq!(h.agent.messages()[1].content.first_text(), Some("partial"));
    }

    #[tokio::test]
    async fn storage_failure_prevents_requests_and_tool_execution() {
        let mut h = harness(vec![vec![text("must not request")]]);
        let data = tempfile::tempdir().unwrap();
        let (session, restored) =
            crate::session::Session::open_in(h._dir.path(), data.path()).unwrap();
        session.fail_writes();
        h.agent = h.agent.with_session(session, restored.messages);
        assert_eq!(
            h.agent
                .submit("hello".into(), &CancellationToken::new())
                .await,
            TurnOutcome::Failed
        );
        assert!(h.provider.requests.lock().unwrap().is_empty());

        struct FailAfterRequest {
            session: crate::session::Session,
            provider: ScriptedProvider,
        }
        impl Provider for FailAfterRequest {
            fn stream(
                &self,
                request: ChatRequest,
            ) -> BoxFuture<'_, Result<crate::provider::EventStream, crate::provider::ProviderError>>
            {
                self.session.fail_writes();
                self.provider.stream(request)
            }
        }
        let mut h = harness(vec![vec![tool_call("c", "list", json!({}))]]);
        let data = tempfile::tempdir().unwrap();
        let (session, restored) =
            crate::session::Session::open_in(h._dir.path(), data.path()).unwrap();
        h.agent.provider = Arc::new(FailAfterRequest {
            session: session.clone(),
            provider: h.provider.clone(),
        });
        h.agent = h.agent.with_session(session, restored.messages);
        assert_eq!(
            h.agent
                .submit("hello".into(), &CancellationToken::new())
                .await,
            TurnOutcome::Failed
        );
        assert_eq!(h.provider.requests.lock().unwrap().len(), 1);
        assert!(
            !drain(&mut h.events)
                .iter()
                .any(|event| matches!(event, AgentEvent::ToolCall { .. }))
        );
        assert_eq!(
            h.agent
                .submit("again".into(), &CancellationToken::new())
                .await,
            TurnOutcome::Failed
        );
        assert_eq!(h.provider.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn streamed_text_survives_a_hard_crash() {
        let mut h = harness(vec![vec![text("saved partial"), Step::Hang]]);
        let data = tempfile::tempdir().unwrap();
        let (session, restored) =
            crate::session::Session::open_in(h._dir.path(), data.path()).unwrap();
        h.agent = h.agent.with_session(session, restored.messages);
        let mut events = h.events;
        let task = tokio::spawn(async move {
            h.agent
                .submit("hello".into(), &CancellationToken::new())
                .await
        });
        while events.recv().await != Some(AgentEvent::AssistantDelta("saved partial".into())) {}
        task.abort();
        let _ = task.await;
        let (_, restored) = crate::session::Session::open_in(h._dir.path(), data.path()).unwrap();
        assert_eq!(restored.messages.len(), 2);
        assert_eq!(
            restored.messages[1].content.first_text(),
            Some("saved partial")
        );
    }

    #[tokio::test]
    async fn persisted_model_history_is_replayed_on_resume() {
        let mut h = harness(vec![
            vec![tool_call("c", "list", json!({}))],
            vec![text("done")],
        ]);
        let data = tempfile::tempdir().unwrap();
        let (session, restored) =
            crate::session::Session::open_in(h._dir.path(), data.path()).unwrap();
        h.agent = h.agent.with_session(session, restored.messages);
        assert_eq!(
            h.agent
                .submit("hello".into(), &CancellationToken::new())
                .await,
            TurnOutcome::Completed
        );
        let expected = serde_json::to_value(h.agent.messages()).unwrap();
        drop(h.agent);
        let (_, restored) = crate::session::Session::open_in(h._dir.path(), data.path()).unwrap();
        assert_eq!(serde_json::to_value(restored.messages).unwrap(), expected);
    }

    #[tokio::test]
    async fn completed_tool_results_survive_a_crash_during_the_next_tool() {
        let mut h = harness(vec![vec![
            tool_call("finished", "list", json!({})),
            tool_call("unfinished", "slow", json!({})),
        ]]);
        let data = tempfile::tempdir().unwrap();
        let (session, restored) =
            crate::session::Session::open_in(h._dir.path(), data.path()).unwrap();
        h.agent = h.agent.with_session(session, restored.messages);
        let mut events = h.events;
        let task = tokio::spawn(async move {
            h.agent
                .submit("hello".into(), &CancellationToken::new())
                .await
        });
        loop {
            if matches!(events.recv().await, Some(AgentEvent::ToolCall { id, .. }) if id == "unfinished")
            {
                break;
            }
        }
        task.abort();
        let _ = task.await;
        let (_, restored) = crate::session::Session::open_in(h._dir.path(), data.path()).unwrap();
        let results: Vec<_> = restored
            .messages
            .iter()
            .flat_map(|message| message.content.tool_responses())
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].call_id, "finished");
        assert!(!results[0].content.to_lowercase().contains("interrupted"));
        assert_eq!(results[1].call_id, "unfinished");
        assert!(results[1].content.to_lowercase().contains("interrupted"));
    }

    #[tokio::test]
    async fn reset_clears_messages_and_shutdown_waits_for_the_turn() {
        let h = harness(vec![vec![text("old")], vec![text("new")]]);
        let provider = h.provider.clone();
        let mut events = h.events;
        let handle = AgentHandle::spawn(h.agent);
        handle.send("first".into());
        while events.recv().await != Some(AgentEvent::TurnFinished(TurnOutcome::Completed)) {}
        handle.reset();
        handle.send("second".into());
        while events.recv().await != Some(AgentEvent::TurnFinished(TurnOutcome::Completed)) {}
        handle.shutdown().await;
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].messages.len(), 1);
        assert_eq!(requests[1].messages[0].content.first_text(), Some("second"));
    }

    #[tokio::test]
    async fn interruptible_tool_reports_partial_result() {
        let mut h = harness(vec![vec![
            tool_call("a", "watcher", json!({})),
            tool_call("b", "list", json!({})),
        ]]);
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });
        let outcome = h.agent.submit("go".into(), &cancel).await;
        assert_eq!(outcome, TurnOutcome::Interrupted);
        let events = drain(&mut h.events);
        assert!(events.contains(&AgentEvent::ToolResult {
            call_id: "a".into(),
            output: "partial: still running".into(),
            is_error: false,
        }));
        let responses: Vec<_> = h.agent.messages()[2..]
            .iter()
            .flat_map(|m| m.content.tool_responses())
            .collect();
        assert_eq!(responses[0].content, "partial: still running");
        assert_eq!(responses[1].call_id, "b");
        assert_eq!(responses[1].content, INTERRUPTED_TOOL);
    }

    #[tokio::test]
    async fn text_turn_streams_and_completes() {
        let mut h = harness(vec![vec![text("Hel"), text("lo")]]);
        let outcome = h.agent.submit("hi".into(), &CancellationToken::new()).await;
        assert_eq!(outcome, TurnOutcome::Completed);
        assert_eq!(
            drain(&mut h.events),
            [
                AgentEvent::TurnStarted,
                AgentEvent::AssistantDelta("Hel".into()),
                AgentEvent::AssistantDelta("lo".into()),
                AgentEvent::AssistantDone { interrupted: false },
                AgentEvent::TurnFinished(TurnOutcome::Completed),
            ]
        );
        assert_eq!(roles(&h.agent), [ChatRole::User, ChatRole::Assistant]);
        let request = &h.provider.requests.lock().unwrap()[0];
        assert_eq!(request.system.as_deref(), Some("system"));
        let tools: Vec<_> = request
            .tools
            .iter()
            .flatten()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(
            tools,
            ["read_file", "list", "glob", "grep", "slow", "watcher"]
        );
    }

    #[tokio::test]
    async fn tool_round_trip_feeds_results_back() {
        let mut h = harness(vec![
            vec![
                text("Let me look."),
                tool_call("c1", "read_file", json!({"path": "hello.txt"})),
                tool_call("c2", "nope", json!({})),
            ],
            vec![text("It says hi.")],
        ]);
        let outcome = h
            .agent
            .submit("read it".into(), &CancellationToken::new())
            .await;
        assert_eq!(outcome, TurnOutcome::Completed);

        let events = drain(&mut h.events);
        assert!(events.contains(&AgentEvent::ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            summary: "hello.txt".into(),
        }));
        assert!(events.contains(&AgentEvent::ToolResult {
            call_id: "c1".into(),
            output: "1\thi there".into(),
            is_error: false,
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolResult { call_id, is_error: true, .. } if call_id == "c2"
        )));
        assert_eq!(
            events.last(),
            Some(&AgentEvent::TurnFinished(TurnOutcome::Completed))
        );

        assert_eq!(
            roles(&h.agent),
            [
                ChatRole::User,
                ChatRole::Assistant,
                ChatRole::Tool,
                ChatRole::Tool,
                ChatRole::Assistant
            ]
        );
        let requests = h.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let responses: Vec<_> = requests[1].messages[2..]
            .iter()
            .flat_map(|m| m.content.tool_responses())
            .collect();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].call_id, "c1");
        assert_eq!(responses[0].content, "1\thi there");
        assert!(responses[1].content.starts_with("Error: unknown tool"));
    }

    #[tokio::test]
    async fn interrupt_mid_stream_keeps_partial_text() {
        let mut h = harness(vec![vec![text("Partial"), Step::Hang]]);
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });
        let outcome = h.agent.submit("go".into(), &cancel).await;
        assert_eq!(outcome, TurnOutcome::Interrupted);
        assert_eq!(
            drain(&mut h.events),
            [
                AgentEvent::TurnStarted,
                AgentEvent::AssistantDelta("Partial".into()),
                AgentEvent::AssistantDone { interrupted: true },
                AgentEvent::TurnFinished(TurnOutcome::Interrupted),
            ]
        );
        assert_eq!(roles(&h.agent), [ChatRole::User, ChatRole::Assistant]);
        assert_eq!(h.agent.messages()[1].content.first_text(), Some("Partial"));
    }

    #[tokio::test]
    async fn interrupt_during_tool_answers_every_call() {
        let mut h = harness(vec![vec![
            tool_call("a", "slow", json!({})),
            tool_call("b", "list", json!({})),
        ]]);
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });
        let outcome = h.agent.submit("go".into(), &cancel).await;
        assert_eq!(outcome, TurnOutcome::Interrupted);
        assert_eq!(
            roles(&h.agent),
            [
                ChatRole::User,
                ChatRole::Assistant,
                ChatRole::Tool,
                ChatRole::Tool
            ]
        );
        let responses: Vec<_> = h.agent.messages()[2..]
            .iter()
            .flat_map(|m| m.content.tool_responses())
            .collect();
        let ids: Vec<_> = responses.iter().map(|r| r.call_id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert!(responses.iter().all(|r| r.content == INTERRUPTED_TOOL));

        // The conversation stays usable afterwards.
        h.provider.push(vec![text("ok")]);
        let outcome = h
            .agent
            .submit("again".into(), &CancellationToken::new())
            .await;
        assert_eq!(outcome, TurnOutcome::Completed);
    }

    #[tokio::test]
    async fn already_cancelled_turn_records_message_without_a_request() {
        let mut h = harness(vec![]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = h.agent.submit("hi".into(), &cancel).await;
        assert_eq!(outcome, TurnOutcome::Interrupted);
        assert!(h.provider.requests.lock().unwrap().is_empty());
        assert_eq!(roles(&h.agent), [ChatRole::User]);
    }

    #[tokio::test]
    async fn provider_errors_end_the_turn_readably() {
        let mut h = harness(vec![]);
        h.provider.push_error("HTTP 401: bad key");
        let outcome = h.agent.submit("hi".into(), &CancellationToken::new()).await;
        assert_eq!(outcome, TurnOutcome::Failed);
        assert_eq!(
            drain(&mut h.events),
            [
                AgentEvent::TurnStarted,
                AgentEvent::Error("HTTP 401: bad key".into()),
                AgentEvent::TurnFinished(TurnOutcome::Failed),
            ]
        );

        h.provider
            .push(vec![text("so far"), Step::Fail("connection reset".into())]);
        let outcome = h
            .agent
            .submit("retry".into(), &CancellationToken::new())
            .await;
        assert_eq!(outcome, TurnOutcome::Failed);
        let events = drain(&mut h.events);
        assert!(events.contains(&AgentEvent::Error("connection reset".into())));
        assert_eq!(
            roles(&h.agent),
            [ChatRole::User, ChatRole::User, ChatRole::Assistant]
        );
    }

    #[tokio::test]
    async fn round_limit_stops_runaway_tool_loops() {
        let responses = (0..MAX_ROUNDS)
            .map(|i| vec![tool_call(&format!("c{i}"), "list", json!({}))])
            .collect();
        let mut h = harness(responses);
        let outcome = h
            .agent
            .submit("loop".into(), &CancellationToken::new())
            .await;
        assert_eq!(outcome, TurnOutcome::Exhausted);
        assert_eq!(h.provider.requests.lock().unwrap().len(), MAX_ROUNDS);
        let events = drain(&mut h.events);
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Notice(_))));
    }

    #[tokio::test]
    async fn handle_interrupts_running_and_queued_turns_only() {
        let provider = ScriptedProvider::new([vec![text("a"), Step::Hang], vec![text("b")]]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let agent = Agent::new(
            Arc::new(provider.clone()),
            Registry::default(),
            String::new(),
            tx,
        );
        let handle = AgentHandle::spawn(agent);
        handle.send("first".into());
        handle.send("queued".into());
        while rx.recv().await != Some(AgentEvent::AssistantDelta("a".into())) {}
        handle.interrupt();
        handle.send("after".into());

        let mut finished = Vec::new();
        while finished.len() < 3 {
            if let Some(AgentEvent::TurnFinished(outcome)) = rx.recv().await {
                finished.push(outcome);
            }
        }
        assert_eq!(
            finished,
            [
                TurnOutcome::Interrupted,
                TurnOutcome::Interrupted,
                TurnOutcome::Completed
            ]
        );
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let last = requests[1].messages.last().unwrap();
        assert!(matches!(
            last.content.parts().first(),
            Some(ContentPart::Text(t)) if t == "after"
        ));
    }
}
