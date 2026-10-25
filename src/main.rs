use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use crossterm::event::EventStream;

use saddle::app::{self, App, Status};
use saddle::{config, events, project, tui};

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

    let status = Status {
        project: project::display_path(&project_root, home.as_deref()),
        model: loaded.config.model.clone(),
        api: loaded.config.api.to_string(),
        sandbox: "not available".into(),
    };
    let (senders, receivers) = events::channels();
    let mut app = App::new(
        loaded.config,
        loaded.credentials,
        project_root,
        status,
        senders,
    );

    let (mut terminal, guard) = tui::init().context("cannot initialise the terminal")?;
    let result = app::run(&mut terminal, &mut app, EventStream::new(), receivers).await;
    drop(guard);
    result
}
