//! Thin wrappers around the `git` CLI.
//!
//! agentplan does not apply changes to `.git`; it only reports what the
//! agent did there (for example, commits it created).

use std::collections::HashSet;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        // Never take optional locks or write the index as a side effect.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

fn output(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// The top level of the git work tree containing `dir`, if any.
pub fn toplevel(dir: &Path) -> Option<PathBuf> {
    output(git(dir).args(["rev-parse", "--show-toplevel"])).map(PathBuf::from)
}

/// Whether `dir` has a `.git` entry directly inside it.
pub fn is_repo_root(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// The commit `HEAD` points to, if any.
pub fn head(dir: &Path) -> Option<String> {
    output(git(dir).args(["rev-parse", "--verify", "-q", "HEAD"]))
}

/// The current branch name, if `HEAD` is attached.
pub fn branch(dir: &Path) -> Option<String> {
    output(git(dir).args(["symbolic-ref", "--short", "-q", "HEAD"]))
}

/// Returns which of `paths` (relative to `dir`) git considers ignored.
/// Tracked files are never reported as ignored.
pub fn ignored_paths(dir: &Path, paths: &[PathBuf]) -> HashSet<PathBuf> {
    let mut result = HashSet::new();
    if paths.is_empty() || !is_repo_root(dir) {
        return result;
    }
    let Ok(mut child) = git(dir)
        .args(["check-ignore", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    else {
        return result;
    };
    let mut input = Vec::new();
    for p in paths {
        input.extend_from_slice(p.as_os_str().as_bytes());
        input.push(0);
    }
    if let Some(mut stdin) = child.stdin.take() {
        // Write from a thread so a large path list cannot deadlock on full pipes.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let Ok(out) = child.wait_with_output() else {
        return result;
    };
    for chunk in out.stdout.split(|b| *b == 0).filter(|c| !c.is_empty()) {
        result.insert(PathBuf::from(std::ffi::OsStr::from_bytes(chunk)));
    }
    result
}

/// What the agent did to the repository's history.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitSummary {
    pub head_before: Option<String>,
    pub head_after: Option<String>,
    pub branch_before: Option<String>,
    pub branch_after: Option<String>,
    /// One line per commit reachable from `head_after` but not `head_before`,
    /// newest first, formatted as `<short-sha> <subject>`.
    pub new_commits: Vec<String>,
}

impl GitSummary {
    pub fn changed(&self) -> bool {
        self.head_before != self.head_after || self.branch_before != self.branch_after
    }
}

/// The state of the repository before the agent ran.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitBefore {
    pub head: Option<String>,
    pub branch: Option<String>,
}

pub fn capture_before(dir: &Path) -> Option<GitBefore> {
    is_repo_root(dir).then(|| GitBefore {
        head: head(dir),
        branch: branch(dir),
    })
}

/// Compares the repository in `work_dir` against the state captured before the run.
pub fn summarize(before: &GitBefore, work_dir: &Path) -> GitSummary {
    let head_after = head(work_dir);
    let branch_after = branch(work_dir);
    let mut new_commits = Vec::new();
    if let Some(after) = &head_after
        && head_after != before.head
    {
        let range = match &before.head {
            Some(b) => format!("{b}..{after}"),
            None => after.clone(),
        };
        if let Some(log) = output(git(work_dir).args([
            "log",
            "--no-decorate",
            "--format=%h %s",
            "-n",
            "50",
            &range,
        ])) {
            new_commits = log.lines().map(str::to_string).collect();
        }
    }
    GitSummary {
        head_before: before.head.clone(),
        head_after,
        branch_before: before.branch.clone(),
        branch_after,
        new_commits,
    }
}
