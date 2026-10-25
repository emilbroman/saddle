//! Persistence across separate application lifetimes, without an inference service.
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use saddle::agent::{Agent, AgentHandle};
use saddle::app::{App, Status};
use saddle::config::{Config, Credentials};
use saddle::events::{self, EventReceivers};
use saddle::provider::scripted::{ScriptedProvider, text, tool_call};
use saddle::session::{LogEvent, Session};
use saddle::tools::{ProjectFs, Registry};
use saddle::transcript::{Entry, Transcript};
use saddle::watch::{Observed, filter::Filter};
use serde_json::json;

fn application(root: &Path, data: &Path, provider: ScriptedProvider) -> (App, EventReceivers) {
    let (session, restored) = Session::open_in(root, data).unwrap();
    let observed = Observed::default();
    observed.restore(restored.state.observed.clone());
    let changes = if restored.existing {
        observed.resume(root, vec![])
    } else {
        observed.seed(&Filter::new(root, vec![]));
        vec![]
    };
    let saved = session.clone();
    observed.set_persistence(Arc::new(move |snapshot| {
        saved.save_snapshots(snapshot).unwrap()
    }));
    session.save_snapshots(observed.snapshot()).unwrap();
    let (senders, receivers) = events::channels();
    let config = Config::parse(
        "model='test'\napi='messages'\nbase_url='http://localhost'",
        root,
    )
    .unwrap();
    let credentials = Credentials::parse("[inference]\napi_key='test'", root).unwrap();
    let agent = Agent::new(
        Arc::new(provider),
        Registry::read_only(ProjectFs::new(root).unwrap(), observed.clone()),
        "system".into(),
        senders.agent.clone(),
    )
    .with_session(session.clone(), restored.messages.clone());
    let jobs = restored.pending_jobs.clone();
    let mut app = App::new(
        config,
        credentials,
        root.to_owned(),
        Status {
            project: root.display().to_string(),
            model: "test".into(),
            api: "messages".into(),
            sandbox: "test".into(),
            web_search: None,
        },
        senders,
    );
    app.attach_session(session, restored, observed, vec![]);
    app.attach_agent(AgentHandle::spawn(agent));
    app.resume(
        saddle::watch::ChangeSet {
            files: changes,
            diagnostics: None,
        },
        jobs,
    );
    (app, receivers)
}

fn submit(app: &mut App, text: &str) {
    app.handle_terminal_event(Event::Paste(text.into()));
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
}

async fn finish(app: &mut App, events: &mut EventReceivers) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while app.is_busy() {
            app.handle_agent_event(events.agent.recv().await.unwrap());
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn reopen_restores_tools_delivers_offline_diff_and_then_is_silent() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let data = tempfile::tempdir().unwrap();
    std::fs::write(root.join("hello.txt"), "before\n").unwrap();
    let provider = ScriptedProvider::new([
        vec![
            text("Reading."),
            tool_call("r1", "read_file", json!({"path":"hello.txt"})),
        ],
        vec![text("It says before.")],
    ]);
    let (mut app, mut events) = application(&root, data.path(), provider.clone());
    assert!(!app.is_busy());
    submit(&mut app, "Read hello.txt");
    finish(&mut app, &mut events).await;
    let old_entries: Vec<_> = app.transcript.entries().cloned().collect();
    app.agent.as_ref().unwrap().shutdown().await;
    drop(app);
    std::fs::write(root.join("hello.txt"), "after\n").unwrap();

    let provider = ScriptedProvider::new([vec![text("I see your edit.")]]);
    let (mut app, mut events) = application(&root, data.path(), provider.clone());
    assert!(app.is_busy());
    assert_eq!(
        app.transcript
            .entries()
            .take(old_entries.len())
            .cloned()
            .collect::<Vec<_>>(),
        old_entries
    );
    finish(&mut app, &mut events).await;
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert!(
            request
                .messages
                .iter()
                .any(|m| !m.content.tool_responses().is_empty())
        );
        let change = request
            .messages
            .last()
            .unwrap()
            .content
            .first_text()
            .unwrap();
        assert!(change.contains("-before"), "{change}");
        assert!(change.contains("+after"), "{change}");
    }
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|f| saddle::ui::render(f, &mut app)).unwrap();
    assert!(
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|c| c.symbol() == "↳")
    );
    app.agent.as_ref().unwrap().shutdown().await;
    drop(app);

    let provider = ScriptedProvider::default();
    let (mut app, _) = application(&root, data.path(), provider.clone());
    assert!(!app.is_busy());
    assert!(provider.requests.lock().unwrap().is_empty());
    assert!(
        app.transcript
            .entries()
            .any(|e| matches!(e, Entry::Assistant{text, ..} if text == "I see your edit."))
    );
    submit(&mut app, "/new");
    assert!(app.transcript.is_empty());
    app.agent.as_ref().unwrap().shutdown().await;
    drop(app);
    let (session, restored) = Session::open_in(&root, data.path()).unwrap();
    assert!(restored.messages.is_empty());
    assert!(restored.ui.is_empty());
    assert!(
        std::fs::read_dir(session.dir().join("archives"))
            .unwrap()
            .next()
            .is_some()
    );
}

