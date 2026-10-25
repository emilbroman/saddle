//! Asynchronous shell jobs: commands started under the sandbox in their own
//! process group, with output captured into a bounded unread window (for the
//! model) and a capped log file in the scratch directory (for later greps).
//! A job outlives any wait on it; only `kill` or shutdown stops it.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::events::JobEvent;
use crate::sandbox::Sandbox;

/// Bytes of unread output kept from the start and from the end of what
/// arrived since the last read; the middle is counted and dropped.
const HEAD_BYTES: usize = 8 * 1024;
const TAIL_BYTES: usize = 24 * 1024;
/// Upper bound on a job's log file.
const LOG_BYTES: u64 = 64 * 1024 * 1024;
/// How long to wait for buffered output after the process exits.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
/// Between SIGTERM and SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Exited(i32),
    Signaled(i32),
}

impl JobStatus {
    pub fn describe(self, killed: bool) -> String {
        match (self, killed) {
            (JobStatus::Running, _) => "running".into(),
            (JobStatus::Exited(code), _) => format!("exited with status {code}"),
            (JobStatus::Signaled(_), true) => "killed".into(),
            (JobStatus::Signaled(signal), false) => format!("killed by signal {signal}"),
        }
    }
}

#[derive(Debug, Default)]
struct Unread {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    omitted: u64,
}

impl Unread {
    fn push(&mut self, mut bytes: &[u8]) {
        if self.head.len() < HEAD_BYTES {
            let n = bytes.len().min(HEAD_BYTES - self.head.len());
            self.head.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
        }
        self.tail.extend(bytes);
        let excess = self.tail.len().saturating_sub(TAIL_BYTES);
        self.tail.drain(..excess);
        self.omitted += excess as u64;
    }

    fn take(&mut self) -> String {
        let mut out = String::from_utf8_lossy(&self.head).into_owned();
        if self.omitted > 0 {
            let _ = write!(
                out,
                "\n[… {} bytes of output omitted; see the full log …]\n",
                self.omitted
            );
        }
        let tail: Vec<u8> = self.tail.drain(..).collect();
        out.push_str(&String::from_utf8_lossy(&tail));
        *self = Unread::default();
        out
    }
}

#[derive(Debug)]
struct Job {
    command: String,
    cwd: String,
    started: Instant,
    finished: Option<Instant>,
    pgid: i32,
    status: JobStatus,
    killed: bool,
    unread: Unread,
    /// The log as the sandbox sees it.
    log: PathBuf,
    done: watch::Receiver<bool>,
}

impl Job {
    fn elapsed(&self) -> Duration {
        self.finished.unwrap_or_else(Instant::now) - self.started
    }

    fn headline(&self, id: u64) -> String {
        format!(
            "job {id} {} after {}",
            self.status.describe(self.killed),
            format_duration(self.elapsed())
        )
    }
}

