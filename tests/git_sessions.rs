use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use saddle::agent::{Agent, AgentHandle};
use saddle::app::{App, Status};
use saddle::config::{Config, Credentials};
use saddle::events::{self, EventReceivers};
use saddle::git::Tracker;
use saddle::provider::scripted::{ScriptedProvider, text};
use saddle::session::Session;
use saddle::tools::{ProjectFs, Registry};
use saddle::transcript::Entry;
use saddle::watch::{ChangeSet, GitTracking, Observed, filter::Filter};

fn git(root: &Path, args: &[&str]) {
    let result = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn application(root: &Path, data: &Path, provider: ScriptedProvider) -> (App, EventReceivers) {
    let (session, restored) = Session::open_in(root, data).unwrap();
    let observed = Observed::default();
    observed.restore(restored.state.observed.clone());
    let mut files = if restored.existing {
        observed.resume_all(root, vec![])
    } else {
        observed.seed(&Filter::new(root, vec![]));
        vec![]
    };
    let tracking = GitTracking {
        tracker: Tracker::new(root.to_owned(), None, session.git_state()),
        session: Some(session.clone()),
    };
    let event = tracking
        .classify(&mut files, None, !restored.existing)
        .unwrap();
    let saved = session.clone();
    observed.set_persistence(Arc::new(move |snapshot| {
        saved.save_snapshots(snapshot).unwrap()
    }));
    session.save_snapshots(observed.snapshot()).unwrap();
    let (senders, receivers) = events::channels();
    let agent = Agent::new(
        Arc::new(provider),
        Registry::read_only(ProjectFs::new(root).unwrap(), observed.clone()),
        "system".into(),
        senders.agent.clone(),
    )
    .with_session(session.clone(), restored.messages.clone());
    let mut app = App::new(
        Config::parse(
            "model='test'\napi='messages'\nbase_url='http://localhost'",
            root,
        )
        .unwrap(),
        Credentials::parse("[inference]\napi_key='test'", root).unwrap(),
        root.to_owned(),
        Status {
            project: "test".into(),
            model: "test".into(),
            api: "messages".into(),
            sandbox: "test".into(),
            web_search: None,
        },
        senders,
    );
    app.attach_session(session, restored, observed, vec![]);
    app.attach_agent(AgentHandle::spawn(agent));
    app.attach_git(tracking);
    app.resume(
        ChangeSet {
            files,
            git: event.into_iter().collect(),
            ..Default::default()
        },
        BTreeMap::new(),
    );
    (app, receivers)
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

async fn close(mut app: App) {
    app.agent.take().unwrap().shutdown().await;
}

#[tokio::test]
async fn first_launch_resume_combines_offline_git_and_files_then_new_reorients() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let data = tempfile::tempdir().unwrap();
    git(&root, &["init", "-qb", "main"]);
    git(&root, &["config", "user.name", "Session Test"]);
    git(&root, &["config", "user.email", "session@example.invalid"]);
    fs::write(root.join("a.txt"), "before\n").unwrap();
    fs::write(root.join("dirty.txt"), "before\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "initial"]);
    let provider = ScriptedProvider::new([vec![text("Orientation.")]]);
    let (mut app, mut events) = application(&root, data.path(), provider.clone());
    finish(&mut app, &mut events).await;
    assert!(app.transcript.entries().any(|e| matches!(e, Entry::GitEvent { text, .. } if text.to_lowercase().contains("first launch"))));
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    close(app).await;

    fs::write(root.join("a.txt"), "committed offline\n").unwrap();
    git(&root, &["commit", "-qam", "offline commit"]);
    fs::write(root.join("dirty.txt"), "uncommitted offline\n").unwrap();
    let provider = ScriptedProvider::new([vec![text("Combined reaction.")]]);
    let (mut app, mut events) = application(&root, data.path(), provider.clone());
    finish(&mut app, &mut events).await;
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    let input = requests[0]
        .messages
        .last()
        .unwrap()
        .content
        .texts()
        .join("");
    assert!(input.contains("<git-event>"), "{input}");
    assert!(input.to_lowercase().contains("unseen commits"), "{input}");
    assert!(input.contains("offline commit"));
    assert!(input.contains("<file-changes>"));
    assert!(input.contains("dirty.txt"));
    assert!(!input.contains("+committed offline"));
    close(app).await;

    let provider = ScriptedProvider::new([vec![text("Fresh orientation.")]]);
    let (mut app, mut events) = application(&root, data.path(), provider.clone());
    assert!(!app.is_busy());
    assert!(provider.requests.lock().unwrap().is_empty());
    app.handle_terminal_event(Event::Paste("/new".into()));
    app.handle_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    finish(&mut app, &mut events).await;
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert_eq!(
        app.transcript
            .entries()
            .filter(|e| matches!(e, Entry::GitEvent { .. }))
            .count(),
        1
    );
    let text = provider.requests.lock().unwrap()[0]
        .messages
        .last()
        .unwrap()
        .content
        .texts()
        .join("");
    assert!(text.to_lowercase().contains("first launch"));
    assert!(!text.contains("unseen commits"));
    close(app).await;

    let provider = ScriptedProvider::new([]);
    let (app, _) = application(&root, data.path(), provider.clone());
    assert!(!app.is_busy());
    assert!(provider.requests.lock().unwrap().is_empty());
    close(app).await;
    let (_, restored) = Session::open_in(&root, data.path()).unwrap();
    assert_eq!(restored.state.branch.as_deref(), Some("main"));
    assert!(restored.state.per_branch.contains_key("main"));
}

#[test]
fn git_checkpoint_preserves_snapshots_and_rejects_old_generation_after_new() {
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let (session, _) = Session::open_in(root.path(), data.path()).unwrap();
    let generation = session.generation();
    let state = saddle::git::GitState {
        head: Some("old-tip".into()),
        ..Default::default()
    };
    session.save_git(state.clone(), &generation).unwrap();
    session.save_snapshots(Default::default()).unwrap();
    assert_eq!(session.git_state(), state);
    session.reset().unwrap();
    session.save_git(state, &generation).unwrap();
    assert_eq!(session.git_state(), Default::default());
}

#[tokio::test]
async fn live_checkout_and_commit_interrupt_active_turns_with_git_only_inputs() {
    use saddle::events::{AgentEvent, FsEvent};
    use saddle::provider::scripted::Step;
    use saddle::watch::{WatchConfig, Watcher};

    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let data = tempfile::tempdir().unwrap();
    git(&root, &["init", "-qb", "main"]);
    git(&root, &["config", "user.name", "Active Test"]);
    git(&root, &["config", "user.email", "active@example.invalid"]);
    fs::write(root.join("a.txt"), "main\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "initial"]);
    git(&root, &["checkout", "-qb", "feature"]);
    fs::write(root.join("a.txt"), "feature\n").unwrap();
    git(&root, &["commit", "-qam", "feature work"]);
    git(&root, &["checkout", "-q", "main"]);

    let provider = ScriptedProvider::new([
        vec![text("Orientation.")],
        vec![text("Partial before checkout"), Step::Hang],
        vec![text("Checkout reaction")],
        vec![text("Partial before commit"), Step::Hang],
        vec![text("Commit reaction")],
    ]);
    let (mut app, mut events) = application(&root, data.path(), provider.clone());
    finish(&mut app, &mut events).await;
    let tracking = GitTracking {
        tracker: Tracker::new(root.clone(), None, Default::default()),
        session: None,
    };
    tracking.classify(&mut vec![], None, true).unwrap();
    let _watcher = Watcher::spawn_with_git(
        root.clone(),
        WatchConfig {
            quiet: Duration::from_millis(200),
            max_window: Duration::from_secs(2),
            excluded: vec![],
        },
        Observed::default(),
        app.senders.fs.clone(),
        tracking,
    );
    tokio::time::sleep(Duration::from_millis(500)).await;

    for (command, needle) in [
        (vec!["checkout", "-q", "feature"], "branch switch"),
        (
            vec!["commit", "--allow-empty", "-qm", "empty marker"],
            "committed ",
        ),
    ] {
        app.handle_terminal_event(Event::Paste("Explain this at length".into()));
        app.handle_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = events.agent.recv().await.unwrap();
                let partial =
                    matches!(&event, AgentEvent::AssistantDelta(text) if text.contains("Partial"));
                app.handle_agent_event(event);
                if partial {
                    break;
                }
            }
        })
        .await
        .unwrap();
        git(&root, &command);
        let event = tokio::time::timeout(Duration::from_secs(5), events.fs.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(&event, FsEvent::Changed(set) if set.files.is_empty() && set.git.len() == 1)
        );
        app.handle_fs_event(event);
        // TurnFinished(interrupted) immediately submits the automatic successor.
        finish(&mut app, &mut events).await;
        let requests = provider.requests.lock().unwrap().clone();
        let input = requests
            .last()
            .unwrap()
            .messages
            .last()
            .unwrap()
            .content
            .texts()
            .join("");
        assert!(input.to_lowercase().contains(needle), "{input}");
        assert!(!input.contains("<file-changes>"));
    }
    assert_eq!(
        app.transcript
            .entries()
            .filter(|e| matches!(
                e,
                Entry::Assistant {
                    state: saddle::transcript::StreamState::Interrupted,
                    ..
                }
            ))
            .count(),
        2
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 5);
    close(app).await;
}
