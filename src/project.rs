//! Locating the project an agent works on, and where agentplan keeps its state.

use std::env;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{git, util};

/// Name of the in-project fallback state directory.
pub const IN_PROJECT_STATE_DIR: &str = ".agentplan";

#[derive(Debug, Clone)]
pub struct Project {
    /// Canonical absolute path of the project root.
    pub root: PathBuf,
    /// Directory holding this project's sessions.
    pub state_dir: PathBuf,
}

impl Project {
    /// Finds the project for the current directory.
    ///
    /// The root is `root_override` if given, otherwise the enclosing git
    /// work tree, otherwise the current directory.
    pub fn discover(root_override: Option<&Path>) -> Result<Self> {
        let cwd = env::current_dir().context("cannot determine current directory")?;
        let root = match root_override {
            Some(p) => p.to_path_buf(),
            None => git::toplevel(&cwd).unwrap_or(cwd),
        };
        let root = root
            .canonicalize()
            .with_context(|| format!("project root {} does not exist", root.display()))?;
        if !root.is_dir() {
            bail!("project root {} is not a directory", root.display());
        }
        if root.parent().is_none() {
            bail!("refusing to use the filesystem root as a project");
        }
        let state_dir = state_dir_for(&root)?;
        Ok(Project { root, state_dir })
    }

    /// The project's directory name, used to name the agent's working copy.
    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".to_string())
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.state_dir.join("sessions")
    }

    /// Path of the current directory relative to the project root, or empty
    /// if the current directory is outside the project.
    pub fn cwd_relative(&self) -> PathBuf {
        env::current_dir()
            .ok()
            .and_then(|c| c.canonicalize().ok())
            .and_then(|c| c.strip_prefix(&self.root).ok().map(Path::to_path_buf))
            .unwrap_or_default()
    }

    /// The state directory, if it lives inside the project tree.
    pub fn state_dir_inside_root(&self) -> Option<&Path> {
        self.state_dir
            .starts_with(&self.root)
            .then_some(self.state_dir.as_path())
    }
}

/// Picks the state directory for a project.
///
/// By default state lives outside the project, under the user's data
/// directory, so that editors, file watchers and build tools never see the
/// copies agentplan makes. On macOS, APFS clones only work within one volume,
/// so if the data directory is on a different volume than the project we
/// fall back to `<project>/.agentplan` (which ignores itself in git).
fn state_dir_for(root: &Path) -> Result<PathBuf> {
    let base = match env::var_os("AGENTPLAN_HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home),
        _ => dirs::data_local_dir()
            .context("cannot determine the user data directory; set AGENTPLAN_HOME")?
            .join("agentplan"),
    };
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let hash = util::fnv1a64(root.as_os_str().as_encoded_bytes());
    let dir = base.join("projects").join(format!("{slug}-{hash:016x}"));
    fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create state directory {}", dir.display()))?;
    let dir = dir.canonicalize()?;

    if cfg!(target_os = "macos") && same_device(&dir, root) == Some(false) {
        let fallback = root.join(IN_PROJECT_STATE_DIR);
        fs::create_dir_all(&fallback)
            .with_context(|| format!("cannot create {}", fallback.display()))?;
        // Keep the state directory out of git without touching the project's .gitignore.
        let ignore = fallback.join(".gitignore");
        if !ignore.exists() {
            fs::write(&ignore, "*\n")?;
        }
        return Ok(fallback.canonicalize()?);
    }
    Ok(dir)
}

fn same_device(a: &Path, b: &Path) -> Option<bool> {
    Some(fs::metadata(a).ok()?.dev() == fs::metadata(b).ok()?.dev())
}
