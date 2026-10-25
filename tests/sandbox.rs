//! The sandbox against the real OS (Linux): every backend that can be
//! established here must keep the project read-only. Backends the host cannot
//! provide are skipped with a message; `SADDLE_REQUIRE_SANDBOX=bwrap,landlock`
//! turns a skip into a failure.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use saddle::jobs::{Jobs, WaitEnd};
use saddle::sandbox::{self, BackendChoice, Policy, Sandbox, Setup};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    cache: PathBuf,
    secret: PathBuf,
    jobs: Jobs,
}

async fn env(choice: BackendChoice) -> Option<Env> {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let root = base.join("project");
    let cache = base.join("cache");
    let scratch = base.join("scratch");
    for dir in [&root.join("src"), &cache, &scratch] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"p\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    let secret = base.join("credentials.toml");
    std::fs::write(&secret, "api_key = \"sk-secret\"\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "."]);
    git(
        &root,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ],
    );

    let (policy, notes) = Policy::new(
        &root,
        &[root.join("target"), cache.clone()],
        &scratch,
        std::slice::from_ref(&secret),
    )
    .unwrap();
    assert!(notes.is_empty(), "{notes:?}");
    let setup = Setup {
        policy,
        env: sandbox::environment(std::env::vars_os(), &[]),
    };
    let launcher = PathBuf::from(env!("CARGO_BIN_EXE_saddle"));
    match Sandbox::establish(choice, setup, Some(launcher)).await {
        Ok(sandbox) => {
            let (tx, _rx) = mpsc::unbounded_channel();
            Some(Env {
                _tmp: tmp,
                root,
                cache,
                secret,
                jobs: Jobs::new(Arc::new(sandbox), tx),
            })
        }
        Err(reason) => {
            let name = format!("{choice:?}").to_lowercase();
            let required = std::env::var("SADDLE_REQUIRE_SANDBOX").unwrap_or_default();
            assert!(
                !required.split(',').any(|r| r == name),
                "{name} is required but unavailable: {reason}"
            );
            eprintln!("skipping {name}: {reason}");
            None
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success());
}

impl Env {
    /// Runs `script` to completion; returns (exited successfully, output).
    async fn run(&self, script: &str) -> (bool, String) {
        let id = self
            .jobs
            .start(script, self.root.clone(), ".".into())
            .await
            .unwrap();
        let end = self
            .jobs
            .wait(id, Duration::from_secs(120), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(end, WaitEnd::Exited);
        let report = self.jobs.report(id).unwrap();
        (report.contains("exited with status 0"), report)
    }

    async fn fails(&self, script: &str) -> String {
        let (ok, report) = self.run(script).await;
        assert!(!ok, "{script} should fail:\n{report}");
        report
    }

    async fn succeeds(&self, script: &str) -> String {
        let (ok, report) = self.run(script).await;
        assert!(ok, "{script} should succeed:\n{report}");
        report
    }
}

const BACKENDS: [BackendChoice; 2] = [BackendChoice::Bwrap, BackendChoice::Landlock];

#[tokio::test]
async fn project_is_read_only() {
    for choice in BACKENDS {
        let Some(env) = env(choice).await else {
            continue;
        };
        let root = env.root.display().to_string();
        env.fails("touch new-file").await;
        env.fails("echo x >> src/main.rs").await;
        env.fails("rm src/main.rs").await;
        env.fails("mv src/main.rs src/moved.rs").await;
        env.fails("mkdir src/sub").await;
        env.fails("truncate -s 0 src/main.rs").await;
        if choice == BackendChoice::Bwrap {
            // Landlock has no right covering metadata changes (documented gap).
            env.fails("chmod 600 src/main.rs").await;
        }
        env.fails("ln -s x src/link").await;
        env.fails("touch .git/x").await;
        env.fails("git -c user.name=t -c user.email=t@t commit -q --allow-empty -m x")
            .await;
        env.fails("echo change >> Cargo.toml; git stash").await;
        // A writable alias through a hard link or symlink in a writable place.
        env.fails("ln src/main.rs \"$TMPDIR/hard\" && echo x >> \"$TMPDIR/hard\"")
            .await;
        env.fails(&format!(
            "ln -s {root}/src \"$TMPDIR/alias\" && touch \"$TMPDIR/alias/x\""
        ))
        .await;
        env.fails("ln -s ../src target/alias && touch target/alias/x")
            .await;
        // Writes from an inherited descriptor: nothing above stderr leaks in.
        env.fails("ls /proc/self/fd | grep -qvE '^[0-3]$'").await;

        assert_eq!(
            std::fs::read_to_string(env.root.join("src/main.rs")).unwrap(),
            "fn main() {}\n"
        );
        assert!(!env.root.join("new-file").exists());
        assert!(!env.root.join("src/x").exists());
        env.succeeds("git status --short && git log --oneline")
            .await;
    }
}

#[tokio::test]
async fn carve_outs_and_scratch_are_writable() {
    for choice in BACKENDS {
        let Some(env) = env(choice).await else {
            continue;
        };
        let cache = env.cache.display().to_string();
        env.succeeds("mkdir -p target/debug && echo built > target/debug/out")
            .await;
        env.succeeds(&format!("echo cached > {cache}/entry")).await;
        env.succeeds("echo tmp > \"$TMPDIR/f\" && cat \"$TMPDIR/f\" && echo x > /dev/null")
            .await;
        assert_eq!(
            std::fs::read_to_string(env.root.join("target/debug/out")).unwrap(),
            "built\n"
        );
        assert_eq!(
            std::fs::read_to_string(env.cache.join("entry")).unwrap(),
            "cached\n"
        );
        // TMPDIR survives between commands of a session.
        env.succeeds("test -f \"$TMPDIR/f\"").await;
    }
}

#[tokio::test]
async fn secrets_stay_out() {
    for choice in BACKENDS {
        let Some(env) = env(choice).await else {
            continue;
        };
        let report = env.succeeds("env").await;
        assert!(report.contains("SADDLE_SANDBOX="), "{report}");
        assert!(report.contains("TERM=dumb"), "{report}");
        if choice == BackendChoice::Bwrap {
            // Landlock cannot hide single files from reads (documented gap).
            let (_, report) = env.run(&format!("cat {}", env.secret.display())).await;
            assert!(!report.contains("sk-secret"), "{report}");
        }
    }
}

#[tokio::test]
async fn build_script_writing_into_src_fails_but_target_works() {
    if std::process::Command::new("cargo")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: cargo not on PATH");
        return;
    }
    for choice in BACKENDS {
        let Some(env) = env(choice).await else {
            continue;
        };
        let lock = std::process::Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(&env.root)
            .status()
            .unwrap();
        assert!(lock.success());
        std::fs::write(
            env.root.join("build.rs"),
            "fn main() { std::fs::write(\"src/generated.rs\", \"\").expect(\"write into src\"); }\n",
        )
        .unwrap();
        let report = env.fails("cargo build --offline 2>&1").await;
        assert!(report.contains("write into src"), "{report}");
        assert!(!env.root.join("src/generated.rs").exists());

        std::fs::write(
            env.root.join("build.rs"),
            "fn main() { let out = std::env::var(\"OUT_DIR\").unwrap(); std::fs::write(format!(\"{out}/gen.rs\"), \"\").unwrap(); }\n",
        )
        .unwrap();
        env.succeeds("cargo build --offline 2>&1").await;
        assert!(env.root.join("target/debug/p").exists());
    }
}

#[tokio::test]
async fn long_job_survives_interrupted_wait() {
    for choice in BACKENDS {
        let Some(env) = env(choice).await else {
            continue;
        };
        let id = env
            .jobs
            .start(
                "echo first; sleep 1; echo second",
                env.root.clone(),
                ".".into(),
            )
            .await
            .unwrap();
        let interrupt = CancellationToken::new();
        let trigger = interrupt.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            trigger.cancel();
        });
        let end = env
            .jobs
            .wait(id, Duration::from_secs(30), &interrupt)
            .await
            .unwrap();
        assert_eq!(end, WaitEnd::Interrupted);
        let report = env.jobs.report(id).unwrap();
        assert!(report.contains("running"), "{report}");
        assert!(report.contains("first"), "{report}");

        let end = env
            .jobs
            .wait(id, Duration::from_secs(30), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(end, WaitEnd::Exited);
        let report = env.jobs.report(id).unwrap();
        assert!(report.contains("exited with status 0"), "{report}");
        assert!(
            report.contains("second") && !report.contains("first"),
            "{report}"
        );
    }
}

