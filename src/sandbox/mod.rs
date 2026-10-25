//! OS-enforced confinement for the agent's shell commands: the project is
//! read-only through every path, and only explicit carve-outs and a per-session
//! scratch directory are writable. Bubblewrap (with a Landlock fallback) on
//! Linux, Seatbelt (`sandbox-exec`) on macOS. When no backend can be
//! established the shell is disabled; commands never run unconfined.

mod bwrap;
mod env;
#[cfg(target_os = "linux")]
pub mod landlock;
mod seatbelt;

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

use serde::Deserialize;

pub use env::environment;

/// Which mechanism to use; `auto` picks the best one available on the platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendChoice {
    #[default]
    Auto,
    Bwrap,
    Landlock,
    Seatbelt,
}

/// What the sandbox allows, with every path canonical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub project_root: PathBuf,
    /// Writable carve-outs: inside the project (build output) or outside it
    /// (toolchain caches). Never the project root, an ancestor of it, or `.git`.
    pub writable: Vec<PathBuf>,
    /// The per-session scratch directory, `TMPDIR` inside the sandbox.
    pub scratch: PathBuf,
    /// Files whose contents the sandbox hides, such as Saddle's credentials.
    pub hidden: Vec<PathBuf>,
}

impl Policy {
    /// Validates the requested carve-outs. Missing directories inside the
    /// project are created (they are build-output directories); missing ones
    /// outside it are skipped. Anything that would expose the project is
    /// dropped. Returns the policy and a note per dropped path.
    pub fn new(
        project_root: &Path,
        requested: &[PathBuf],
        scratch: &Path,
        hidden: &[PathBuf],
    ) -> io::Result<(Self, Vec<String>)> {
        let root = std::fs::canonicalize(project_root)?;
        let scratch = std::fs::canonicalize(scratch)?;
        let mut notes = Vec::new();
        let mut writable: Vec<PathBuf> = Vec::new();
        for path in requested {
            match carve_out(&root, path) {
                Ok(Some(path)) if !writable.contains(&path) => writable.push(path),
                Ok(_) => {}
                Err(reason) => notes.push(format!("{}: {reason}", path.display())),
            }
        }
        let hidden = hidden
            .iter()
            .filter_map(|p| std::fs::canonicalize(p).ok())
            .filter(|p| p.is_file())
            .collect();
        Ok((
            Self {
                project_root: root,
                writable,
                scratch,
                hidden,
            },
            notes,
        ))
    }
}

fn carve_out(root: &Path, path: &Path) -> Result<Option<PathBuf>, String> {
    let normal = lexically_normal(path);
    if normal.starts_with(root) && normal != root && !path.exists() {
        std::fs::create_dir_all(&normal).map_err(|e| format!("cannot create it: {e}"))?;
    }
    let Ok(canonical) = std::fs::canonicalize(&normal) else {
        return Ok(None);
    };
    if root.starts_with(&canonical) {
        return Err("ignored as a writable path: it contains the project".into());
    }
    if canonical.starts_with(root.join(".git")) {
        return Err("ignored as a writable path: it is inside .git".into());
    }
    Ok(Some(canonical))
}

fn lexically_normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcMount {
    /// A fresh procfs for the sandbox's PID namespace.
    Fresh,
    /// The host's `/proc`, read-only: for hosts (such as unprivileged
    /// containers) where mounting a new procfs is refused.
    HostReadOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    Bwrap {
        program: PathBuf,
        proc: ProcMount,
    },
    /// Re-executes `launcher` (the Saddle binary) in its Landlock helper mode.
    Landlock {
        launcher: PathBuf,
    },
    Seatbelt,
}

impl Backend {
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Bwrap { .. } => "bwrap",
            Backend::Landlock { .. } => "landlock",
            Backend::Seatbelt => "seatbelt",
        }
    }
}

/// A program, its arguments and its complete environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
}

impl Invocation {
    /// The command, in its own session and process group, with every
    /// inherited descriptor above stderr closed on exec.
    pub fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(&self.args)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&self.cwd)
            .stdin(Stdio::null());
        // SAFETY: only async-signal-safe calls (setsid, close_range/fcntl).
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                cloexec_from(3);
                Ok(())
            });
        }
        command
    }
}

