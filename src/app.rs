use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::{Stream, StreamExt};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Block;
use ratatui_textarea::{TextArea, WrapMode};

use crate::agent::AgentHandle;
use crate::config::{Config, Credentials};
use crate::events::{
    AgentEvent, EventReceivers, EventSenders, FsEvent, JobEvent, LspEvent, TurnOutcome,
};
use crate::lsp::{ServerState, ServerStatus};
use crate::transcript::{Entry, NoticeLevel, Scroll, StreamState, Transcript};
use crate::ui;
use crate::watch::{ChangeSet, message};

/// What the status line shows.
#[derive(Debug, Clone)]
pub struct Status {
    pub project: String,
    pub model: String,
    pub api: String,
    pub sandbox: String,
    /// Shown when web search is enabled in config but cannot be offered.
    pub web_search: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Idle,
    /// A turn is running.
    Working,
    /// A turn is running and the model is reasoning.
    Thinking,
}

pub struct App {
    pub config: Config,
    pub credentials: Credentials,
    pub project_root: PathBuf,
    pub senders: EventSenders,
    pub status: Status,
    pub agent: Option<AgentHandle>,
    pub activity: Activity,
    pub transcript: Transcript,
    pub scroll: Scroll,
    pub input: TextArea<'static>,
    pub should_quit: bool,
    /// Change sets waiting for the interrupted turn to wind down.
    pending_changes: Vec<Arc<ChangeSet>>,
    pub lsp_status: Vec<ServerStatus>,
    /// Shell jobs still running, with their commands.
    pub running_jobs: BTreeMap<u64, String>,
    /// Transcript size from the last render, for scrolling: (total lines, viewport rows).
    pub(crate) transcript_metrics: (usize, usize),
}

impl App {
    pub fn new(
        config: Config,
        credentials: Credentials,
        project_root: PathBuf,
        status: Status,
        senders: EventSenders,
    ) -> Self {
        Self {
            config,
            credentials,
            project_root,
            senders,
            status,
            agent: None,
            activity: Activity::Idle,
            transcript: Transcript::default(),
            scroll: Scroll::default(),
            input: new_input(),
            should_quit: false,
            pending_changes: Vec::new(),
            lsp_status: Vec::new(),
            running_jobs: BTreeMap::new(),
            transcript_metrics: (0, 0),
        }
    }

    pub fn attach_agent(&mut self, agent: AgentHandle) {
        self.agent = Some(agent);
    }

    pub fn is_busy(&self) -> bool {
        self.activity != Activity::Idle
    }

    /// Stops the running turn, if any. Also the hook for file-change batching.
    pub fn interrupt(&mut self) {
        if self.is_busy()
            && let Some(agent) = &self.agent
        {
            agent.interrupt();
        }
    }

    pub fn handle_terminal_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.handle_key(key),
            Event::Paste(text) => {
                self.input
                    .insert_str(text.replace("\r\n", "\n").replace('\r', "\n"));
            }
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let (total, height) = self.transcript_metrics;
        let page = height.saturating_sub(1).max(1);