#[derive(Debug, Default)]
struct State {
    next_id: u64,
    jobs: BTreeMap<u64, Job>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitEnd {
    Exited,
    TimedOut,
    Interrupted,
}

#[derive(Clone)]
pub struct Jobs {
    state: Arc<Mutex<State>>,
    sandbox: Arc<Sandbox>,
    events: mpsc::UnboundedSender<JobEvent>,
}

impl Jobs {
    pub fn new(sandbox: Arc<Sandbox>, events: mpsc::UnboundedSender<JobEvent>) -> Self {
        Self {
            state: Arc::default(),
            sandbox,
            events,
        }
    }

    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    /// Starts `command` in `cwd` (a canonical path inside the project) under
    /// the sandbox, after re-probing it.
    pub async fn start(
        &self,
        command: &str,
        cwd: PathBuf,
        cwd_display: String,
    ) -> Result<u64, String> {
        if let Err(reason) = self.sandbox.check().await {
            let _ = self.events.send(JobEvent::SandboxLost(reason.clone()));
            return Err(format!("The shell is disabled: {reason}"));
        }
        let id = {
            let mut state = self.state.lock().unwrap();
            state.next_id += 1;
            state.next_id
        };
        let logs = self.sandbox.policy().scratch.join("jobs");
        std::fs::create_dir_all(&logs)
            .map_err(|e| format!("cannot create the job log directory: {e}"))?;
        let log_file = std::fs::File::create(logs.join(format!("{id}.log")))
            .map_err(|e| format!("cannot create the job log: {e}"))?;

        let (reader, writer) = std::io::pipe().map_err(|e| e.to_string())?;
        let writer_err = writer.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = self.sandbox.invocation(command, &cwd).command();
        cmd.stdout(Stdio::from(writer))
            .stderr(Stdio::from(writer_err))
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start the sandbox: {e}"))?;
        drop(cmd);
        let pgid = child.id().map_or(0, |pid| pid as i32);

        let (done_tx, done_rx) = watch::channel(false);
        self.state.lock().unwrap().jobs.insert(
            id,
            Job {
                command: command.to_owned(),
                cwd: cwd_display,
                started: Instant::now(),
                finished: None,
                pgid,
                status: JobStatus::Running,
                killed: false,
                unread: Unread::default(),
                log: self
                    .sandbox
                    .visible_scratch()
                    .join("jobs")
                    .join(format!("{id}.log")),
                done: done_rx,
            },
        );
        let _ = self.events.send(JobEvent::Started {
            id,
            command: command.to_owned(),
        });

        let (drained_tx, drained_rx) = oneshot::channel();
        let state = self.state.clone();
        std::thread::spawn(move || capture(reader, log_file, state, id, drained_tx));

        let state = self.state.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let status = match child.wait().await {
                Ok(status) => match (status.code(), status.signal()) {
                    (Some(code), _) => JobStatus::Exited(code),
                    (None, Some(signal)) => JobStatus::Signaled(signal),
                    (None, None) => JobStatus::Exited(-1),
                },
                Err(_) => JobStatus::Exited(-1),
            };
            let _ = tokio::time::timeout(DRAIN_TIMEOUT, drained_rx).await;
            let summary = {
                let mut state = state.lock().unwrap();
                let job = state.jobs.get_mut(&id).expect("jobs are never removed");
                job.status = status;
                job.finished = Some(Instant::now());
                status.describe(job.killed)
            };
            let _ = done_tx.send(true);
            let _ = events.send(JobEvent::Finished { id, summary });
        });
        Ok(id)
    }

    /// Waits until the job exits, `timeout` passes or `interrupt` fires. The
    /// job keeps running in the latter two cases.
    pub async fn wait(
        &self,
        id: u64,
        timeout: Duration,
        interrupt: &CancellationToken,
    ) -> Result<WaitEnd, String> {
        let mut done = self.with_job(id, |job| job.done.clone())?;
        tokio::select! {
            biased;
            _ = done.wait_for(|done| *done) => Ok(WaitEnd::Exited),
            _ = interrupt.cancelled() => Ok(WaitEnd::Interrupted),
            _ = tokio::time::sleep(timeout) => Ok(WaitEnd::TimedOut),
        }
    }

    /// The job's status line followed by its output since the last read.
    pub fn report(&self, id: u64) -> Result<String, String> {
        self.with_job(id, |job| {
            let output = job.unread.take();
            let mut report = job.headline(id);
            if job.status == JobStatus::Running {
                let _ = write!(
                    report,
                    " (still running; shell_wait or shell_output with job_id {id})"
                );
            }
            report.push('\n');
            if output.is_empty() {
                report.push_str("(no new output)\n");
            } else {
                report.push_str(&output);
                if !output.ends_with('\n') {
                    report.push('\n');
                }
            }
            let _ = write!(report, "[full log: {}]", job.log.display());
            report
        })
    }

    pub fn is_running(&self, id: u64) -> Result<bool, String> {
        self.with_job(id, |job| job.status == JobStatus::Running)
    }

    /// Terminates the job's process group: SIGTERM, then SIGKILL.
    pub fn kill(&self, id: u64) -> Result<bool, String> {
        let pgid = self.with_job(id, |job| {
            (job.status == JobStatus::Running).then(|| {
                job.killed = true;
                job.pgid
            })
        })?;
        let Some(pgid) = pgid else {
            return Ok(false);
        };
        signal_group(pgid, libc::SIGTERM);
        let jobs = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(KILL_GRACE).await;
            if jobs.is_running(id).unwrap_or(false) {
                signal_group(pgid, libc::SIGKILL);
            }
        });
        Ok(true)
    }

    pub fn list(&self) -> String {
        let state = self.state.lock().unwrap();
        if state.jobs.is_empty() {
            return "No jobs have been started.".into();
        }
        state
            .jobs
            .iter()
            .map(|(id, job)| format!("{} · in {} · {}", job.headline(*id), job.cwd, job.command))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Kills every running job and waits briefly for them to go.
    pub async fn shutdown(&self) {
        let running: Vec<u64> = {
            let state = self.state.lock().unwrap();
            state
                .jobs
                .iter()
                .filter(|(_, job)| job.status == JobStatus::Running)
                .map(|(id, _)| *id)
                .collect()
        };
        for id in &running {
            let _ = self.kill(*id);
        }
        let deadline = Instant::now() + KILL_GRACE + Duration::from_millis(500);
        while Instant::now() < deadline
            && running
                .iter()
                .any(|id| self.is_running(*id).unwrap_or(false))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn with_job<T>(&self, id: u64, f: impl FnOnce(&mut Job) -> T) -> Result<T, String> {
        let mut state = self.state.lock().unwrap();
        match state.jobs.get_mut(&id) {
            Some(job) => Ok(f(job)),
            None => Err(format!("there is no job {id}; shell_list shows the jobs")),
        }
    }
}

fn signal_group(pgid: i32, signal: i32) {
    if pgid > 0 {
        // SAFETY: kill(2) on a process group we created.
        unsafe {
            libc::kill(-pgid, signal);
        }
    }
}

fn capture(
    mut reader: std::io::PipeReader,
    mut log: std::fs::File,
    state: Arc<Mutex<State>>,
    id: u64,
    drained: oneshot::Sender<()>,
) {
    let mut buf = [0u8; 8192];
    let mut logged = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if logged < LOG_BYTES {
            let n = n.min((LOG_BYTES - logged) as usize);
            let _ = log.write_all(&buf[..n]);
            logged += n as u64;
            if logged == LOG_BYTES {
                let _ = log.write_all(b"\n[log truncated]\n");
            }
        }
        if let Some(job) = state.lock().unwrap().jobs.get_mut(&id) {
            job.unread.push(&buf[..n]);
        }
    }
    let _ = drained.send(());
}

pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_keeps_head_and_tail() {
        let mut unread = Unread::default();
        unread.push(b"hello ");
        unread.push(b"world");
        assert_eq!(unread.take(), "hello world");
        assert_eq!(unread.take(), "");

        let big: Vec<u8> = (0..HEAD_BYTES + TAIL_BYTES + 100)
            .map(|i| b'a' + (i % 26) as u8)
            .collect();
        for chunk in big.chunks(1000) {
            unread.push(chunk);
        }
        let out = unread.take();
        assert!(out.starts_with(std::str::from_utf8(&big[..HEAD_BYTES]).unwrap()));
        assert!(out.ends_with(std::str::from_utf8(&big[big.len() - TAIL_BYTES..]).unwrap()));
        assert!(out.contains("[… 100 bytes of output omitted"));
    }

    #[test]
    fn status_descriptions() {
        assert_eq!(JobStatus::Exited(0).describe(false), "exited with status 0");
        assert_eq!(JobStatus::Signaled(15).describe(true), "killed");
        assert_eq!(JobStatus::Signaled(9).describe(false), "killed by signal 9");
        assert_eq!(format_duration(Duration::from_millis(1234)), "1.2s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m05s");
    }
}