/// Marks every descriptor from `first` up as close-on-exec. Marking rather
/// than closing keeps the spawner's own exec-error pipe working.
fn cloexec_from(first: i32) {
    #[cfg(target_os = "linux")]
    {
        const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
        // SAFETY: plain syscall on integers.
        let done = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                first as libc::c_uint,
                libc::c_uint::MAX,
                CLOSE_RANGE_CLOEXEC,
            )
        } == 0;
        if done {
            return;
        }
    }
    // SAFETY: getrlimit/fcntl on integers.
    let max = unsafe {
        let mut limit = std::mem::zeroed::<libc::rlimit>();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 {
            limit.rlim_cur.min(65_536) as i32
        } else {
            4096
        }
    };
    for fd in first..max {
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
}

/// Inputs that do not depend on the backend.
#[derive(Debug, Clone)]
pub struct Setup {
    pub policy: Policy,
    /// Curated environment, before the backend sets `TMPDIR`.
    pub env: Vec<(OsString, OsString)>,
}

/// An established sandbox. Once a probe fails it stays disabled.
pub struct Sandbox {
    backend: Backend,
    setup: Setup,
    disabled: Mutex<Option<String>>,
}

impl fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sandbox")
            .field("backend", &self.backend)
            .field("policy", &self.setup.policy)
            .finish_non_exhaustive()
    }
}

impl Sandbox {
    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    pub fn policy(&self) -> &Policy {
        &self.setup.policy
    }

    /// The scratch directory as commands inside the sandbox see it.
    pub fn visible_scratch(&self) -> PathBuf {
        match self.backend {
            Backend::Bwrap { .. } => PathBuf::from(bwrap::SANDBOX_TMP),
            _ => self.setup.policy.scratch.clone(),
        }
    }

    /// Wraps `/bin/sh -c script` run in `cwd`.
    pub fn invocation(&self, script: &str, cwd: &Path) -> Invocation {
        invocation(&self.backend, &self.setup, script, cwd)
    }

    /// Why the shell is disabled, if it is.
    pub fn disabled(&self) -> Option<String> {
        self.disabled.lock().unwrap().clone()
    }

    /// Re-checks that the sandbox still starts, disabling it for good if not.
    pub async fn check(&self) -> Result<(), String> {
        if let Some(reason) = self.disabled() {
            return Err(reason);
        }
        if let Err(reason) = quick_probe(&self.backend, &self.setup).await {
            let reason = format!("{} sandbox stopped working: {reason}", self.backend.name());
            *self.disabled.lock().unwrap() = Some(reason.clone());
            return Err(reason);
        }
        Ok(())
    }

    /// Picks and verifies a backend. The error explains why none could be
    /// established, per candidate.
    pub async fn establish(
        choice: BackendChoice,
        setup: Setup,
        launcher: Option<PathBuf>,
    ) -> Result<Self, String> {
        let mut failures = Vec::new();
        let path = setup
            .env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone());
        for candidate in candidates(choice, launcher, path)? {
            let backend = match candidate {
                Ok(backend) => backend,
                Err(reason) => {
                    failures.push(reason);
                    continue;
                }
            };
            match full_probe(&backend, &setup).await {
                Ok(()) => {
                    return Ok(Self {
                        backend,
                        setup,
                        disabled: Mutex::new(None),
                    });
                }
                Err(reason) => failures.push(format!("{}: {reason}", describe(&backend))),
            }
        }
        Err(failures.join("; "))
    }
}

fn describe(backend: &Backend) -> String {
    match backend {
        Backend::Bwrap {
            proc: ProcMount::HostReadOnly,
            ..
        } => "bwrap (host /proc)".into(),
        other => other.name().into(),
    }
}