        match key.code {
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Esc => self.interrupt(),
            KeyCode::Char('o') if ctrl => self.transcript.toggle_expanded(),
            KeyCode::Enter if shift || alt => self.input.insert_newline(),
            KeyCode::Char('j') if ctrl => self.input.insert_newline(),
            KeyCode::Enter => self.submit(),
            KeyCode::PageUp => self.scroll.up(page, total, height),
            KeyCode::PageDown => self.scroll.down(page, total, height),
            KeyCode::Up if shift => self.scroll.up(1, total, height),
            KeyCode::Down if shift => self.scroll.down(1, total, height),
            KeyCode::Home if ctrl => self.scroll.top(total, height),
            KeyCode::End if ctrl => self.scroll.bottom(),
            _ => {
                self.input.input(key);
            }
        }
    }

    fn submit(&mut self) {
        let text = self.input.lines().join("\n");
        // While a turn runs the draft stays in the input; Esc interrupts.
        if text.trim().is_empty() || self.is_busy() {
            return;
        }
        self.input = new_input();
        self.transcript.push(Entry::User { text: text.clone() });
        match &self.agent {
            Some(agent) => {
                agent.send(text);
                self.activity = Activity::Working;
            }
            None => self.transcript.push(Entry::Notice {
                text: "No agent is connected.".into(),
                level: NoticeLevel::Error,
            }),
        }
        self.scroll.bottom();
    }

    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TurnStarted => self.activity = Activity::Working,
            AgentEvent::TurnFinished(outcome) => {
                self.activity = Activity::Idle;
                self.transcript.finish_assistant(StreamState::Interrupted);
                let changes = std::mem::take(&mut self.pending_changes);
                if outcome == TurnOutcome::Interrupted {
                    self.transcript.push(Entry::Notice {
                        text: if changes.is_empty() {
                            "Interrupted.".into()
                        } else {
                            "Interrupted: files changed.".into()
                        },
                        level: NoticeLevel::Info,
                    });
                }
                if !changes.is_empty() {
                    self.deliver_changes(&changes);
                }
            }
            AgentEvent::Thinking => self.activity = Activity::Thinking,
            AgentEvent::AssistantDelta(delta) => {
                self.activity = Activity::Working;
                self.transcript.append_assistant(&delta)
            }
            AgentEvent::AssistantDone { interrupted } => {
                self.transcript.finish_assistant(if interrupted {
                    StreamState::Interrupted
                } else {
                    StreamState::Complete
                })
            }
            AgentEvent::ToolCall { id, name, summary } => {
                self.transcript.finish_assistant(StreamState::Complete);
                self.transcript.push(Entry::ToolCall { id, name, summary })
            }
            AgentEvent::ToolResult {
                call_id,
                output,
                is_error,
            } => self.transcript.push(Entry::ToolResult {
                call_id,
                output,
                is_error,
            }),
            AgentEvent::Notice(text) => self.transcript.push(Entry::Notice {
                text,
                level: NoticeLevel::Info,
            }),
            AgentEvent::Error(text) => {
                self.transcript.finish_assistant(StreamState::Interrupted);
                self.transcript.push(Entry::Notice {
                    text,
                    level: NoticeLevel::Error,
                })
            }
        }
    }

    /// File changes always interrupt: a running turn is stopped and the
    /// changes start a new one once it has wound down.
    pub fn handle_fs_event(&mut self, event: FsEvent) {
        match event {
            FsEvent::Changed(changes) => {
                if self.is_busy()
                    && let Some(agent) = &self.agent
                {
                    agent.interrupt();
                    self.pending_changes.push(changes);
                } else {
                    self.deliver_changes(&[changes]);
                }
            }
            FsEvent::Warning(text) => self.transcript.push(Entry::Notice {
                text,
                level: NoticeLevel::Error,
            }),
        }
    }

    fn deliver_changes(&mut self, sets: &[Arc<ChangeSet>]) {
        let files: Vec<_> = sets
            .iter()
            .flat_map(|set| set.files.iter().cloned())
            .collect();
        self.transcript.push(Entry::FilesChanged {
            headline: message::headline(&files),
            paths: files.iter().map(|f| f.display.clone()).collect(),
            details: files.iter().map(message::file_line).collect(),
        });
        if let Some(agent) = &self.agent {
            agent.send(message::describe(&files));
            self.activity = Activity::Working;
        }
    }

    pub fn handle_lsp_event(&mut self, event: LspEvent) {
        match event {
            LspEvent::Status(statuses) => {
                for status in &statuses {
                    let before = self
                        .lsp_status
                        .iter()
                        .find(|s| s.language == status.language && s.root == status.root)
                        .map(|s| &s.state);
                    if before == Some(&status.state) {
                        continue;
                    }
                    let text = match &status.state {
                        ServerState::Restarting { attempt, reason } => format!(
                            "The {} language server stopped ({reason}); restarting (attempt {attempt}).",
                            status.language
                        ),
                        ServerState::Failed(reason) => {
                            format!("The {} language server failed: {reason}", status.language)
                        }
                        _ => continue,
                    };
                    self.transcript.push(Entry::Notice {
                        text,
                        level: NoticeLevel::Error,
                    });
                }
                self.lsp_status = statuses;
            }
        }
    }

    pub fn handle_job_event(&mut self, event: JobEvent) {
        match event {
            JobEvent::Started { id, command } => {
                self.running_jobs.insert(id, command);
            }
            JobEvent::Finished { id, summary } => {
                let command = self.running_jobs.remove(&id).unwrap_or_default();
                self.notice(format!("job {id} {summary}: {}", one_line(&command)), false);
            }
            JobEvent::SandboxLost(reason) => {
                self.status.sandbox = "lost, shell disabled".into();
                self.notice(format!("The shell tool is disabled: {reason}"), true);
            }
        }
    }

    /// Adds a notice to the transcript.
    pub fn notice(&mut self, text: String, error: bool) {
        self.transcript.push(Entry::Notice {
            text,
            level: if error {
                NoticeLevel::Error
            } else {
                NoticeLevel::Info
            },
        });
    }
}

/// The first line of `text`, marked when more follows, capped at 80 chars.
fn one_line(text: &str) -> String {
    let mut lines = text.trim().lines();
    let first = lines.next().unwrap_or_default();
    let mut out: String = first.chars().take(80).collect();
    if lines.next().is_some() || first.chars().count() > 80 {
        out.push('…');
    }
    out
}

fn new_input() -> TextArea<'static> {
    let mut input = TextArea::default();
    input.set_block(
        Block::bordered()
            .border_style(Style::new().add_modifier(Modifier::DIM))
            .title(" message "),
    );
    input.set_cursor_line_style(Style::new());
    input.set_wrap_mode(WrapMode::WordOrGlyph);
    input.set_placeholder_text("Ask about the codebase…");
    input
}

/// The central event loop: redraws, then waits for the next terminal, agent,
/// filesystem or job event, until the app asks to quit or input ends.
pub async fn run<B, S>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    mut terminal_events: S,
    mut events: EventReceivers,
) -> anyhow::Result<()>
where
    B: Backend,
    B::Error: Send + Sync + 'static,
    S: Stream<Item = io::Result<Event>> + Unpin,
{
    loop {
        terminal.draw(|frame| ui::render(frame, app))?;

        tokio::select! {
            event = terminal_events.next() => match event {
                Some(event) => app.handle_terminal_event(event?),
                None => return Ok(()),
            },
            Some(event) = events.agent.recv() => {
                app.handle_agent_event(event);
                // Coalesce a burst of streamed deltas into one redraw.
                while let Ok(event) = events.agent.try_recv() {
                    app.handle_agent_event(event);
                }
            }
            Some(event) = events.fs.recv() => app.handle_fs_event(event),
            Some(event) = events.job.recv() => app.handle_job_event(event),
            Some(event) = events.lsp.recv() => app.handle_lsp_event(event),
        }

        if app.should_quit {
            return Ok(());
        }
    }
}