#[tokio::test]
async fn lost_jobs_and_changes_produce_one_resume_request() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let data = tempfile::tempdir().unwrap();
    let (session, _) = Session::open_in(&root, data.path()).unwrap();
    session
        .append(LogEvent::JobStarted {
            id: 7,
            command: "cargo test".into(),
        })
        .unwrap();
    drop(session);
    std::fs::write(root.join("new.txt"), "new").unwrap();
    let provider = ScriptedProvider::new([vec![text("Understood.")]]);
    let (mut app, mut events) = application(&root, data.path(), provider.clone());
    finish(&mut app, &mut events).await;
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let input = requests[0]
            .messages
            .last()
            .unwrap()
            .content
            .first_text()
            .unwrap();
        assert!(input.contains("new.txt") && input.contains("job 7") && input.contains("lost"));
    }
    app.agent.as_ref().unwrap().shutdown().await;
    drop(app);
    let (_, restored) = Session::open_in(&root, data.path()).unwrap();
    assert_eq!(restored.pending_jobs, BTreeMap::new());
    assert_eq!(restored.last_job_id, 7);
}

#[test]
fn transcript_delta_replay_is_exact_and_collapsed() {
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let (session, _) = Session::open_in(root.path(), data.path()).unwrap();
    let mut transcript = Transcript::default();
    transcript.attach_session(session.clone());
    transcript.push(Entry::User {
        text: "hello".into(),
    });
    transcript.append_assistant("one");
    transcript.append_assistant(" two");
    transcript.finish_assistant(saddle::transcript::StreamState::Complete);
    let expected: Vec<_> = transcript.entries().cloned().collect();
    drop(transcript);
    drop(session);
    let (_, restored) = Session::open_in(root.path(), data.path()).unwrap();
    let transcript = Transcript::restore(restored.ui);
    assert_eq!(transcript.entries().cloned().collect::<Vec<_>>(), expected);
}

#[tokio::test]
async fn resume_diagnostics_and_lost_jobs_share_one_request() {
    use saddle::watch::{ChangeKind, ChangeSet, Detail, FileChange};
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::new([vec![text("Noted.")]]);
    let (mut app, mut events) = application(root.path(), data.path(), provider.clone());
    app.resume(
        ChangeSet {
            files: vec![FileChange {
                path: root.path().join("new.rs"),
                display: "new.rs".into(),
                kind: ChangeKind::Created,
                detail: Detail::Unseen { size: 20 },
            }],
            diagnostics: Some("<diagnostic-changes>new: type error</diagnostic-changes>".into()),
        },
        BTreeMap::from([(4, "cargo test".into())]),
    );
    finish(&mut app, &mut events).await;
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let input = requests[0]
            .messages
            .last()
            .unwrap()
            .content
            .first_text()
            .unwrap();
        assert!(
            input.contains("new.rs") && input.contains("type error") && input.contains("job 4")
        );
    }
    assert!(app.transcript.entries().any(|e| matches!(e, Entry::FilesChanged {details, ..} if details.iter().any(|s| s.contains("type error")))));
    app.agent.as_ref().unwrap().shutdown().await;
}