#[tokio::test]
async fn kill_stops_the_whole_group() {
    for choice in BACKENDS {
        let Some(env) = env(choice).await else {
            continue;
        };
        let marker = env.cache.join("survivor");
        let script = format!("(sleep 3; touch {}) & sleep 30", marker.display());
        let id = env
            .jobs
            .start(&script, env.root.clone(), ".".into())
            .await
            .unwrap();
        assert!(env.jobs.kill(id).unwrap());
        let end = env
            .jobs
            .wait(id, Duration::from_secs(10), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(end, WaitEnd::Exited);
        assert!(env.jobs.report(id).unwrap().contains("killed"));
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(!marker.exists(), "a background child outlived the kill");
    }
}

#[tokio::test]
async fn missing_bwrap_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (policy, _) = Policy::new(tmp.path(), &[], scratch.path(), &[]).unwrap();
    let setup = Setup {
        policy,
        env: vec![("PATH".into(), "/nonexistent".into())],
    };
    let err = Sandbox::establish(BackendChoice::Bwrap, setup, None)
        .await
        .unwrap_err();
    assert!(err.contains("bwrap: not found on PATH"), "{err}");
}

#[tokio::test]
async fn auto_falls_back_to_landlock_without_bwrap() {
    let tmp = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let (policy, _) = Policy::new(tmp.path(), &[], scratch.path(), &[]).unwrap();
    let setup = Setup {
        policy,
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
    };
    let launcher = PathBuf::from(env!("CARGO_BIN_EXE_saddle"));
    match Sandbox::establish(BackendChoice::Auto, setup, Some(launcher)).await {
        Ok(sandbox) => assert_eq!(sandbox.backend().name(), "landlock"),
        Err(err) => {
            assert!(err.contains("bwrap: not found on PATH"), "{err}");
            assert!(err.contains("landlock"), "{err}");
        }
    }
}