fn candidates(
    choice: BackendChoice,
    launcher: Option<PathBuf>,
    path: Option<OsString>,
) -> Result<Vec<Result<Backend, String>>, String> {
    let linux = cfg!(target_os = "linux");
    let macos = cfg!(target_os = "macos");
    let bwrap = || match find_program("bwrap", path.as_deref()) {
        Some(program) => vec![
            Ok(Backend::Bwrap {
                program: program.clone(),
                proc: ProcMount::Fresh,
            }),
            Ok(Backend::Bwrap {
                program,
                proc: ProcMount::HostReadOnly,
            }),
        ],
        None => vec![Err("bwrap: not found on PATH (install bubblewrap)".into())],
    };
    let landlock = || match &launcher {
        Some(launcher) => Ok(Backend::Landlock {
            launcher: launcher.clone(),
        }),
        None => Err("landlock: cannot locate the saddle executable".into()),
    };
    let seatbelt = || {
        if Path::new(seatbelt::SANDBOX_EXEC).is_file() {
            Ok(Backend::Seatbelt)
        } else {
            Err(format!("seatbelt: {} not found", seatbelt::SANDBOX_EXEC))
        }
    };
    let unsupported = |name: &str| Err(format!("the {name} backend is not available on this OS"));
    Ok(match choice {
        BackendChoice::Auto if linux => {
            let mut list = bwrap();
            list.push(landlock());
            list
        }
        BackendChoice::Auto if macos => vec![seatbelt()],
        BackendChoice::Auto => return Err("no sandbox backend exists for this OS".into()),
        BackendChoice::Bwrap if linux => bwrap(),
        BackendChoice::Landlock if linux => vec![landlock()],
        BackendChoice::Seatbelt if macos => vec![seatbelt()],
        BackendChoice::Bwrap => return unsupported("bwrap"),
        BackendChoice::Landlock => return unsupported("landlock"),
        BackendChoice::Seatbelt => return unsupported("seatbelt"),
    })
}

/// Looks `name` up on the sandbox's own `PATH`.
fn find_program(name: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

pub(crate) fn invocation(backend: &Backend, setup: &Setup, script: &str, cwd: &Path) -> Invocation {
    let mut env = setup.env.clone();
    let tmp: OsString = match backend {
        Backend::Bwrap { .. } => bwrap::SANDBOX_TMP.into(),
        _ => setup.policy.scratch.clone().into(),
    };
    for key in ["TMPDIR", "TMP", "TEMP"] {
        env.push((key.into(), tmp.clone()));
    }
    env.push(("SADDLE_SANDBOX".into(), backend.name().into()));
    let shell = ["/bin/sh", "-c", script].map(OsString::from);
    let (program, args) = match backend {
        Backend::Bwrap { program, proc } => (
            program.clone(),
            bwrap::args(&setup.policy, *proc, cwd, &shell),
        ),
        Backend::Landlock { launcher } => (launcher.clone(), landlock_args(&setup.policy, &shell)),
        Backend::Seatbelt => (
            PathBuf::from(seatbelt::SANDBOX_EXEC),
            seatbelt::args(&setup.policy, &shell),
        ),
    };
    Invocation {
        program,
        args,
        env,
        cwd: cwd.to_owned(),
    }
}

/// The argument that switches the Saddle binary into its Landlock helper mode.
pub const LANDLOCK_HELPER_ARG: &str = "__landlock-exec";

/// `saddle __landlock-exec <n> <writable>... <program> <args>...`
fn landlock_args(policy: &Policy, command: &[OsString]) -> Vec<OsString> {
    let writable = landlock_writable(policy);
    let mut args = vec![
        OsString::from(LANDLOCK_HELPER_ARG),
        writable.len().to_string().into(),
    ];
    args.extend(writable.into_iter().map(OsString::from));
    args.extend(command.iter().cloned());
    args
}

/// Writable paths under Landlock: carve-outs, scratch and the harmless devices.
fn landlock_writable(policy: &Policy) -> Vec<PathBuf> {
    let mut paths = vec![policy.scratch.clone()];
    paths.extend(policy.writable.iter().cloned());
    paths.extend(
        ["/dev/null", "/dev/zero", "/dev/full"]
            .iter()
            .map(PathBuf::from),
    );
    paths
}

const PROBE_OK: &str = "saddle-sandbox-ok";

async fn quick_probe(backend: &Backend, setup: &Setup) -> Result<(), String> {
    let root = setup.policy.project_root.clone();
    run_probe(backend, setup, &format!("echo {PROBE_OK}"), &root).await
}

/// Starts a command that must fail to create a directory in the project and
/// succeed in writing to `TMPDIR`. If the project turns out to be writable the
/// stray directory is removed and the backend rejected.
async fn full_probe(backend: &Backend, setup: &Setup) -> Result<(), String> {
    let root = &setup.policy.project_root;
    let probe = root.join(format!(".saddle-probe-{}", std::process::id()));
    let script = format!(
        "if mkdir {probe} 2>/dev/null; then echo project-writable; exit 3; fi; \
         echo probe > \"$TMPDIR/.saddle-probe\" && rm \"$TMPDIR/.saddle-probe\" || exit 4; echo {PROBE_OK}",
        probe = shell_quote(&probe.to_string_lossy()),
    );
    let result = run_probe(backend, setup, &script, root).await;
    if probe.exists() {
        let _ = std::fs::remove_dir(&probe);
        return Err("the project was writable inside the sandbox".into());
    }
    result
}

async fn run_probe(
    backend: &Backend,
    setup: &Setup,
    script: &str,
    cwd: &Path,
) -> Result<(), String> {
    let mut command = invocation(backend, setup, script, cwd).command();
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), command.output())
        .await
        .map_err(|_| "the probe timed out".to_string())?
        .map_err(|e| format!("cannot start it: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if output.status.success() && stdout.trim_end().ends_with(PROBE_OK) {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no output")
        .to_owned();
    Err(match output.status.code() {
        Some(3) => "the project was writable inside the sandbox".into(),
        Some(4) => "TMPDIR was not writable inside the sandbox".into(),
        _ => format!("{detail} ({})", output.status),
    })
}

