//! A session is one `agentplan run`: its metadata, snapshots, and logs.
//!
//! On-disk layout of `<state_dir>/sessions/<id>/`:
//!
//! ```text
//! session.json   metadata (this module)
//! changes.json   the computed file changes (see `changes`)
//! net.jsonl      captured network requests (see `net::log`)
//! ca.pem         the session's interception CA certificate (public part only)
//! base/          the project as it was before the run
//! work/<name>/   the agent's working copy        (clone backend)
//! upper/         the overlay's upper layer         (overlay backend)
//! ovl-work/      overlayfs scratch directory       (overlay backend)
//! ```

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::git::GitBefore;
use crate::project::Project;
use crate::util;

/// Bumped when the on-disk format changes incompatibly.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// The project is cloned (APFS `clonefile`, reflink, or copy); the agent
    /// runs in the clone.
    Clone,
    /// Linux only: an overlayfs is mounted over the project path inside a
    /// private mount namespace, so the agent sees the real path.
    Overlay,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Clone => "clone",
            Backend::Overlay => "overlay",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// The agent is running (or agentplan was killed while it ran).
    Running,
    /// The plan is ready to review.
    Ready,
    /// Some, but not all, changes were applied.
    PartiallyApplied,
    /// Every change was applied.
    Applied,
    /// Preparing the session or computing the plan failed.
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl Exit {
    pub fn describe(&self) -> String {
        match (self.code, self.signal) {
            (Some(c), _) => format!("exited {c}"),
            (None, Some(s)) => format!("killed by signal {s}"),
            (None, None) => "exited".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub format: u32,
    pub id: String,
    pub agentplan_version: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub finished_at: Option<OffsetDateTime>,
    pub project_root: PathBuf,
    /// Working directory relative to the project root.
    pub cwd: PathBuf,
    pub command: Vec<String>,
    pub backend: Backend,
    /// Network policy mode the run used, or `off`.
    pub network: String,
    /// PID of the `agentplan run` process while it is running.
    pub pid: u32,
    pub status: Status,
    #[serde(default)]
    pub exit: Option<Exit>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub git_before: Option<GitBefore>,
    /// Paths (relative to the project root) already applied to the project.
    #[serde(default)]
    pub applied: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub dir: PathBuf,
    pub meta: Meta,
}

impl Session {
    pub fn create(
        project: &Project,
        command: Vec<String>,
        backend: Backend,
        network: &str,
    ) -> Result<Session> {
        let sessions = project.sessions_dir();
        fs::create_dir_all(&sessions)?;
        let (id, dir) = loop {
            let id = util::short_id(6)?;
            let dir = sessions.join(&id);
            match fs::create_dir(&dir) {
                Ok(()) => break (id, dir),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).context("cannot create session directory"),
            }
        };
        let meta = Meta {
            format: FORMAT_VERSION,
            id,
            agentplan_version: env!("CARGO_PKG_VERSION").to_string(),
            created_at: OffsetDateTime::now_utc(),
            finished_at: None,
            project_root: project.root.clone(),
            cwd: project.cwd_relative(),
            command,
            backend,
            network: network.to_string(),
            pid: std::process::id(),
            status: Status::Running,
            exit: None,
            error: None,
            git_before: None,
            applied: BTreeSet::new(),
        };
        let session = Session { dir, meta };
        session.save()?;
        Ok(session)
    }

    pub fn load(dir: &Path) -> Result<Session> {
        let path = dir.join("session.json");
        let data = fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        let meta: Meta = serde_json::from_slice(&data)
            .with_context(|| format!("cannot parse {}", path.display()))?;
        if meta.format > FORMAT_VERSION {
            bail!(
                "plan {} was created by a newer agentplan ({}); please upgrade",
                meta.id,
                meta.agentplan_version
            );
        }
        Ok(Session {
            dir: dir.to_path_buf(),
            meta,
        })
    }

    pub fn save(&self) -> Result<()> {
        let data = serde_json::to_vec_pretty(&self.meta)?;
        util::write_atomic(&self.dir.join("session.json"), &data)
    }

    /// All sessions of a project, newest first. Unreadable sessions are skipped.
    pub fn list(project: &Project) -> Result<Vec<Session>> {
        let dir = project.sessions_dir();
        let mut sessions = Vec::new();
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(sessions),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
        };
        for entry in entries.flatten() {
            if let Ok(s) = Session::load(&entry.path()) {
                sessions.push(s);
            }
        }
        sessions.sort_by(|a, b| b.meta.created_at.cmp(&a.meta.created_at));
        Ok(sessions)
    }

    /// Finds a session by ID or unique ID prefix, or the newest one.
    pub fn resolve(project: &Project, id: Option<&str>) -> Result<Session> {
        let sessions = Session::list(project)?;
        let Some(id) = id else {
            return sessions.into_iter().next().with_context(|| {
                format!(
                    "no plans for {}; create one with `agentplan run -- <command>`",
                    project.root.display()
                )
            });
        };
        let mut matches: Vec<Session> = sessions
            .into_iter()
            .filter(|s| s.meta.id.starts_with(id))
            .collect();
        match matches.len() {
            0 => bail!("no plan matches `{id}` (see `agentplan list`)"),
            1 => Ok(matches.remove(0)),
            _ => {
                if let Some(pos) = matches.iter().position(|s| s.meta.id == id) {
                    return Ok(matches.remove(pos));
                }
                let ids: Vec<_> = matches.iter().map(|s| s.meta.id.as_str()).collect();
                bail!("`{id}` is ambiguous: {}", ids.join(", "))
            }
        }
    }

    /// The status to show users: a `Running` session whose process is gone
    /// was interrupted.
    pub fn display_status(&self) -> &'static str {
        match self.meta.status {
            Status::Running if !util::pid_alive(self.meta.pid) => "interrupted",
            Status::Running => "running",
            Status::Ready => "ready",
            Status::PartiallyApplied => "partially applied",
            Status::Applied => "applied",
            Status::Failed => "failed",
        }
    }

    pub fn is_running(&self) -> bool {
        self.meta.status == Status::Running && util::pid_alive(self.meta.pid)
    }

    pub fn base_dir(&self) -> PathBuf {
        self.dir.join("base")
    }

    /// Root of the agent's view of the project after the run: the clone for
    /// the clone backend, the overlay's upper layer for the overlay backend.
    pub fn work_dir(&self) -> PathBuf {
        match self.meta.backend {
            Backend::Clone => self.clone_work_root(),
            Backend::Overlay => self.upper_dir(),
        }
    }

    /// Where the clone backend puts the agent's copy. The last component is
    /// the project's own name so tools that display the directory name (and
    /// agents that key settings on it) behave sensibly.
    pub fn clone_work_root(&self) -> PathBuf {
        let name = self
            .meta
            .project_root
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| "project".into());
        self.dir.join("work").join(name)
    }

    pub fn upper_dir(&self) -> PathBuf {
        self.dir.join("upper")
    }

    pub fn overlay_scratch_dir(&self) -> PathBuf {
        self.dir.join("ovl-work")
    }

    pub fn changes_file(&self) -> PathBuf {
        self.dir.join("changes.json")
    }

    pub fn net_log(&self) -> PathBuf {
        self.dir.join("net.jsonl")
    }

    pub fn ca_cert(&self) -> PathBuf {
        self.dir.join("ca.pem")
    }

    pub fn ca_bundle(&self) -> PathBuf {
        self.dir.join("ca-bundle.pem")
    }

    pub fn git_after_file(&self) -> PathBuf {
        self.dir.join("git-after.json")
    }
}
