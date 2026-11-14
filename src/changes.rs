//! The file half of a plan: what the agent added, modified, and deleted.

use std::collections::BTreeSet;
use std::fs::{self, File, Metadata};
use std::io::{self, BufReader, Read};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::git::GitSummary;
use crate::util;

/// Directory entries never treated as project content. Repository metadata
/// is summarized separately (see `git::GitSummary`) and never applied.
const SKIPPED_NAMES: &[&str] = &[".git"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Entry {
    File { mode: u32, size: u64 },
    Symlink { target: PathBuf },
    Dir,
}

impl Entry {
    fn from_metadata(path: &Path, meta: &Metadata) -> io::Result<Option<Entry>> {
        let ft = meta.file_type();
        Ok(if ft.is_symlink() {
            Some(Entry::Symlink {
                target: fs::read_link(path)?,
            })
        } else if ft.is_dir() {
            Some(Entry::Dir)
        } else if ft.is_file() {
            Some(Entry::File {
                mode: meta.mode() & 0o7777,
                size: meta.len(),
            })
        } else {
            // Sockets, FIFOs, devices (and overlay whiteouts) are not content.
            None
        })
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, Entry::Dir)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

impl ChangeKind {
    pub fn letter(self) -> char {
        match self {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Deleted => 'D',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Path relative to the project root.
    pub path: PathBuf,
    pub kind: ChangeKind,
    pub before: Option<Entry>,
    pub after: Option<Entry>,
    /// Whether git ignores this path (build output, dependencies, ...).
    #[serde(default)]
    pub ignored: bool,
}

/// Everything the agent changed on disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChangeSet {
    pub changes: Vec<Change>,
    #[serde(default)]
    pub git: Option<GitSummary>,
}

impl ChangeSet {
    pub fn load(path: &Path) -> Result<ChangeSet> {
        let data = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_slice(&data).with_context(|| format!("cannot parse {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        util::write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn sort(&mut self) {
        self.changes.sort_by(|a, b| a.path.cmp(&b.path));
    }
}

/// Where to find the before/after content of a change set.
#[derive(Debug, Clone)]
pub struct Trees {
    pub base: PathBuf,
    pub work: PathBuf,
}

impl Trees {
    pub fn base_path(&self, rel: &Path) -> PathBuf {
        self.base.join(rel)
    }
    pub fn work_path(&self, rel: &Path) -> PathBuf {
        self.work.join(rel)
    }
}

/// Reads `path`'s metadata without following symlinks. Returns `None` if it
/// does not exist (or, with `whiteouts`, if it is an overlayfs whiteout).
fn lstat(path: &Path, whiteouts: bool) -> io::Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(m) if whiteouts && is_whiteout(&m) => Ok(None),
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        // A path component that is a file (e.g. "a/b" where "a" is a file).
        Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => Ok(None),
        Err(e) => Err(e),
    }
}

/// overlayfs marks deleted lower entries with a 0/0 character device.
pub fn is_whiteout(meta: &Metadata) -> bool {
    meta.file_type().is_char_device() && meta.rdev() == 0
}

fn sorted_names(dir: &Path) -> io::Result<BTreeSet<std::ffi::OsString>> {
    match fs::read_dir(dir) {
        Ok(rd) => rd
            .map(|e| e.map(|e| e.file_name()))
            .filter(|n| {
                n.as_ref()
                    .map_or(true, |n| !SKIPPED_NAMES.iter().any(|s| n == *s))
            })
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(e) => Err(e),
    }
}

/// Whether two regular files have the same content (and permission bits).
///
/// Like git and rsync, files with identical size and mtime are assumed equal
/// without reading them; snapshots preserve mtimes, so untouched files take
/// this fast path.
pub fn same_file(a: &Path, ma: &Metadata, b: &Path, mb: &Metadata) -> io::Result<bool> {
    if ma.len() != mb.len() || (ma.mode() & 0o7777) != (mb.mode() & 0o7777) {
        return Ok(false);
    }
    if ma.mtime() == mb.mtime() && ma.mtime_nsec() == mb.mtime_nsec() {
        return Ok(true);
    }
    same_content(a, b)
}

pub fn same_content(a: &Path, b: &Path) -> io::Result<bool> {
    let mut ra = BufReader::with_capacity(64 * 1024, File::open(a)?);
    let mut rb = BufReader::with_capacity(64 * 1024, File::open(b)?);
    let (mut ba, mut bb) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let na = read_full(&mut ra, &mut ba)?;
        let nb = read_full(&mut rb, &mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

struct Scanner<'a> {
    base: &'a Path,
    work: &'a Path,
    /// Treat overlayfs whiteouts in `work` as absent.
    whiteouts: bool,
    out: Vec<Change>,
}

impl Scanner<'_> {
    fn push(
        &mut self,
        path: PathBuf,
        kind: ChangeKind,
        before: Option<Entry>,
        after: Option<Entry>,
    ) {
        self.out.push(Change {
            path,
            kind,
            before,
            after,
            ignored: false,
        });
    }

    /// Full comparison of `base/rel` and `work/rel`, both of which are
    /// complete trees.
    fn compare_dirs(&mut self, rel: &Path) -> io::Result<()> {
        let mut names = sorted_names(&self.base.join(rel))?;
        names.extend(sorted_names(&self.work.join(rel))?);
        for name in names {
            let p = rel.join(&name);
            let b = lstat(&self.base.join(&p), false)?;
            let w = lstat(&self.work.join(&p), self.whiteouts)?;
            self.compare_entry(&p, b.as_ref(), w.as_ref())?;
        }
        Ok(())
    }

    fn compare_entry(
        &mut self,
        p: &Path,
        b_meta: Option<&Metadata>,
        w_meta: Option<&Metadata>,
    ) -> io::Result<()> {
        let be = match b_meta {
            Some(m) => Entry::from_metadata(&self.base.join(p), m)?,
            None => None,
        };
        let we = match w_meta {
            Some(m) => Entry::from_metadata(&self.work.join(p), m)?,
            None => None,
        };
        match (be, we) {
            (None, None) => {}
            (Some(Entry::Dir), Some(Entry::Dir)) => self.compare_dirs(p)?,
            (Some(Entry::Dir), w) => {
                self.delete_tree(p)?;
                if let Some(w) = w {
                    self.push(p.to_path_buf(), ChangeKind::Added, None, Some(w));
                }
            }
            (b, Some(Entry::Dir)) => {
                if let Some(b) = b {
                    self.push(p.to_path_buf(), ChangeKind::Deleted, Some(b), None);
                }
                self.add_tree(p)?;
            }
            (None, Some(w)) => self.push(p.to_path_buf(), ChangeKind::Added, None, Some(w)),
            (Some(b), None) => self.push(p.to_path_buf(), ChangeKind::Deleted, Some(b), None),
            (Some(b), Some(w)) => {
                let same = match (&b, &w) {
                    (Entry::File { .. }, Entry::File { .. }) => match (b_meta, w_meta) {
                        (Some(bm), Some(wm)) => {
                            same_file(&self.base.join(p), bm, &self.work.join(p), wm)?
                        }
                        _ => false,
                    },
                    (Entry::Symlink { target: x }, Entry::Symlink { target: y }) => x == y,
                    _ => false,
                };
                if !same {
                    self.push(p.to_path_buf(), ChangeKind::Modified, Some(b), Some(w));
                }
            }
        }
        Ok(())
    }

    /// Records everything under `work/rel` as added. Empty directories are
    /// recorded explicitly; other directories are implied by their contents.
    fn add_tree(&mut self, rel: &Path) -> io::Result<()> {
        let names = sorted_names(&self.work.join(rel))?;
        let mut any = false;
        for name in names {
            let p = rel.join(&name);
            let Some(m) = lstat(&self.work.join(&p), self.whiteouts)? else {
                continue;
            };
            let Some(e) = Entry::from_metadata(&self.work.join(&p), &m)? else {
                continue;
            };
            any = true;
            if e.is_dir() {
                self.add_tree(&p)?;
            } else {
                self.push(p, ChangeKind::Added, None, Some(e));
            }
        }
        if !any {
            self.push(rel.to_path_buf(), ChangeKind::Added, None, Some(Entry::Dir));
        }
        Ok(())
    }

    /// Records everything under `base/rel` as deleted, children first,
    /// including the directory itself.
    fn delete_tree(&mut self, rel: &Path) -> io::Result<()> {
        for name in sorted_names(&self.base.join(rel))? {
            let p = rel.join(&name);
            let Some(m) = lstat(&self.base.join(&p), false)? else {
                continue;
            };
            let Some(e) = Entry::from_metadata(&self.base.join(&p), &m)? else {
                continue;
            };
            if e.is_dir() {
                self.delete_tree(&p)?;
            } else {
                self.push(p, ChangeKind::Deleted, Some(e), None);
            }
        }
        self.push(
            rel.to_path_buf(),
            ChangeKind::Deleted,
            Some(Entry::Dir),
            None,
        );
        Ok(())
    }

    /// Walks an overlayfs upper layer, where `self.work` is the upper
    /// directory and `self.base` is the (unchanged) lower directory.
    fn scan_upper(&mut self, rel: &Path) -> io::Result<()> {
        for name in sorted_names(&self.work.join(rel))? {
            let p = rel.join(&name);
            let Some(um) = lstat(&self.work.join(&p), false)? else {
                continue;
            };
            let lm = lstat(&self.base.join(&p), false)?;
            if is_whiteout(&um) {
                self.compare_entry(&p, lm.as_ref(), None)?;
            } else if um.is_dir() && lm.as_ref().is_some_and(Metadata::is_dir) {
                if is_opaque(&self.work.join(&p)) {
                    // The upper directory replaces the lower one entirely.
                    self.compare_dirs(&p)?;
                } else {
                    self.scan_upper(&p)?;
                }
            } else {
                self.compare_entry(&p, lm.as_ref(), Some(&um))?;
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn is_opaque(dir: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    for name in [c"user.overlay.opaque", c"trusted.overlay.opaque"] {
        let mut buf = [0u8; 4];
        // SAFETY: valid NUL-terminated strings and a buffer of the stated length.
        let n = unsafe {
            libc::lgetxattr(
                c.as_ptr(),
                name.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if n == 1 && buf[0] == b'y' {
            return true;
        }
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn is_opaque(_dir: &Path) -> bool {
    false
}

/// Compares two complete trees.
pub fn diff_trees(base: &Path, work: &Path) -> io::Result<Vec<Change>> {
    let mut s = Scanner {
        base,
        work,
        whiteouts: false,
        out: Vec::new(),
    };
    s.compare_dirs(Path::new(""))?;
    Ok(s.out)
}

/// Computes changes from an overlayfs upper layer over `lower`.
pub fn diff_overlay(lower: &Path, upper: &Path) -> io::Result<Vec<Change>> {
    let mut s = Scanner {
        base: lower,
        work: upper,
        whiteouts: true,
        out: Vec::new(),
    };
    s.scan_upper(Path::new(""))?;
    Ok(s.out)
}

/// Line statistics for one change, for display.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LineStats {
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
}

/// Reads the content of one side of a change, as git would show it: file
/// bytes, or a symlink's target.
pub fn side_bytes(entry: Option<&Entry>, path: &Path) -> io::Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    match entry {
        Some(Entry::File { .. }) => fs::read(path),
        Some(Entry::Symlink { target }) => Ok(target.as_os_str().as_bytes().to_vec()),
        Some(Entry::Dir) | None => Ok(Vec::new()),
    }
}

pub fn line_stats(change: &Change, trees: &Trees) -> io::Result<LineStats> {
    let before = side_bytes(change.before.as_ref(), &trees.base_path(&change.path))?;
    let after = side_bytes(change.after.as_ref(), &trees.work_path(&change.path))?;
    if util::looks_binary(&before) || util::looks_binary(&after) {
        return Ok(LineStats {
            binary: true,
            ..Default::default()
        });
    }
    let (a, b) = (
        String::from_utf8_lossy(&before),
        String::from_utf8_lossy(&after),
    );
    let diff = similar::TextDiff::configure()
        .timeout(std::time::Duration::from_secs(2))
        .diff_lines(a.as_ref(), b.as_ref());
    let mut stats = LineStats::default();
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            match change.tag() {
                similar::ChangeTag::Insert => stats.added += 1,
                similar::ChangeTag::Delete => stats.removed += 1,
                similar::ChangeTag::Equal => {}
            }
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn summary(changes: &[Change]) -> Vec<String> {
        changes
            .iter()
            .map(|c| format!("{} {}", c.kind.letter(), c.path.display()))
            .collect()
    }

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let work = tmp.path().join("work");
        for root in [&base, &work] {
            write(root, "keep.txt", "same");
            write(root, "edit.txt", "v1");
            write(root, "gone.txt", "bye");
            write(root, "dir/nested.txt", "n");
            write(root, "olddir/a.txt", "a");
            write(root, ".git/HEAD", "ref: refs/heads/main");
        }
        (tmp, base, work)
    }

    #[test]
    fn detects_adds_edits_deletes_and_dirs() {
        let (_tmp, base, work) = setup();
        fs::write(work.join("edit.txt"), "v2").unwrap();
        fs::remove_file(work.join("gone.txt")).unwrap();
        write(&work, "new/deep/file.txt", "hi");
        fs::create_dir(work.join("emptydir")).unwrap();
        fs::remove_dir_all(work.join("olddir")).unwrap();
        fs::write(work.join(".git/HEAD"), "changed").unwrap();

        let changes = diff_trees(&base, &work).unwrap();
        assert_eq!(
            summary(&changes),
            [
                "M edit.txt",
                "A emptydir",
                "D gone.txt",
                "A new/deep/file.txt",
                "D olddir/a.txt",
                "D olddir",
            ]
        );
    }

    #[test]
    fn touched_but_identical_files_are_unchanged() {
        let (_tmp, base, work) = setup();
        // Rewrite with the same content: new mtime, same bytes.
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(work.join("keep.txt"), "same").unwrap();
        assert!(diff_trees(&base, &work).unwrap().is_empty());
    }

    #[test]
    fn same_size_edits_are_detected() {
        let (_tmp, base, work) = setup();
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(work.join("keep.txt"), "SAME").unwrap();
        assert_eq!(summary(&diff_trees(&base, &work).unwrap()), ["M keep.txt"]);
    }

    #[test]
    fn mode_and_symlink_and_type_changes() {
        let (_tmp, base, work) = setup();
        fs::set_permissions(work.join("edit.txt"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("keep.txt", base.join("ln")).unwrap();
        symlink("edit.txt", work.join("ln")).unwrap();
        // file -> directory
        fs::remove_file(work.join("gone.txt")).unwrap();
        write(&work, "gone.txt/inner", "x");
        let changes = diff_trees(&base, &work).unwrap();
        assert_eq!(
            summary(&changes),
            ["M edit.txt", "D gone.txt", "A gone.txt/inner", "M ln"]
        );
    }
}
