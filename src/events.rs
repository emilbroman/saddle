//! Channels feeding the central event loop besides terminal input. Producers
//! (agent loop, file watcher, shell jobs) arrive in later slices; they get a
//! clone of the relevant sender from [`EventSenders`].

use std::path::PathBuf;

use tokio::sync::mpsc;

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
    Changed(Vec<PathBuf>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEvent {
    Finished { id: u64, exit_code: Option<i32> },
}

#[derive(Debug, Clone)]
pub struct EventSenders {
    pub agent: mpsc::UnboundedSender<AgentEvent>,
    pub fs: mpsc::UnboundedSender<FsEvent>,
    pub job: mpsc::UnboundedSender<JobEvent>,
}

#[derive(Debug)]
pub struct EventReceivers {
    pub agent: mpsc::UnboundedReceiver<AgentEvent>,
    pub fs: mpsc::UnboundedReceiver<FsEvent>,
    pub job: mpsc::UnboundedReceiver<JobEvent>,
}

pub fn channels() -> (EventSenders, EventReceivers) {
    let (agent_tx, agent_rx) = mpsc::unbounded_channel();
    let (fs_tx, fs_rx) = mpsc::unbounded_channel();
    let (job_tx, job_rx) = mpsc::unbounded_channel();
    (
        EventSenders {
            agent: agent_tx,
            fs: fs_tx,
            job: job_tx,
        },
        EventReceivers {
            agent: agent_rx,
            fs: fs_rx,
            job: job_rx,
        },
    )
}
