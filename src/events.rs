//! Channels feeding the central event loop besides terminal input. Producers
//! (agent loop, file watcher, shell jobs) arrive in later slices; they get a
//! clone of the relevant sender from [`EventSenders`].

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::watch::ChangeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The model stopped of its own accord.
    Completed,
    Interrupted,
    Failed,
    /// The turn hit the tool-round limit.
    Exhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    Usage {
        prompt_tokens: usize,
    },
    TurnStarted,
    TurnFinished(TurnOutcome),
    /// The model is reasoning; the content itself is not shown.
    Thinking,
    AssistantDelta(String),
    AssistantDone {
        interrupted: bool,
    },
    ToolCall {
        id: String,
        name: String,
        summary: String,
    },
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
    Notice(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsEvent {
    /// Interrupt now; the combined change event follows after diagnostics settle.
    Settling,
    /// A batch of saved changes, never empty.
    Changed(Arc<ChangeSet>),
    /// The watcher could not watch (part of) the project.
    Warning(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEvent {
    Started {
        id: u64,
        command: String,
    },
    /// `summary` is e.g. "exited with status 0".
    Finished {
        id: u64,
        summary: String,
    },
    /// A pre-command probe failed; the shell is disabled for the session.
    SandboxLost(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspEvent {
    Status(Vec<crate::lsp::ServerStatus>),
}

#[derive(Debug, Clone)]
pub struct EventSenders {
    pub agent: mpsc::UnboundedSender<AgentEvent>,
    pub fs: mpsc::UnboundedSender<FsEvent>,
    pub job: mpsc::UnboundedSender<JobEvent>,
    pub lsp: mpsc::UnboundedSender<LspEvent>,
}

#[derive(Debug)]
pub struct EventReceivers {
    pub agent: mpsc::UnboundedReceiver<AgentEvent>,
    pub fs: mpsc::UnboundedReceiver<FsEvent>,
    pub job: mpsc::UnboundedReceiver<JobEvent>,
    pub lsp: mpsc::UnboundedReceiver<LspEvent>,
}

pub fn channels() -> (EventSenders, EventReceivers) {
    let (agent_tx, agent_rx) = mpsc::unbounded_channel();
    let (fs_tx, fs_rx) = mpsc::unbounded_channel();
    let (job_tx, job_rx) = mpsc::unbounded_channel();
    let (lsp_tx, lsp_rx) = mpsc::unbounded_channel();
    (
        EventSenders {
            agent: agent_tx,
            fs: fs_tx,
            job: job_tx,
            lsp: lsp_tx,
        },
        EventReceivers {
            agent: agent_rx,
            fs: fs_rx,
            job: job_rx,
            lsp: lsp_rx,
        },
    )
}
