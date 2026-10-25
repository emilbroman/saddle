//! The sandboxed shell: `shell_start` launches an asynchronous job, the other
//! tools wait on, read, kill and list jobs. Waits are interruptible; the job
//! keeps running when a wait is cut short.

use std::time::Duration;

use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::jobs::{Jobs, WaitEnd};

use super::{ProjectFs, Registry, Tool, ToolOutput, str_arg};

/// How long `shell_start` waits so that quick commands return their result directly.
pub const START_GRACE: Duration = Duration::from_millis(1500);
const DEFAULT_WAIT: Duration = Duration::from_secs(60);
const MAX_WAIT_SECS: u64 = 600;

/// Registers the five shell tools.
pub fn install(registry: &mut Registry, jobs: Jobs, fs: ProjectFs) {
    let description: &'static str = Box::leak(start_description(&jobs).into_boxed_str());
    registry.register(ShellStart {
        jobs: jobs.clone(),
        fs,
        description,
    });
    registry.register(ShellWait(jobs.clone()));
    registry.register(ShellOutput(jobs.clone()));
    registry.register(ShellKill(jobs.clone()));
    registry.register(ShellList(jobs));
}

fn start_description(jobs: &Jobs) -> String {
    let sandbox = jobs.sandbox();
    let policy = sandbox.policy();
    let mut writable = vec![format!(
        "$TMPDIR ({}, private to this session)",
        sandbox.visible_scratch().display()
    )];
    writable.extend(
        policy
            .writable
            .iter()
            .map(|p| match p.strip_prefix(&policy.project_root) {
                Ok(rel) => format!("{}/ (in the project)", rel.display()),
                Err(_) => p.display().to_string(),
            }),
    );
    let mut text = String::from(
        "Start a shell command (`/bin/sh -c`) as a background job and return its job id. \
         If it finishes within 1.5 s, its status and output are returned directly; otherwise \
         use shell_wait to wait for it, shell_output to read new output, shell_kill to stop it.\n\
         Commands run in an OS-enforced sandbox: the project is read-only through every path, \
         including .git (so git commit, stash, checkout and the like fail), and so is the rest \
         of the filesystem, except for these writable paths: ",
    );
    text.push_str(&writable.join(", "));
    text.push_str(
        ". Network access is allowed. There is no terminal and no stdin: use non-interactive \
         flags. Builds and tests that write elsewhere fail with \"Read-only file system\" or \
         \"Operation not permitted\"; that is the sandbox, not a bug to work around. Never use \
         the shell to change project files.",
    );
    text
}

fn job_id(args: &Value) -> Result<u64, ToolOutput> {
    args.get("job_id")
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.trim().parse().ok()))
        .ok_or_else(|| ToolOutput::error("`job_id` must be a job number"))
}

fn job_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "job_id": { "type": "integer", "description": "Job id from shell_start." } },
        "required": ["job_id"]
    })
}

fn report(jobs: &Jobs, id: u64, note: Option<&str>) -> ToolOutput {
    match jobs.report(id) {
        Ok(report) => match note {
            Some(note) => ToolOutput::ok(format!("{note}\n{report}")),
            None => ToolOutput::ok(report),
        },
        Err(e) => ToolOutput::error(e),
    }
}

struct ShellStart {
    jobs: Jobs,
    fs: ProjectFs,
    description: &'static str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    command: String,
    #[serde(default)]
    cwd: Option<String>,
}

impl ShellStart {
    fn run(&self, args: Value, interrupt: CancellationToken) -> BoxFuture<'static, ToolOutput> {
        let jobs = self.jobs.clone();
        let fs = self.fs.clone();
        Box::pin(async move {
            let args: StartArgs = match super::parse_args(args) {
                Ok(args) => args,
                Err(e) => return e,
            };
            if args.command.trim().is_empty() {
                return ToolOutput::error("`command` must not be empty");
            }
            let cwd_arg = args.cwd.as_deref().unwrap_or(".");
            let cwd = match fs.resolve(cwd_arg) {
                Ok(cwd) if cwd.is_dir() => cwd,
                Ok(_) => return ToolOutput::error(format!("{cwd_arg}: not a directory")),
                Err(e) => return ToolOutput::error(e.to_string()),
            };
            let display = match cwd.strip_prefix(fs.root()) {
                Ok(rel) if rel.as_os_str().is_empty() => ".".to_owned(),
                Ok(rel) => rel.display().to_string(),
                Err(_) => cwd.display().to_string(),
            };
            let id = match jobs.start(&args.command, cwd, display).await {
                Ok(id) => id,
                Err(e) => return ToolOutput::error(e),
            };
            let note = match jobs.wait(id, START_GRACE, &interrupt).await {
                Ok(WaitEnd::Interrupted) => Some("Interrupted; the job keeps running."),
                _ => None,
            };
            report(&jobs, id, note)
        })
    }
}

