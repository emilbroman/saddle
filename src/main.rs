use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::EventStream;

use std::sync::Arc;

use saddle::agent::{Agent, AgentHandle};
use saddle::app::{self, App, Status};
use saddle::provider::GenaiProvider;
use saddle::tools::{ProjectFs, Registry, web_search};
use saddle::watch::{Observed, WatchConfig, Watcher};
use saddle::{config, events, project, prompt, tui};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("saddle: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "-V" | "--version" => {
                println!("saddle {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-h" | "--help" => {
                println!(
                    "Usage: saddle\n\nRun inside the project to work on. See the README for configuration."
                );
                return Ok(());
            }
            other => anyhow::bail!("unexpected argument {other:?} (see --help)"),
        }
    }

    let loaded = config::load()?;
    let cwd = std::env::current_dir().context("cannot determine the current directory")?;
    let project_root = project::resolve_root(&cwd)
        .with_context(|| format!("cannot resolve project root from {}", cwd.display()))?;
    let home = std::env::var_os("HOME").map(PathBuf::from);

    let mut status = Status {
        project: project::display_path(&project_root, home.as_deref()),
        model: loaded.config.model.clone(),
        api: loaded.config.api.to_string(),
        sandbox: "not available".into(),
        web_search: None,
    };
    let provider = GenaiProvider::new(&loaded.config, &loaded.credentials, home.as_deref())
        .context("cannot set up the inference client")?;
    let fs = ProjectFs::new(&project_root)
        .with_context(|| format!("cannot open project root {}", project_root.display()))?;
    let (senders, receivers) = events::channels();
    let observed = Observed::default();
    let _watcher = Watcher::spawn(
        project_root.clone(),
        WatchConfig {
            quiet: Duration::from_millis(loaded.config.batching.quiet_ms),
            max_window: Duration::from_millis(loaded.config.batching.max_window_ms),
            excluded: loaded.config.writable_paths(&project_root, home.as_deref()),
        },
        observed.clone(),
        senders.fs.clone(),
    );
    let mut tools = Registry::read_only(fs, observed);
    status.web_search = web_search::install(
        &mut tools,
        &loaded.config.search,
        loaded.credentials.exa.as_ref(),
    )
    .context("cannot set up the web search client")?;
    let agent = Agent::new(
        Arc::new(provider),
        tools,
        prompt::system(&project_root),
        senders.agent.clone(),
    );
    let mut app = App::new(
        loaded.config,
        loaded.credentials,
        project_root,
        status,
        senders,
    );
    app.attach_agent(AgentHandle::spawn(agent));

    let (mut terminal, guard) = tui::init().context("cannot initialise the terminal")?;
    let result = app::run(&mut terminal, &mut app, EventStream::new(), receivers).await;
    drop(guard);
    result
}