/// Quotes `s` as a single POSIX shell word.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn policy() -> Policy {
        Policy {
            project_root: "/src/proj".into(),
            writable: vec!["/src/proj/target".into(), "/home/u/.cargo".into()],
            scratch: "/tmp/saddle-x".into(),
            hidden: vec!["/home/u/.config/saddle/credentials.toml".into()],
        }
    }

    fn setup() -> Setup {
        Setup {
            policy: policy(),
            env: vec![("PATH".into(), "/usr/bin".into())],
        }
    }

    #[test]
    fn carve_outs_are_validated() {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let root = base.join("proj");
        let cache = base.join("cache");
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let scratch = base.join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        std::os::unix::fs::symlink(&root, base.join("alias")).unwrap();
        let secret = base.join("credentials.toml");
        std::fs::write(&secret, "x").unwrap();

        let requested = [
            root.join("target"),
            root.join("target"),
            cache.clone(),
            base.join("missing"),
            base.clone(),
            base.join("alias"),
            root.join(".git/objects"),
            root.join("sub/../build"),
        ];
        let (policy, notes) = Policy::new(
            &root,
            &requested,
            &scratch,
            &[secret.clone(), base.join("nope")],
        )
        .unwrap();
        assert_eq!(
            policy.writable,
            [root.join("target"), cache, root.join("build")]
        );
        assert!(root.join("target").is_dir());
        assert!(!base.join("missing").exists());
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert!(notes[0].contains("contains the project"));
        assert!(notes[1].contains("contains the project"));
        assert!(notes[2].contains(".git"));
        assert_eq!(policy.hidden, [secret]);
    }

    #[test]
    fn landlock_invocation() {
        let backend = Backend::Landlock {
            launcher: "/bin/saddle".into(),
        };
        let inv = invocation(&backend, &setup(), "ls", Path::new("/src/proj/src"));
        assert_eq!(inv.program, PathBuf::from("/bin/saddle"));
        let args: Vec<_> = inv.args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                LANDLOCK_HELPER_ARG,
                "6",
                "/tmp/saddle-x",
                "/src/proj/target",
                "/home/u/.cargo",
                "/dev/null",
                "/dev/zero",
                "/dev/full",
                "/bin/sh",
                "-c",
                "ls"
            ]
        );
        assert_eq!(inv.cwd, PathBuf::from("/src/proj/src"));
        assert!(inv.env.contains(&("TMPDIR".into(), "/tmp/saddle-x".into())));
        assert!(
            inv.env
                .contains(&("SADDLE_SANDBOX".into(), "landlock".into()))
        );
    }

    #[test]
    fn bwrap_tmpdir_is_the_mounted_scratch() {
        let backend = Backend::Bwrap {
            program: "/usr/bin/bwrap".into(),
            proc: ProcMount::Fresh,
        };
        let inv = invocation(&backend, &setup(), "ls", Path::new("/src/proj"));
        assert!(inv.env.contains(&("TMPDIR".into(), "/tmp".into())));
        assert_eq!(inv.program, PathBuf::from("/usr/bin/bwrap"));
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