impl Tool for ShellStart {
    fn name(&self) -> &'static str {
        "shell_start"
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command line." },
                "cwd": { "type": "string", "description": "Working directory relative to the project root (default: the root)." }
            },
            "required": ["command"]
        })
    }

    fn summary(&self, args: &Value) -> String {
        let command = str_arg(args, "command").unwrap_or("?");
        match str_arg(args, "cwd") {
            Some(cwd) if !matches!(cwd.trim(), "" | ".") => format!("(in {cwd}) {command}"),
            _ => command.to_owned(),
        }
    }

    fn call(&self, args: Value) -> BoxFuture<'static, ToolOutput> {
        self.run(args, CancellationToken::new())
    }

    fn call_interruptible(
        &self,
        args: Value,
        interrupt: CancellationToken,
    ) -> Option<BoxFuture<'static, ToolOutput>> {
        Some(self.run(args, interrupt))
    }
}

struct ShellWait(Jobs);

impl ShellWait {
    fn run(&self, args: Value, interrupt: CancellationToken) -> BoxFuture<'static, ToolOutput> {
        let jobs = self.0.clone();
        Box::pin(async move {
            let id = match job_id(&args) {
                Ok(id) => id,
                Err(e) => return e,
            };
            let timeout = args
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .map_or(DEFAULT_WAIT, |s| {
                    Duration::from_secs(s.clamp(1, MAX_WAIT_SECS))
                });
            let note = match jobs.wait(id, timeout, &interrupt).await {
                Ok(WaitEnd::Exited) => None,
                Ok(WaitEnd::TimedOut) => Some(format!(
                    "Waited {} s; the job keeps running.",
                    timeout.as_secs()
                )),
                Ok(WaitEnd::Interrupted) => {
                    Some("Wait interrupted; the job keeps running.".to_owned())
                }
                Err(e) => return ToolOutput::error(e),
            };
            report(&jobs, id, note.as_deref())
        })
    }
}

impl Tool for ShellWait {
    fn name(&self) -> &'static str {
        "shell_wait"
    }

    fn description(&self) -> &'static str {
        "Wait for a job to finish, up to timeout_secs (default 60, at most 600), and return its \
         status and the output produced since it was last read. If the wait times out or is \
         interrupted (for example because the user saved files), the job keeps running and can \
         be waited on again."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "integer", "description": "Job id from shell_start." },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": MAX_WAIT_SECS, "description": "Maximum time to wait (default 60)." }
            },
            "required": ["job_id"]
        })
    }

    fn summary(&self, args: &Value) -> String {
        let id = job_id(args).map_or("?".into(), |id| id.to_string());
        match args.get("timeout_secs").and_then(Value::as_u64) {
            Some(secs) => format!("job {id} (up to {secs}s)"),
            None => format!("job {id}"),
        }
    }

    fn call(&self, args: Value) -> BoxFuture<'static, ToolOutput> {
        self.run(args, CancellationToken::new())
    }

    fn call_interruptible(
        &self,
        args: Value,
        interrupt: CancellationToken,
    ) -> Option<BoxFuture<'static, ToolOutput>> {
        Some(self.run(args, interrupt))
    }
}

struct ShellOutput(Jobs);

impl Tool for ShellOutput {
    fn name(&self) -> &'static str {
        "shell_output"
    }

    fn description(&self) -> &'static str {
        "Return a job's status and the output it produced since it was last read, without waiting."
    }

    fn schema(&self) -> Value {
        job_schema()
    }

    fn summary(&self, args: &Value) -> String {
        job_id(args).map_or("job ?".into(), |id| format!("job {id}"))
    }

    fn call(&self, args: Value) -> BoxFuture<'static, ToolOutput> {
        let output = match job_id(&args) {
            Ok(id) => report(&self.0, id, None),
            Err(e) => e,
        };
        Box::pin(async move { output })
    }
}

struct ShellKill(Jobs);

impl Tool for ShellKill {
    fn name(&self) -> &'static str {
        "shell_kill"
    }

    fn description(&self) -> &'static str {
        "Terminate a running job and everything it started (SIGTERM, then SIGKILL after 2 s)."
    }

    fn schema(&self) -> Value {
        job_schema()
    }

    fn summary(&self, args: &Value) -> String {
        job_id(args).map_or("job ?".into(), |id| format!("job {id}"))
    }

    fn call(&self, args: Value) -> BoxFuture<'static, ToolOutput> {
        let jobs = self.0.clone();
        Box::pin(async move {
            let id = match job_id(&args) {
                Ok(id) => id,
                Err(e) => return e,
            };
            match jobs.kill(id) {
                Ok(true) => {
                    let never = CancellationToken::new();
                    let _ = jobs.wait(id, Duration::from_secs(3), &never).await;
                    report(&jobs, id, None)
                }
                Ok(false) => report(&jobs, id, Some("The job had already finished.")),
                Err(e) => ToolOutput::error(e),
            }
        })
    }
}

struct ShellList(Jobs);

impl Tool for ShellList {
    fn name(&self) -> &'static str {
        "shell_list"
    }

    fn description(&self) -> &'static str {
        "List this session's jobs with their status, working directory and command."
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn summary(&self, _: &Value) -> String {
        String::new()
    }

    fn call(&self, _: Value) -> BoxFuture<'static, ToolOutput> {
        let list = self.0.list();
        Box::pin(async move { ToolOutput::ok(list) })
    }
}
