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

const INTERRUPTED_TOOL: &str = "Interrupted: the user stopped this turn before the tool finished.";

pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: Registry,
    system: String,
    messages: Vec<ChatMessage>,
    events: mpsc::UnboundedSender<AgentEvent>,
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
        }
    }

    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    fn emit(&self, event: AgentEvent) {
        // The UI only goes away on shutdown, when nobody cares any more.
        let _ = self.events.send(event);
    }

    /// Adds a user message and runs a turn on it.
    pub async fn submit(&mut self, text: String, cancel: &CancellationToken) -> TurnOutcome {
        self.messages.push(ChatMessage::user(text));
        self.emit(AgentEvent::TurnStarted);
        let outcome = self.run_turn(cancel).await;
        self.emit(AgentEvent::TurnFinished(outcome));
        outcome
    }

    async fn run_turn(&mut self, cancel: &CancellationToken) -> TurnOutcome {
        for _ in 0..MAX_ROUNDS {
            if cancel.is_cancelled() {
                return TurnOutcome::Interrupted;
            }
            let request = ChatRequest::new(self.messages.clone())
                .with_system(self.system.clone())
                .with_tools(self.tools.definitions());

            let stream = tokio::select! {
                biased;
                _ = cancel.cancelled() => return TurnOutcome::Interrupted,
                stream = self.provider.stream(request) => stream,
            };
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(e) => {
                    self.emit(AgentEvent::Error(e.0));
                    return TurnOutcome::Failed;
                }
            };

            let mut text = String::new();
            let mut thinking = false;
            let completion = loop {
                let event = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        self.keep_partial(text);
                        self.emit(AgentEvent::AssistantDone { interrupted: true });
                        return TurnOutcome::Interrupted;
                    }
                    event = stream.next() => event,
                };
                match event {
                    Some(Ok(StreamEvent::Text(delta))) => {
                        text.push_str(&delta);
                        self.emit(AgentEvent::AssistantDelta(delta));
                    }
                    Some(Ok(StreamEvent::Reasoning(_))) => {
                        if !thinking {
                            thinking = true;
                            self.emit(AgentEvent::Thinking);
                        }
                    }
                    Some(Ok(StreamEvent::End(completion))) => break completion,
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
            };
            self.emit(AgentEvent::AssistantDone { interrupted: false });
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
            if !completion.content.is_empty() {
                self.messages
                    .push(ChatMessage::assistant(completion.content));
            }
            if calls.is_empty() {
                return TurnOutcome::Completed;
            }

            let mut responses = Vec::with_capacity(calls.len());
            for call in &calls {
                self.emit(AgentEvent::ToolCall {
                    id: call.call_id.clone(),
                    name: call.fn_name.clone(),
                    summary: self.tools.summary(&call.fn_name, &call.fn_arguments),
                });
                let run = self.tools.call(&call.fn_name, call.fn_arguments.clone());
                let output = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => None,
                    output = run => Some(output),
                };
                let Some(output) = output else {
                    // Every call needs a response or the next request is rejected.
                    for pending in &calls[responses.len()..] {
                        responses.push(ToolResponse::new(&pending.call_id, INTERRUPTED_TOOL));
                    }
                    self.emit(AgentEvent::ToolResult {
                        call_id: call.call_id.clone(),
                        output: "interrupted".into(),
                        is_error: true,
                    });
                    self.push_tool_responses(responses);
                    return TurnOutcome::Interrupted;
                };
                self.emit(AgentEvent::ToolResult {
                    call_id: call.call_id.clone(),
                    output: output.content.clone(),
                    is_error: output.is_error,
                });
                let content = if output.is_error {
                    format!("Error: {}", output.content)
                } else {
                    output.content
                };
                responses.push(ToolResponse::new(&call.call_id, content));
            }
            self.push_tool_responses(responses);
        }

        self.emit(AgentEvent::Notice(format!(
            "Stopped after {MAX_ROUNDS} model responses in one turn; send a message to continue."
        )));
        TurnOutcome::Exhausted
    }

    /// Keeps streamed text from a response that did not finish, so the model
    /// knows what it already said.
    fn keep_partial(&mut self, text: String) {
        if !text.trim().is_empty() {
            self.messages.push(ChatMessage::assistant(text));
        }
    }

    fn push_tool_responses(&mut self, responses: Vec<ToolResponse>) {
        self.messages
            .push(ChatMessage::tool(MessageContent::from_tool_responses(
                responses,
            )));
    }
}

/// Talks to an [`Agent`] running on its own task.
#[derive(Clone)]
pub struct AgentHandle {
    commands: mpsc::UnboundedSender<(String, CancellationToken)>,
    cancel: Arc<Mutex<CancellationToken>>,
}

impl AgentHandle {
    pub fn spawn(mut agent: Agent) -> Self {
        let (commands, mut queue) = mpsc::unbounded_channel::<(String, CancellationToken)>();
        tokio::spawn(async move {
            while let Some((text, cancel)) = queue.recv().await {
                agent.submit(text, &cancel).await;
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
        let _ = self.commands.send((text, cancel));
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

    struct Harness {
        agent: Agent,
        provider: ScriptedProvider,
        events: mpsc::UnboundedReceiver<AgentEvent>,
        _dir: tempfile::TempDir,
    }

    fn harness(responses: Vec<Vec<Step>>) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hi there\n").unwrap();
        let mut tools = Registry::read_only(ProjectFs::new(dir.path()).unwrap());
        tools.register(Slow);
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
        assert_eq!(tools, ["read_file", "list", "glob", "grep", "slow"]);
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
                ChatRole::Assistant
            ]
        );
        let requests = h.provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let responses = requests[1].messages[2].content.tool_responses();
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
            [ChatRole::User, ChatRole::Assistant, ChatRole::Tool]
        );
        let responses = h.agent.messages()[2].content.tool_responses();
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
