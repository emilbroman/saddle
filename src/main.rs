use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::EventStream;

use std::sync::Arc;

use saddle::agent::{Agent, AgentHandle};
use saddle::app::{self, App, Status};
use saddle::jobs::Jobs;
use saddle::lsp::LspManager;
use saddle::provider::GenaiProvider;
use saddle::sandbox::{self, Policy, Sandbox, Setup};
use saddle::tools::{ProjectFs, Registry, lsp as lsp_tools, shell, web_search};
use saddle::watch::{Observed, WatchConfig, Watcher};
use saddle::{config, events, project, prompt, tui};
use tokio::sync::mpsc;

fn main() -> ExitCode {
    // The Landlock helper must restrict itself while still single-threaded.
    #[cfg(target_os = "linux")]
    {
        let mut args = std::env::args_os().skip(1);
        if args
            .next()
            .is_some_and(|a| a == sandbox::LANDLOCK_HELPER_ARG)
        {
            sandbox::landlock::exec_helper(args.collect());
        }
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("saddle: cannot start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
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

    let writable = loaded.config.writable_paths(&project_root, home.as_deref());
    let mut status = Status {
        project: project::display_path(&project_root, home.as_deref()),
        model: loaded.config.model.clone(),
        api: loaded.config.api.to_string(),
        sandbox: String::new(),
        web_search: None,
    };
    let provider = GenaiProvider::new(&loaded.config, &loaded.credentials, home.as_deref())
        .context("cannot set up the inference client")?;
    let fs = ProjectFs::new(&project_root)
        .with_context(|| format!("cannot open project root {}", project_root.display()))?;
    let (senders, receivers) = events::channels();
    let observed = Observed::default();
    let (changes_tx, changes_rx) = mpsc::unbounded_channel();
    let _watcher = Watcher::spawn(
        project_root.clone(),
        WatchConfig {
            quiet: Duration::from_millis(loaded.config.batching.quiet_ms),
            max_window: Duration::from_millis(loaded.config.batching.max_window_ms),
            excluded: writable.clone(),
        },
        observed.clone(),
        changes_tx,
    );
    let mut tools = Registry::read_only(fs.clone(), observed);
    status.web_search = web_search::install(
        &mut tools,
        &loaded.config.search,
        loaded.credentials.exa.as_ref(),
    )
    .context("cannot set up the web search client")?;
    let lsp = LspManager::new(
        project_root.clone(),
        &loaded.config.lsp,
        Some(senders.lsp.clone()),
    );
    lsp.start_detected();
    lsp_tools::install(&mut tools, lsp.clone(), fs.clone());
    let changes_task = tokio::spawn(saddle::lsp::changes::forward(
        changes_rx,
        senders.fs.clone(),
        lsp.clone(),
        Duration::from_millis(loaded.config.batching.diagnostics_quiet_ms),
        Duration::from_millis(loaded.config.batching.diagnostics_max_wait_ms),
    ));
    let scratch = tempfile::Builder::new()
        .prefix("saddle-")
        .tempdir()
        .context("cannot create the shell's scratch directory")?;
    let (shell, mut sandbox_notes) = shell_sandbox(
        &loaded.config.sandbox,
        &loaded.dir,
        &project_root,
        scratch.path(),
        writable,
    )
    .await;
    let jobs = match shell {
        Ok(sandbox) => {
            status.sandbox = sandbox.backend().name().into();
            let jobs = Jobs::new(Arc::new(sandbox), senders.job.clone());
            shell::install(&mut tools, jobs.clone(), fs);
            Some(jobs)
        }
        Err(reason) => {
            status.sandbox = "unavailable, shell disabled".into();
            sandbox_notes.push((
                format!("The shell tool is disabled: no sandbox could be established ({reason})."),
                true,
            ));
            None
        }
    };
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
    for (text, error) in sandbox_notes {
        app.notice(text, error);
    }

    let (mut terminal, guard) = tui::init().context("cannot initialise the terminal")?;
    // On SIGHUP/SIGTERM (a closed terminal window) still kill the jobs and
    // remove the scratch directory.
    let result = tokio::select! {
        result = app::run(&mut terminal, &mut app, EventStream::new(), receivers) => result,
        _ = termination() => Ok(()),
    };
    drop(guard);
    if let Some(jobs) = jobs {
        jobs.shutdown().await;
    }
    drop(scratch);
    changes_task.abort();
    lsp.shutdown().await;
    result
}

async fn termination() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut hangup), Ok(mut terminate)) = (
        signal(SignalKind::hangup()),
        signal(SignalKind::terminate()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = hangup.recv() => {}
        _ = terminate.recv() => {}
    }
}

/// Builds the sandbox policy and establishes a backend. Also returns
/// transcript notices: (text, is_error).
async fn shell_sandbox(
    config: &config::SandboxConfig,
    config_dir: &Path,
    project_root: &Path,
    scratch: &Path,
    mut writable: Vec<PathBuf>,
) -> (Result<Sandbox, String>, Vec<(String, bool)>) {
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME").filter(|v| !v.is_empty()) {
        writable.push(PathBuf::from(cargo_home));
    }
    let hidden = [config_dir.join(config::CREDENTIALS_FILE)];
    let (policy, dropped) = match Policy::new(project_root, &writable, scratch, &hidden) {
        Ok(result) => result,
        Err(e) => {
            return (
                Err(format!("cannot resolve sandbox paths: {e}")),
                Vec::new(),
            );
        }
    };
    let notes = dropped
        .into_iter()
        .map(|note| (format!("Sandbox: {note}"), false))
        .collect();
    let setup = Setup {
        policy,
        env: sandbox::environment(std::env::vars_os(), &config.env),
    };
    let launcher = std::env::current_exe().ok();
    (
        Sandbox::establish(config.backend, setup, launcher).await,
        notes,
    )
}
