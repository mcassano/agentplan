//! Small helpers shared across modules.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use time::OffsetDateTime;

/// FNV-1a, 64-bit. Used for stable, non-cryptographic identifiers (such as
/// naming a project's state directory). `std`'s `DefaultHasher` is explicitly
/// allowed to change between Rust releases, so it is unsuitable here.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Returns `n` random bytes from the operating system.
pub fn random_bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut buf = [0u8; N];
    fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

/// Generates a short, human-typable identifier, e.g. `k3x9ab`.
///
/// The alphabet omits characters that are easy to confuse (`0`/`o`, `1`/`l`).
pub fn short_id(len: usize) -> io::Result<String> {
    const ALPHABET: &[u8] = b"23456789abcdefghijkmnpqrstuvwxyz";
    let bytes = random_bytes::<16>()?;
    Ok(bytes
        .iter()
        .take(len)
        .map(|b| ALPHABET[usize::from(*b) % ALPHABET.len()] as char)
        .collect())
}

/// Writes `contents` to `path` atomically (write to a sibling temp file, then rename).
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    let mut tmp_name = OsString::from(".");
    tmp_name.push(path.file_name().context("path has no file name")?);
    tmp_name.push(format!(".tmp-{}", short_id(6)?));
    let tmp = dir.join(tmp_name);
    {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Lexically normalizes a relative path: strips `.` components and resolves
/// `..` where possible. Returns `None` if the path escapes its base.
pub fn normalize_relative(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::Normal(c) => out.push(c),
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// Renders a path relative to the project root using `/` separators.
pub fn display_rel(path: &Path) -> String {
    let s = String::from_utf8_lossy(path.as_os_str().as_bytes()).into_owned();
    if s.is_empty() { ".".to_string() } else { s }
}

/// Formats a duration compactly: `850ms`, `42s`, `3m12s`, `1h04m`.
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        return format!("{}ms", d.as_millis());
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3600 {
        return format!("{}m{:02}s", secs / 60, secs % 60);
    }
    format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
}

/// Formats how long ago `t` was, relative to now: `just now`, `5m ago`, `3d ago`.
pub fn format_ago(t: OffsetDateTime) -> String {
    let secs = (OffsetDateTime::now_utc() - t).whole_seconds().max(0);
    match secs {
        0..=44 => "just now".to_string(),
        45..=3599 => format!("{}m ago", (secs / 60).max(1)),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// Quotes a command line for display, shell-style.
pub fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            let safe = !a.is_empty()
                && a.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%^".contains(&b));
            if safe {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns whether a process with the given PID currently exists.
pub fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: kill with signal 0 performs only an existence/permission check.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Heuristic used by git: a file is binary if its first 8000 bytes contain NUL.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|b| *b == 0)
}

/// Removes a directory tree, first making directories writable if needed.
/// (overlayfs leaves a mode-000 `work/work` directory behind.)
pub fn remove_tree(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => {
            use std::os::unix::fs::PermissionsExt;
            for entry in walkdir::WalkDir::new(path).into_iter().flatten() {
                if entry.file_type().is_dir() {
                    let _ = fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o700));
                }
            }
            fs::remove_dir_all(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn short_ids_use_alphabet() {
        let id = short_id(6).unwrap();
        assert_eq!(id.len(), 6);
        assert!(id.bytes().all(|b| !b"01lo".contains(&b)));
    }

    #[test]
    fn normalizes_relative_paths() {
        assert_eq!(
            normalize_relative(Path::new("./a/../b/c")),
            Some(PathBuf::from("b/c"))
        );
        assert_eq!(normalize_relative(Path::new("../x")), None);
        assert_eq!(normalize_relative(Path::new("/abs")), None);
    }

    #[test]
    fn shell_join_quotes_when_needed() {
        let args = ["claude", "-p", "fix the bug", "it's"].map(String::from);
        assert_eq!(shell_join(&args), r"claude -p 'fix the bug' 'it'\''s'");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(Duration::from_millis(850)), "850ms");
        assert_eq!(format_duration(Duration::from_secs(42)), "42s");
        assert_eq!(format_duration(Duration::from_secs(192)), "3m12s");
        assert_eq!(format_duration(Duration::from_secs(3840)), "1h04m");
    }
}
