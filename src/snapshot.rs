//! Copying a project tree as cheaply as the filesystem allows.
//!
//! - macOS (APFS): `clonefile(2)` clones whole directory trees copy-on-write
//!   in roughly constant time per entry, sharing data blocks.
//! - Linux: `std::fs::copy` uses `copy_file_range(2)`, which reflinks on
//!   filesystems that support it (btrfs, XFS) and falls back to a real copy.
//!
//! Modification times are preserved so the change scan can skip files whose
//! size and mtime are unchanged without reading them.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};

#[derive(Debug, Default, Clone, Copy)]
pub struct CloneStats {
    /// Top-level entries cloned with a copy-on-write clone.
    pub cloned: u64,
    /// Files copied the slow way.
    pub copied: u64,
}

/// Copies the contents of `src` into the new directory `dst`, skipping the
/// top-level entry `exclude` (agentplan's own state directory, if it lives in
/// the project).
pub fn clone_tree(src: &Path, dst: &Path, exclude: Option<&Path>) -> Result<CloneStats> {
    fs::create_dir_all(dst).with_context(|| format!("cannot create {}", dst.display()))?;
    let mut stats = CloneStats::default();
    let mut entries: Vec<_> = fs::read_dir(src)
        .with_context(|| format!("cannot read {}", src.display()))?
        .collect::<io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let from = entry.path();
        if Some(from.as_path()) == exclude {
            continue;
        }
        let to = dst.join(entry.file_name());
        if clone_entry(&from, &to)? {
            stats.cloned += 1;
        } else {
            copy_recursive(&from, &to, &mut stats)
                .with_context(|| format!("cannot copy {}", from.display()))?;
        }
    }
    let mode = fs::metadata(src)?.permissions().mode();
    fs::set_permissions(dst, fs::Permissions::from_mode(mode))?;
    Ok(stats)
}

/// Clones a single file, preferring a copy-on-write clone.
pub fn clone_file(from: &Path, to: &Path) -> io::Result<()> {
    if clone_entry(from, to).unwrap_or(false) {
        return Ok(());
    }
    fs::copy(from, to)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn clone_entry(from: &Path, to: &Path) -> Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    const CLONE_NOFOLLOW: u32 = 0x0001;
    let src = CString::new(from.as_os_str().as_bytes())?;
    let dst = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: both arguments are valid NUL-terminated paths.
    let rc = unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), CLONE_NOFOLLOW) };
    if rc == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // Not supported by this filesystem, or crossing devices: fall back.
        Some(libc::ENOTSUP) | Some(libc::EXDEV) => Ok(false),
        _ => Err(err).with_context(|| format!("cannot clone {}", from.display())),
    }
}

#[cfg(not(target_os = "macos"))]
fn clone_entry(_from: &Path, _to: &Path) -> Result<bool> {
    Ok(false)
}

fn copy_recursive(from: &Path, to: &Path, stats: &mut CloneStats) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)?;
    } else if ft.is_dir() {
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &to.join(entry.file_name()), stats)?;
        }
        fs::set_permissions(to, meta.permissions())?;
    } else if ft.is_file() {
        fs::copy(from, to)?;
        preserve_mtime(to, &meta)?;
        stats.copied += 1;
    }
    // Sockets, FIFOs and device files are not part of a project's contents.
    Ok(())
}

fn preserve_mtime(path: &Path, meta: &fs::Metadata) -> io::Result<()> {
    let mtime = std::time::UNIX_EPOCH
        + std::time::Duration::new(
            u64::try_from(meta.mtime()).unwrap_or(0),
            u32::try_from(meta.mtime_nsec()).unwrap_or(0),
        );
    let f = fs::OpenOptions::new().write(true).open(path);
    // Read-only files cannot be opened for writing; their mtime is best-effort.
    match f {
        Ok(f) => f.set_modified(mtime),
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_tree_preserving_content_mtime_and_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("a/b")).unwrap();
        fs::write(src.join("a/b/f.txt"), "hello").unwrap();
        fs::write(src.join("x.sh"), "#!/bin/sh").unwrap();
        fs::set_permissions(src.join("x.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("a/b/f.txt", src.join("link")).unwrap();
        fs::create_dir(src.join(".agentplan")).unwrap();

        let dst = tmp.path().join("dst");
        clone_tree(&src, &dst, Some(&src.join(".agentplan"))).unwrap();

        assert_eq!(fs::read_to_string(dst.join("a/b/f.txt")).unwrap(), "hello");
        assert_eq!(
            fs::read_link(dst.join("link")).unwrap(),
            Path::new("a/b/f.txt")
        );
        assert!(!dst.join(".agentplan").exists());
        let m = |p: &Path| fs::symlink_metadata(p).unwrap();
        assert_eq!(m(&dst.join("x.sh")).mode() & 0o777, 0o755);
        for rel in ["a/b/f.txt", "x.sh"] {
            let (a, b) = (m(&src.join(rel)), m(&dst.join(rel)));
            assert_eq!(
                (a.mtime(), a.mtime_nsec()),
                (b.mtime(), b.mtime_nsec()),
                "{rel}"
            );
        }
    }
}
