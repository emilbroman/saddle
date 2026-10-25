use super::*;
use crate::watch::{ChangeKind, Detail};
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

struct TestRepo(TempDir);
impl TestRepo {
    fn new() -> Self {
        let repo = Self(tempfile::tempdir().unwrap());
        repo.git(&["init", "-b", "main"]);
        repo.git(&["config", "user.name", "Test Author"]);
        repo.git(&["config", "user.email", "test@example.com"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn git(&self, args: &[&str]) -> String {
        let result = Command::new("git")
            .arg("-C")
            .arg(self.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8_lossy(&result.stdout).trim().into()
    }
    fn commit(&self, name: &str, content: &str, subject: &str) -> String {
        fs::write(self.path().join(name), content).unwrap();
        self.git(&["add", "--", name]);
        self.git(&["commit", "-m", subject]);
        self.git(&["rev-parse", "HEAD"])
    }
    fn tracker(&self) -> Tracker {
        let tracker = Tracker::new(self.path().into(), None, GitState::default());
        tracker.classify(&mut vec![], None, true).unwrap().unwrap();
        tracker
    }
    fn change(&self, name: &str) -> FileChange {
        FileChange {
            path: self.path().join(name),
            display: name.into(),
            kind: ChangeKind::Modified,
            detail: Detail::Unseen { size: 0 },
        }
    }
}
fn classify(tracker: &Tracker) -> Option<GitEvent> {
    tracker.classify(&mut vec![], None, false).unwrap()
}

#[test]
fn first_launch_only_tip_and_reset() {
    let repo = TestRepo::new();
    repo.commit("a", "1", "old commit");
    repo.commit("a", "2", "latest commit");
    let tracker = Tracker::new(repo.path().into(), None, GitState::default());
    let event = tracker.classify(&mut vec![], None, true).unwrap().unwrap();
    assert!(
        event
            .text
            .contains("First launch in this worktree: main at")
    );
    assert!(event.text.contains("latest commit"));
    assert!(!event.text.contains("old commit"));
    assert!(classify(&tracker).is_none());
    tracker.reset();
    assert_eq!(tracker.state(), GitState::default());
    assert!(tracker.classify(&mut vec![], None, true).unwrap().is_some());
}

#[test]
fn no_repository_and_unborn_then_first_commit() {
    let dir = tempfile::tempdir().unwrap();
    let tracker = Tracker::new(dir.path().into(), None, GitState::default());
    assert!(
        tracker
            .classify(&mut vec![], None, true)
            .unwrap()
            .unwrap()
            .text
            .contains("no repository")
    );
    assert!(classify(&tracker).is_none());
    let repo = TestRepo::new();
    let tracker = repo.tracker();
    assert_eq!(tracker.state().branch.as_deref(), Some("main"));
    assert!(tracker.state().head.is_none());
    repo.commit("a", "first", "initial commit");
    assert!(classify(&tracker).unwrap().text.contains("initial commit"));
}

#[test]
fn offline_commits_are_unseen_and_fast_forward_folds_clean_files_only() {
    let repo = TestRepo::new();
    repo.commit("a", "1", "base");
    let tracker = repo.tracker();
    repo.git(&["checkout", "-b", "incoming"]);
    repo.commit("a", "2", "pulled commit");
    repo.git(&["checkout", "main"]);
    repo.git(&["merge", "--ff-only", "incoming"]);
    fs::write(repo.path().join("dirty"), "user edit").unwrap();
    let mut files = vec![repo.change("a"), repo.change("dirty")];
    let event = tracker.classify(&mut files, None, false).unwrap().unwrap();
    assert!(event.text.contains("Unseen commits:"));
    assert!(event.text.contains("Test Author  pulled commit"));
    assert!(event.text.contains("Aggregate changes:"));
    assert!(!event.text.contains("committed "));
    assert_eq!(files, vec![repo.change("dirty")]);
    let resumed = Tracker::new(repo.path().into(), None, tracker.state());
    assert!(classify(&resumed).is_none());
}

#[test]
fn own_commit_seconds_boundary_and_committer_email() {
    let repo = TestRepo::new();
    repo.commit("a", "1", "base");
    let tracker = repo.tracker();
    repo.commit("a", "2", "my change");
    let seconds: u64 = repo
        .git(&["show", "-s", "--format=%ct", "HEAD"])
        .parse()
        .unwrap();
    let start = UNIX_EPOCH + Duration::from_secs(seconds) + Duration::from_millis(900);
    let event = tracker
        .classify(&mut vec![], Some((start, start)), false)
        .unwrap()
        .unwrap();
    assert!(event.text.contains("committed "));
    assert!(event.text.contains("my change, 1 files"));
    assert!(!event.text.contains("Unseen commits"));
    assert!(!event.text.contains("Aggregate changes"));
    repo.git(&["config", "user.email", "other@example.com"]);
    repo.commit("a", "3", "someone else");
    repo.git(&["config", "user.email", "test@example.com"]);
    let event = tracker
        .classify(&mut vec![], Some((UNIX_EPOCH, SystemTime::now())), false)
        .unwrap()
        .unwrap();
    assert!(event.text.contains("Unseen commits"));
}

#[test]
fn amend_and_reset_report_replaced_counts() {
    let repo = TestRepo::new();
    let base = repo.commit("a", "1", "base");
    repo.commit("a", "2", "old subject");
    let tracker = repo.tracker();
    repo.git(&["commit", "--amend", "-m", "new subject"]);
    let event = classify(&tracker).unwrap();
    assert!(
        event
            .text
            .contains("History rewritten: 1 commits replaced by 1")
    );
    assert!(event.text.contains("new subject"));
    repo.git(&["reset", "--hard", &base]);
    let event = classify(&tracker).unwrap();
    assert!(
        event
            .text
            .contains("History rewritten: 1 commits replaced by 0")
    );
    assert!(classify(&tracker).is_none());
}

#[test]
fn branch_first_visit_mergebase_and_per_branch_resume() {
    let repo = TestRepo::new();
    repo.commit("a", "1", "base not unseen");
    let tracker = repo.tracker();
    repo.git(&["checkout", "-b", "feature"]);
    let tip = repo.commit("a", "2", "feature change");
    let event = classify(&tracker).unwrap();
    assert!(event.text.contains("main → feature"));
    assert!(event.text.contains("feature change"));
    assert!(!event.text.contains("base not unseen"));
    repo.git(&["checkout", "main"]);
    assert!(classify(&tracker).unwrap().text.contains("feature → main"));
    repo.git(&["checkout", "feature"]);
    let event = classify(&tracker).unwrap();
    assert!(!event.text.contains("Unseen commits"));
    assert_eq!(tracker.state().per_branch.get("feature"), Some(&tip));
    repo.git(&["checkout", "main"]);
    classify(&tracker);
    repo.git(&["checkout", "feature"]);
    repo.commit("a", "3", "offline feature change");
    assert!(
        classify(&tracker)
            .unwrap()
            .text
            .contains("offline feature change")
    );
}

#[test]
fn default_branch_override_and_origin_head() {
    let repo = TestRepo::new();
    repo.commit("a", "1", "main base");
    repo.git(&["checkout", "-b", "develop"]);
    let develop = repo.commit("a", "2", "develop base");
    repo.git(&["checkout", "-b", "feature"]);
    repo.commit("a", "3", "feature tip");
    let state = GitState {
        branch: Some("main".into()),
        ..Default::default()
    };
    let tracker = Tracker::new(repo.path().into(), Some("develop".into()), state.clone());
    let event = classify(&tracker).unwrap();
    assert!(event.text.contains("feature tip"));
    assert!(!event.text.contains("develop base"));
    repo.git(&["update-ref", "refs/remotes/origin/develop", &develop]);
    repo.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/develop",
    ]);
    let tracker = Tracker::new(repo.path().into(), None, state);
    let event = classify(&tracker).unwrap();
    assert!(!event.text.contains("develop base"));
}

#[test]
fn detached_head_uses_hashes() {
    let repo = TestRepo::new();
    let base = repo.commit("a", "1", "base");
    let tracker = repo.tracker();
    repo.git(&["checkout", "--detach", &base]);
    let event = classify(&tracker).unwrap();
    assert!(
        event
            .text
            .contains(&format!("main → detached {}", &base[..12]))
    );
    assert!(tracker.state().branch.is_none());
    let tip = repo.commit("a", "2", "detached work");
    let event = classify(&tracker).unwrap();
    assert!(event.text.contains("detached work"));
    assert_eq!(tracker.state().head.as_ref(), Some(&tip));
}

#[test]
fn stash_checkout_and_reset_reverts_fold_deleted_files() {
    let repo = TestRepo::new();
    repo.commit("a", "base", "base");
    let tracker = repo.tracker();
    for operation in [
        vec!["checkout", "--", "a"],
        vec!["stash", "push"],
        vec!["reset", "--hard"],
    ] {
        fs::write(repo.path().join("a"), "dirty").unwrap();
        repo.git(&operation);
        let mut files = vec![repo.change("a")];
        let event = tracker.classify(&mut files, None, false).unwrap().unwrap();
        assert!(
            event
                .text
                .contains("Working tree reverted to HEAD for 1 files")
        );
        assert!(files.is_empty());
    }
    let mut files = vec![repo.change("missing")];
    let event = tracker.classify(&mut files, None, false).unwrap().unwrap();
    assert!(event.text.contains("missing"));
    assert!(files.is_empty());
}

#[test]
fn rebase_reports_rewritten_history() {
    let repo = TestRepo::new();
    repo.commit("a", "1", "base");
    repo.git(&["checkout", "-b", "feature"]);
    repo.commit("feature", "feature", "feature commit");
    let tracker = repo.tracker();
    repo.git(&["checkout", "main"]);
    repo.commit("main", "main", "main commit");
    repo.git(&["checkout", "feature"]);
    repo.git(&["rebase", "main"]);
    let event = classify(&tracker).unwrap();
    assert!(
        event
            .text
            .contains("History rewritten: 1 commits replaced by 2")
    );
    assert!(event.text.contains("feature commit"));
    assert!(event.text.contains("main commit"));
}

#[test]
fn commit_and_stat_lists_are_bounded() {
    let repo = TestRepo::new();
    repo.commit("a", "base", "base");
    let tracker = repo.tracker();
    for n in 0..53 {
        repo.commit(&format!("file-{n}"), "content", &format!("commit-{n}"));
    }
    let event = classify(&tracker).unwrap();
    assert!(event.text.contains("and 3 more commits"));
    assert_eq!(
        event
            .text
            .lines()
            .filter(|line| line.contains("  Test Author  "))
            .count(),
        50
    );
    assert!(event.text.len() < 15000);
}

#[test]
fn linked_worktree_metadata_has_private_and_common_dirs() {
    let repo = TestRepo::new();
    repo.commit("a", "1", "base");
    let other = tempfile::tempdir().unwrap();
    let path = other.path().join("linked");
    repo.git(&["worktree", "add", "-b", "linked", path.to_str().unwrap()]);
    let dirs = metadata_dirs(&path);
    assert_eq!(dirs.len(), 2);
    assert!(
        dirs.iter()
            .any(|dir| dir.join("HEAD").is_file() && dir.ends_with("linked"))
    );
    assert!(dirs.contains(&repo.path().join(".git").canonicalize().unwrap()));
    assert_eq!(metadata_dirs(repo.path()).len(), 1);
    assert!(metadata_dirs(other.path()).is_empty());
}

#[test]
fn classification_does_not_run_external_helpers_or_parse_option_refs() {
    use std::os::unix::fs::PermissionsExt;

    let repo = TestRepo::new();
    repo.commit(".gitattributes", "a diff=custom\n", "attributes");
    repo.commit("a", "base", "base");
    let tracker = repo.tracker();
    repo.commit("a", "new", "new commit");
    let script = repo.path().join("helper");
    fs::write(&script, "#!/bin/sh\ntouch helper-ran\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    for setting in ["diff.external", "diff.custom.textconv", "core.fsmonitor"] {
        repo.git(&["config", setting, script.to_str().unwrap()]);
    }
    assert!(classify(&tracker).unwrap().text.contains("new commit"));
    assert!(!repo.path().join("helper-ran").exists());
    let tracker = Tracker::new(
        repo.path().into(),
        Some("--output=unexpected".into()),
        GitState::default(),
    );
    assert!(classify(&tracker).is_some());
    assert!(!repo.path().join("unexpected").exists());
}
