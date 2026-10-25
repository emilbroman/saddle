//! Headless end-to-end runs of the event loop against ratatui's test backend.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures::channel::mpsc;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use std::sync::Arc;

use saddle::agent::{Agent, AgentHandle};
use saddle::app::{self, Activity, App, Status};
use saddle::config::{Config, Credentials};
use saddle::events::{self, AgentEvent, EventSenders};
use saddle::provider::scripted::{ScriptedProvider, Step, text, tool_call};
use saddle::tools::{ProjectFs, Registry};
use saddle::transcript::{Entry, NoticeLevel, StreamState};
use saddle::watch::{Observed, WatchConfig, Watcher};

fn app(senders: EventSenders) -> App {
    let path = PathBuf::from("/cfg");
    let config = Config::parse(
        "model = \"test-model\"\napi = \"messages\"\nbase_url = \"https://llm.test\"\n",
        &path,
    )
    .unwrap();
    let credentials = Credentials::parse("[inference]\napi_key = \"k\"\n", &path).unwrap();
    App::new(
        config,
        credentials,
        PathBuf::from("/src/project"),
        Status {
            project: "~/src/project".into(),
            model: "test-model".into(),
            api: "messages".into(),
            sandbox: "not available".into(),
            web_search: None,
        },
        senders,
    )
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> io::Result<Event> {
    Ok(Event::Key(KeyEvent::new(code, modifiers)))
}

fn typed(text: &str) -> Vec<io::Result<Event>> {
    text.chars()
        .map(|c| key(KeyCode::Char(c), KeyModifiers::NONE))
        .collect()
}

fn screen(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let width = usize::from(buffer.area.width);
    buffer
        .content
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

#[tokio::test]
async fn conversation_renders_through_the_event_loop() {
    let (senders, receivers) = events::channels();
    let agent = senders.agent.clone();
    let mut app = app(senders);
    let (input, terminal_events) = mpsc::unbounded();

    let handle = tokio::spawn(async move {
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        app::run(&mut terminal, &mut app, terminal_events, receivers)
            .await
            .unwrap();
        (terminal, app)
    });

    for event in typed("first line") {
        input.unbounded_send(event).unwrap();
    }
    input
        .unbounded_send(key(KeyCode::Enter, KeyModifiers::SHIFT))
        .unwrap();
    for event in typed("second") {
        input.unbounded_send(event).unwrap();
    }
    input
        .unbounded_send(Ok(Event::Paste(" pasted\r\nmore".into())))
        .unwrap();
    input
        .unbounded_send(key(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    settle().await;

    agent
        .send(AgentEvent::AssistantDelta("Streaming ".into()))
        .unwrap();
    agent
        .send(AgentEvent::AssistantDelta("reply".into()))
        .unwrap();
    agent
        .send(AgentEvent::AssistantDone { interrupted: false })
        .unwrap();
    settle().await;

    input.unbounded_send(Ok(Event::Resize(60, 15))).unwrap();
    input
        .unbounded_send(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .unwrap();

    let (terminal, app) = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("event loop did not quit")
        .unwrap();

    let entries: Vec<_> = app.transcript.entries().cloned().collect();
    assert_eq!(
        entries[0],
        Entry::User {
            text: "first line\nsecond pasted\nmore".into()
        }
    );
    assert_eq!(
        entries.last().unwrap(),
        &Entry::Assistant {
            text: "Streaming reply".into(),
            state: StreamState::Complete
        }
    );
    assert!(app.input.is_empty());

    let screen = screen(&terminal);
    assert!(screen.contains("› first line"), "{screen}");
    assert!(screen.contains("  second pasted"), "{screen}");
    assert!(screen.contains("● Streaming reply"), "{screen}");
    assert!(screen.contains("~/src/project"), "{screen}");
    assert!(screen.contains("test-model (messages)"), "{screen}");
    assert!(screen.contains("sandbox: not available"), "{screen}");
}

#[tokio::test]
async fn loop_ends_when_terminal_input_ends() {
    let (senders, receivers) = events::channels();
    let mut app = app(senders);
    let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
    let terminal_events = futures::stream::iter(typed("hi"));
    app::run(&mut terminal, &mut app, terminal_events, receivers)
        .await
        .unwrap();
    assert_eq!(app.input.lines(), ["hi"]);
    assert!(app.transcript.is_empty());
}

#[tokio::test]
async fn page_up_scrolls_away_from_the_bottom() {
    let (senders, receivers) = events::channels();
    let agent = senders.agent.clone();
    let mut app = app(senders);
    for i in 0..50 {
        agent.send(AgentEvent::Error(format!("line {i}"))).unwrap();
    }
    let (input, terminal_events) = mpsc::unbounded();
    let handle = tokio::spawn(async move {
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        app::run(&mut terminal, &mut app, terminal_events, receivers)
            .await
            .unwrap();
        (terminal, app)
    });
    settle().await;
    input
        .unbounded_send(key(KeyCode::PageUp, KeyModifiers::NONE))
        .unwrap();
    settle().await;
    input
        .unbounded_send(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .unwrap();
    let (terminal, app) = handle.await.unwrap();
    assert!(!app.scroll.is_following());
    let screen = screen(&terminal);
    assert!(screen.contains("[scrolled]"), "{screen}");
    assert!(!screen.contains("line 49"), "{screen}");
}

#[tokio::test]
async fn agent_turn_runs_tools_and_can_be_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "hi there\n").unwrap();
    let provider = ScriptedProvider::new([
        vec![
            text("Reading."),
            tool_call("c1", "read_file", serde_json::json!({"path": "hello.txt"})),
        ],
        vec![text("It says hi.")],
        vec![text("A long answer"), Step::Hang],
    ]);

    let (senders, receivers) = events::channels();
    let agent = Agent::new(
        Arc::new(provider.clone()),
        Registry::read_only(ProjectFs::new(dir.path()).unwrap(), Observed::default()),
        "system".into(),
        senders.agent.clone(),
    );
    let mut app = app(senders);
    app.attach_agent(AgentHandle::spawn(agent));

    let (input, terminal_events) = mpsc::unbounded();
    let handle = tokio::spawn(async move {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        app::run(&mut terminal, &mut app, terminal_events, receivers)
            .await
            .unwrap();
        (terminal, app)
    });
    let send = |events: Vec<io::Result<Event>>| {
        for event in events {
            input.unbounded_send(event).unwrap();
        }
    };

    send(typed("what is in hello.txt"));
    send(vec![key(KeyCode::Enter, KeyModifiers::NONE)]);
    settle().await;
    send(typed("next"));
    send(vec![key(KeyCode::Enter, KeyModifiers::NONE)]);
    settle().await;
    // Busy: Enter keeps the draft instead of sending it.
    send(typed("draft"));
    send(vec![key(KeyCode::Enter, KeyModifiers::NONE)]);
    settle().await;
    send(vec![key(KeyCode::Esc, KeyModifiers::NONE)]);
    settle().await;
    send(vec![key(KeyCode::Char('c'), KeyModifiers::CONTROL)]);

    let (terminal, app) = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("event loop did not quit")
        .unwrap();

    let entries: Vec<_> = app.transcript.entries().cloned().collect();
    assert_eq!(
        entries,
        [
            Entry::User {
                text: "what is in hello.txt".into()
            },
            Entry::Assistant {
                text: "Reading.".into(),
                state: StreamState::Complete
            },
            Entry::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                summary: "hello.txt".into()
            },
            Entry::ToolResult {
                call_id: "c1".into(),
                output: "1\thi there".into(),
                is_error: false
            },
            Entry::Assistant {
                text: "It says hi.".into(),
                state: StreamState::Complete
            },
            Entry::User {
                text: "next".into()
            },
            Entry::Assistant {
                text: "A long answer".into(),
                state: StreamState::Interrupted
            },
            Entry::Notice {
                text: "Interrupted.".into(),
                level: NoticeLevel::Info
            },
        ]
    );
    assert_eq!(app.activity, Activity::Idle);
    assert_eq!(app.input.lines(), ["draft"]);
    assert_eq!(provider.requests.lock().unwrap().len(), 3);

    let screen = screen(&terminal);
    assert!(screen.contains("⚙ read_file hello.txt"), "{screen}");
    assert!(screen.contains("↳ 1    hi there"), "{screen}");
    assert!(screen.contains("[interrupted]"), "{screen}");
}

#[tokio::test]
async fn saved_changes_interrupt_the_turn_and_start_a_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(root.join("hello.txt"), "hi there\n").unwrap();
    let provider = ScriptedProvider::new([
        vec![tool_call(
            "c1",
            "read_file",
            serde_json::json!({"path": "hello.txt"}),
        )],
        vec![text("Halfway through"), Step::Hang],
        vec![text("You changed the greeting.")],
    ]);

    let (senders, receivers) = events::channels();
    let observed = Observed::default();
    let _watcher = Watcher::spawn(
        root.clone(),
        WatchConfig {
            quiet: Duration::from_millis(150),
            max_window: Duration::from_secs(5),
            excluded: vec![],
        },
        observed.clone(),
        senders.fs.clone(),
    );
    let agent = Agent::new(
        Arc::new(provider.clone()),
        Registry::read_only(ProjectFs::new(&root).unwrap(), observed),
        "system".into(),
        senders.agent.clone(),
    );
    let mut app = app(senders);
    app.attach_agent(AgentHandle::spawn(agent));

    let (input, terminal_events) = mpsc::unbounded();
    let handle = tokio::spawn(async move {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        app::run(&mut terminal, &mut app, terminal_events, receivers)
            .await
            .unwrap();
        (terminal, app)
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    for event in typed("explain hello.txt") {
        input.unbounded_send(event).unwrap();
    }
    input
        .unbounded_send(key(KeyCode::Enter, KeyModifiers::NONE))
        .unwrap();
    // The turn is now hanging mid-stream.
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(root.join("hello.txt"), "bye there\n").unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    input
        .unbounded_send(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .unwrap();

    let (terminal, app) = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("event loop did not quit")
        .unwrap();

    let entries: Vec<_> = app.transcript.entries().cloned().collect();
    assert_eq!(
        entries[3..],
        [
            Entry::Assistant {
                text: "Halfway through".into(),
                state: StreamState::Interrupted
            },
            Entry::Notice {
                text: "Interrupted: files changed.".into(),
                level: NoticeLevel::Info
            },
            Entry::FilesChanged {
                headline: "1 file changed".into(),
                paths: vec!["hello.txt".into()],
                details: vec!["modified hello.txt (+1 -1)".into()],
            },
            Entry::Assistant {
                text: "You changed the greeting.".into(),
                state: StreamState::Complete
            },
        ]
    );
    assert_eq!(app.activity, Activity::Idle);

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let event = requests[2]
        .messages
        .last()
        .unwrap()
        .content
        .first_text()
        .unwrap();
    assert!(event.starts_with("<file-changes>"), "{event}");
    assert!(
        event.contains(
            "modified hello.txt (+1 -1)\n```diff\n@@ -1 +1 @@\n-hi there\n+bye there\n```"
        ),
        "{event}"
    );

    let screen = screen(&terminal);
    assert!(screen.contains("✎ 1 file changed: hello.txt"), "{screen}");
}

#[tokio::test]
async fn lsp_status_reaches_the_status_line() {
    use saddle::events::LspEvent;
    use saddle::lsp::{ServerState, ServerStatus};

    let (senders, receivers) = events::channels();
    let lsp = senders.lsp.clone();
    let mut app = app(senders);
    let (input, terminal_events) = mpsc::unbounded();
    let handle = tokio::spawn(async move {
        let mut terminal = Terminal::new(TestBackend::new(120, 10)).unwrap();
        app::run(&mut terminal, &mut app, terminal_events, receivers)
            .await
            .unwrap();
        (terminal, app)
    });

    let status = |state| {
        LspEvent::Status(vec![ServerStatus {
            language: "rust".into(),
            root: "/src/project".into(),
            state,
        }])
    };
    lsp.send(status(ServerState::Busy("Indexing".into())))
        .unwrap();
    lsp.send(status(ServerState::Restarting {
        attempt: 1,
        reason: "exited with exit status: 101".into(),
    }))
    .unwrap();
    lsp.send(status(ServerState::Ready)).unwrap();
    settle().await;
    input
        .unbounded_send(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .unwrap();

    let (terminal, app) = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("event loop did not quit")
        .unwrap();
    let screen = screen(&terminal);
    assert!(screen.contains("lsp: rust ✓"), "{screen}");
    let notices: Vec<_> = app
        .transcript
        .entries()
        .filter(|e| {
            matches!(
                e,
                Entry::Notice {
                    level: NoticeLevel::Error,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
}

#[tokio::test]
async fn diagnostic_wait_interrupts_now_and_delivers_only_the_combined_message() {
    use saddle::events::{FsEvent, TurnOutcome};
    use saddle::watch::{ChangeKind, ChangeSet, Detail, FileChange};

    let provider = ScriptedProvider::new([
        vec![text("In flight"), Step::Hang],
        vec![text("That is a type error.")],
    ]);
    let (senders, mut receivers) = events::channels();
    let mut app = app(senders.clone());
    app.attach_agent(AgentHandle::spawn(Agent::new(
        Arc::new(provider.clone()),
        Registry::default(),
        "system".into(),
        senders.agent,
    )));
    app.handle_terminal_event(Event::Paste("explain this".into()));
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), receivers.agent.recv())
            .await
            .unwrap()
            .unwrap();
        let streaming = matches!(event, AgentEvent::AssistantDelta(_));
        app.handle_agent_event(event);
        if streaming {
            break;
        }
    }
    app.handle_fs_event(FsEvent::Settling);
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), receivers.agent.recv())
            .await
            .unwrap()
            .unwrap();
        let finished = event == AgentEvent::TurnFinished(TurnOutcome::Interrupted);
        app.handle_agent_event(event);
        if finished {
            break;
        }
    }
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    app.handle_terminal_event(Event::Paste("keep my draft".into()));
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    assert_eq!(app.input.lines(), ["keep my draft"]);
    let diagnostics =
        "<diagnostic-changes>\nnew: a.rs:1:1 Error: mismatched types\n</diagnostic-changes>";
    app.handle_fs_event(FsEvent::Changed(Arc::new(ChangeSet {
        files: vec![FileChange {
            path: "/src/project/a.rs".into(),
            display: "a.rs".into(),
            kind: ChangeKind::Modified,
            detail: Detail::Unseen { size: 1 },
        }],
        diagnostics: Some(diagnostics.into()),
    })));
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), receivers.agent.recv())
            .await
            .unwrap()
            .unwrap();
        let finished = matches!(event, AgentEvent::TurnFinished(_));
        app.handle_agent_event(event);
        if finished {
            break;
        }
    }
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let event = requests[1]
        .messages
        .last()
        .unwrap()
        .content
        .first_text()
        .unwrap();
    assert!(
        event.contains("<file-changes>") && event.contains(diagnostics),
        "{event}"
    );
    assert!(app.transcript.entries().any(|entry| matches!(entry, Entry::FilesChanged { details, .. } if details.iter().any(|s| s == diagnostics))));
}
