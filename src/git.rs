//! Read-only classification of worktree and ref movements.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::watch::{FileChange, state::equals_head};

const COMMIT_LIMIT: usize = 50;
const STAT_LIMIT: usize = 8192;
const DETACHED: &str = "(detached)";

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitState {
    pub per_branch: BTreeMap<String, String>,
    pub branch: Option<String>,
    pub head: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitEvent {
    pub headline: String,
    pub text: String,
}

#[derive(Clone)]
pub struct Tracker(Arc<Mutex<Inner>>);

struct Inner {
    root: PathBuf,
    default_branch: Option<String>,
    state: GitState,
}

impl Tracker {
    pub fn new(root: PathBuf, default_branch: Option<String>, state: GitState) -> Self {
        Self(Arc::new(Mutex::new(Inner {
            root,
            default_branch,
            state,
        })))
    }

    pub fn state(&self) -> GitState {
        self.0.lock().unwrap().state.clone()
    }

    pub fn reset(&self) {
        self.0.lock().unwrap().state = GitState::default();
    }

    pub fn classify(
        &self,
        files: &mut Vec<FileChange>,
        window: Option<(SystemTime, SystemTime)>,
        first: bool,
    ) -> Result<Option<GitEvent>> {
        let mut inner = self.0.lock().unwrap();
        let repo = Repo(&inner.root);
        if repo
            .optional(&["rev-parse", "--is-inside-work-tree"])?
            .as_deref()
            != Some("true")
        {
            inner.state = GitState::default();
            return Ok(first.then(|| {
                event(
                    "First launch",
                    "First launch in this worktree; no repository.".into(),
                )
            }));
        }
        let branch = repo.optional(&["symbolic-ref", "--quiet", "--short", "HEAD"])?;
        let head = repo.resolve("HEAD")?;
        let key = branch.as_deref().unwrap_or(DETACHED);
        let old = &inner.state;
        let switched = old.branch != branch;
        let moved = old.head != head;
        let mut lines = Vec::new();
        let mut headline = String::new();

        if first {
            headline = "First launch".into();
            let tip = match &head {
                Some(head) => repo.required(&[
                    "show",
                    "--no-patch",
                    "--no-show-signature",
                    "--format=%h %s",
                    head,
                    "--",
                ])?,
                None => "unborn HEAD (no commits)".into(),
            };
            lines.push(format!(
                "First launch in this worktree: {} at {}.",
                branch.as_deref().unwrap_or("detached HEAD"),
                tip
            ));
        } else {
            if switched {
                headline = "Branch switch".into();
                lines.push(format!(
                    "Branch switch: {} → {}",
                    location(old.branch.as_deref(), old.head.as_deref()),
                    location(branch.as_deref(), head.as_deref())
                ));
            } else if branch.is_none() && moved {
                headline = "Detached HEAD moved".into();
                lines.push(format!(
                    "Detached HEAD: {} → {}",
                    short(old.head.as_deref()),
                    short(head.as_deref())
                ));
            }
            if let Some(head) = &head {
                let previous = old
                    .per_branch
                    .get(key)
                    .or_else(|| (!switched).then_some(old.head.as_ref()).flatten());
                let previous = match previous {
                    Some(previous) => repo.resolve(previous)?,
                    None => None,
                };
                let base = match &previous {
                    Some(previous) => Some(previous.clone()),
                    None if !switched && old.branch.is_some() && old.head.is_none() => None,
                    None => repo.default_base(head, inner.default_branch.as_deref())?,
                };
                if base.as_deref() != Some(head) {
                    if let Some(previous) = &previous
                        && !switched
                        && branch.is_some()
                        && !repo.is_ancestor(previous, head)?
                    {
                        let replaced = repo.count(&format!("{head}..{previous}"))?;
                        let added = repo.count(&format!("{previous}..{head}"))?;
                        headline = "History rewritten".into();
                        lines.push(format!(
                            "History rewritten: {replaced} commits replaced by {added}"
                        ));
                    }
                    let range = base
                        .as_ref()
                        .map_or_else(|| head.clone(), |base| format!("{base}..{head}"));
                    let total = repo.count(&range)?;
                    let commits = repo.commits(&range)?;
                    let email = repo.optional(&["config", "--get", "user.email"])?;
                    let mut unseen = Vec::new();
                    for commit in &commits {
                        if commit.is_own(email.as_deref(), window) {
                            let count = repo.file_count(&commit.hash)?;
                            lines.push(format!(
                                "committed {} {}, {count} files",
                                commit.short, commit.subject
                            ));
                            if headline.is_empty() {
                                headline = "Committed".into();
                            }
                        } else {
                            unseen.push(format!(
                                "{}  {}  {}",
                                commit.short, commit.author, commit.subject
                            ));
                        }
                    }
                    if !unseen.is_empty() {
                        lines.push(format!("Unseen commits:\n{}", unseen.join("\n")));
                        if headline.is_empty() {
                            headline = "Unseen commits".into();
                        }
                    }
                    if total > commits.len() {
                        lines.push(format!(
                            "and {} more commits (list capped at {COMMIT_LIMIT})",
                            total - commits.len()
                        ));
                    }
                    if !unseen.is_empty() || total > commits.len() {
                        let stat = repo.stat(base.as_deref(), head)?;
                        if !stat.is_empty() {
                            lines.push(format!("Aggregate changes:\n{stat}"));
                        }
                    }
                }
            } else if moved {
                lines.push("HEAD has no commits.".into());
                if headline.is_empty() {
                    headline = "Git HEAD changed".into();
                }
            }
        }

        let mut folded = Vec::new();
        if head.is_some() {
            files.retain(|file| {
                if equals_head(&inner.root, &file.path) {
                    folded.push(file.display.clone());
                    false
                } else {
                    true
                }
            });
        }
        if !first && !moved && !folded.is_empty() {
            if headline.is_empty() {
                headline = "Working tree reverted".into();
            }
            let count = folded.len();
            folded.truncate(COMMIT_LIMIT);
            lines.push(format!(
                "Working tree reverted to HEAD for {count} files:\n{}",
                folded.join("\n")
            ));
            if count > folded.len() {
                lines.push(format!("and {} more files", count - folded.len()));
            }
        }
        if let Some(head) = &head {
            inner.state.per_branch.insert(key.to_owned(), head.clone());
        } else {
            inner.state.per_branch.remove(key);
        }
        inner.state.branch = branch;
        inner.state.head = head;
        Ok((!lines.is_empty()).then(|| event(&headline, lines.join("\n\n"))))
    }
}

fn short(hash: Option<&str>) -> &str {
    hash.map(|hash| {
        let end = hash
            .char_indices()
            .nth(12)
            .map_or(hash.len(), |(offset, _)| offset);
        &hash[..end]
    })
    .unwrap_or("unborn HEAD")
}

fn location(branch: Option<&str>, head: Option<&str>) -> String {
    branch
        .map(str::to_owned)
        .unwrap_or_else(|| format!("detached {}", short(head)))
}

fn event(headline: &str, body: String) -> GitEvent {
    let body = body
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    GitEvent {
        headline: headline.into(),
        text: format!("<git-event>\n{body}\n</git-event>"),
    }
}

/// Worktrees have private HEAD/index metadata and shared refs in another directory.
pub fn metadata_dirs(root: &Path) -> Vec<PathBuf> {
    let repo = Repo(root);
    let mut dirs = Vec::new();
    for flag in ["--git-dir", "--git-common-dir"] {
        if let Ok(Some(path)) = repo.optional(&["rev-parse", "--path-format=absolute", flag])
            && let Ok(path) = PathBuf::from(path).canonicalize()
            && !dirs.contains(&path)
        {
            dirs.push(path);
        }
    }
    dirs
}

struct Repo<'a>(&'a Path);

