//! Channels feeding the central event loop besides terminal input. Producers
//! (agent loop, file watcher, shell jobs) arrive in later slices; they get a
//! clone of the relevant sender from [`EventSenders`].

use std::path::PathBuf;

use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
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