#[tokio::test]
async fn shell_tools_through_the_registry() {
    use saddle::tools::{ProjectFs, Registry, shell};
    use serde_json::json;

    let Some(env) = env(BackendChoice::Auto).await else {
        return;
    };
    let mut tools = Registry::default();
    shell::install(
        &mut tools,
        env.jobs.clone(),
        ProjectFs::new(&env.root).unwrap(),
    );

    let quick = tools
        .call(
            "shell_start",
            json!({"command": "echo hi; pwd", "cwd": "src"}),
        )
        .await;
    assert!(!quick.is_error, "{}", quick.content);
    assert!(
        quick.content.starts_with("job 1 exited with status 0"),
        "{}",
        quick.content
    );
    assert!(quick.content.contains("hi\n") && quick.content.contains("/project/src"));

    let escape = tools
        .call("shell_start", json!({"command": "true", "cwd": ".."}))
        .await;
    assert!(
        escape.is_error && escape.content.contains("outside"),
        "{}",
        escape.content
    );

    let slow = tools
        .call("shell_start", json!({"command": "echo started; sleep 5"}))
        .await;
    assert!(slow.content.contains("still running"), "{}", slow.content);
    assert!(slow.content.contains("started"), "{}", slow.content);

    let interrupt = CancellationToken::new();
    let call = tools.start("shell_wait", json!({"job_id": 2}), Some(interrupt.clone()));
    assert!(call.interruptible);
    interrupt.cancel();
    let waited = call.future.await;
    assert!(
        waited
            .content
            .starts_with("Wait interrupted; the job keeps running."),
        "{}",
        waited.content
    );

    let list = tools.call("shell_list", json!({})).await;
    assert!(list.content.contains("job 2 running"), "{}", list.content);
    let killed = tools.call("shell_kill", json!({"job_id": 2})).await;
    assert!(
        killed.content.starts_with("job 2 killed"),
        "{}",
        killed.content
    );
    let missing = tools.call("shell_output", json!({"job_id": 9})).await;
    assert!(missing.is_error);
}