impl Repo<'_> {
    fn output(&self, args: &[&str]) -> Result<std::process::Output> {
        Command::new("git")
            .arg("--no-pager")
            .arg("--no-replace-objects")
            .args(["-c", "core.fsmonitor=false", "-c", "color.ui=false"])
            .arg("-C")
            .arg(self.0)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .output()
            .context("reading Git state")
    }

    fn optional(&self, args: &[&str]) -> Result<Option<String>> {
        let out = self.output(args)?;
        Ok(out.status.success().then(|| {
            let text = String::from_utf8_lossy(&out.stdout);
            text.strip_suffix('\n').unwrap_or(&text).to_owned()
        }))
    }

    fn required(&self, args: &[&str]) -> Result<String> {
        let out = self.output(args)?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args[0],
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
    }

    fn resolve(&self, reference: &str) -> Result<Option<String>> {
        self.optional(&[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ])
    }

    fn default_base(&self, head: &str, configured: Option<&str>) -> Result<Option<String>> {
        let candidates = configured.map_or_else(
            || {
                vec![
                    "refs/remotes/origin/HEAD",
                    "refs/heads/main",
                    "refs/heads/master",
                ]
            },
            |branch| vec![branch],
        );
        for candidate in candidates {
            if let Some(tip) = self.resolve(candidate)? {
                return self.optional(&["merge-base", head, &tip]);
            }
        }
        Ok(None)
    }

    fn is_ancestor(&self, old: &str, new: &str) -> Result<bool> {
        let output = self.output(&["merge-base", "--is-ancestor", old, new])?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!(
                "git merge-base failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        }
    }

    fn count(&self, range: &str) -> Result<usize> {
        self.required(&["rev-list", "--count", range, "--"])?
            .parse()
            .context("reading Git commit count")
    }

    fn commits(&self, range: &str) -> Result<Vec<Commit>> {
        let raw = self.required(&[
            "log",
            "--no-show-signature",
            "--no-decorate",
            "-z",
            "--max-count=50",
            "--format=%H%x00%h%x00%an%x00%s%x00%ce%x00%ct",
            range,
            "--",
        ])?;
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        raw.trim_end_matches('\0')
            .split('\0')
            .collect::<Vec<_>>()
            .chunks(6)
            .map(|fields| {
                if fields.len() != 6 {
                    bail!("invalid git log record");
                }
                Ok(Commit {
                    hash: fields[0].into(),
                    short: fields[1].into(),
                    author: fields[2].into(),
                    subject: fields[3].into(),
                    email: fields[4].into(),
                    time: fields[5].parse()?,
                })
            })
            .collect()
    }

    fn file_count(&self, head: &str) -> Result<usize> {
        let output = self.output(&[
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-only",
            "--no-renames",
            "-r",
            "-m",
            "-z",
            head,
            "--",
        ])?;
        if !output.status.success() {
            bail!("reading committed file count failed");
        }
        Ok(output
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .collect::<std::collections::BTreeSet<_>>()
            .len())
    }

    fn stat(&self, base: Option<&str>, head: &str) -> Result<String> {
        let mut args = vec![
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--stat=100,70,50",
        ];
        let empty;
        if let Some(base) = base {
            args.extend([base, head]);
        } else {
            // Hash the empty tree without writing it; this also supports SHA-256 repositories.
            empty = self.required(&["hash-object", "-t", "tree", "--stdin"])?;
            args.extend([&empty, head]);
        }
        args.push("--");
        let mut stat = self.required(&args)?;
        if stat.len() > STAT_LIMIT {
            let mut end = STAT_LIMIT;
            while !stat.is_char_boundary(end) {
                end -= 1;
            }
            stat.truncate(end);
            stat.push_str("\n[stat truncated]");
        }
        Ok(stat)
    }
}

struct Commit {
    hash: String,
    short: String,
    author: String,
    subject: String,
    email: String,
    time: u64,
}

impl Commit {
    fn is_own(&self, email: Option<&str>, window: Option<(SystemTime, SystemTime)>) -> bool {
        let Some((start, end)) = window else {
            return false;
        };
        let (Ok(start), Ok(end)) = (
            start.duration_since(UNIX_EPOCH),
            end.duration_since(UNIX_EPOCH),
        ) else {
            return false;
        };
        email == Some(self.email.as_str()) && (start.as_secs()..=end.as_secs()).contains(&self.time)
    }
}

#[cfg(test)]
mod tests;
