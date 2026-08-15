//! Filesystem sandbox primitives.
//!
//! These helpers enforce the `$HOME`-rooted sandbox boundary for all
//! sidecar filesystem IPC commands. They are the single source of truth
//! for symlink refusal, canonicalization, and scope checks.
//!
//! **Do not inline these checks in command modules.** Every sandbox
//! primitive lives here so that `list.rs`, `read.rs`, and `write.rs`
//! cannot silently diverge. See `SECURITY.md` in this directory
//! for the full threat model and a map of each primitive to the
//! review findings it prevents.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};

/// Expand `~` to the user's home directory.
///
/// Returns the input unchanged if it does not start with `~`, or if the
/// home directory cannot be determined (in which case downstream checks
/// will reject the path anyway).
pub(crate) fn expand_home(path: &str) -> PathBuf {
    if path == "~" || path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            if path == "~" {
                return home;
            }
            return home.join(&path[2..]);
        }
    }
    PathBuf::from(path)
}

/// Resolve the canonical home directory.
///
/// Failure here means we cannot enforce the sandbox at all, so every
/// caller treats it as a fatal `access denied`.
pub(crate) fn home_canonical() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| "cannot determine home directory".to_string())?;
    fs::canonicalize(&home).map_err(|e| format!("cannot resolve home dir: {}", e))
}

/// Reject any `..` component in a path.
///
/// Called before any filesystem mutation so a forged path like
/// `~/../etc/evil.txt` cannot trigger `create_dir_all` or any other
/// side effect outside of home. Legitimate UI paths are built from
/// the current working directory plus a basename, so `..` is always
/// suspicious and blocking it sidesteps the subtle interaction between
/// lexical `Path::parent()` walks and OS-level `..` resolution.
pub(crate) fn reject_parent_refs(path: &Path) -> Result<(), String> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!(
            "access denied: path contains parent traversal segments: {}",
            path.display()
        ));
    }
    Ok(())
}

/// Verify that a canonical path is under the canonical home directory.
///
/// The caller is responsible for canonicalizing first; this helper only
/// performs the `starts_with` check.
pub(crate) fn ensure_within_home(canonical: &Path, home_canonical: &Path) -> Result<(), String> {
    if !canonical.starts_with(home_canonical) {
        return Err(format!(
            "access denied: path is outside home directory: {}",
            canonical.display()
        ));
    }
    Ok(())
}

/// Walk up `parent` until we find an existing ancestor, canonicalize it,
/// verify it's inside home, then re-anchor the unresolved tail onto the
/// canonical ancestor. Finally, create missing segments ONE AT A TIME,
/// canonicalizing after each step — this caps the blast radius of a
/// racing symlink at one stray empty directory per call.
///
/// Returns the canonical `PathBuf` of the resolved parent. Guaranteed
/// to be inside `home_canonical` on success.
///
/// # Why per-segment mkdir?
///
/// `create_dir_all` in a single shot would leave a TOCTOU window where
/// a concurrent process creates a symlink at an intermediate
/// not-yet-existing component and redirects subsequent segments outside
/// home. Even if the eventual write is blocked by the final scope
/// re-check plus `O_NOFOLLOW`, `create_dir_all` itself would have
/// already created empty directories outside the sandbox along the way.
///
/// Walking the segments one at a time and canonicalizing after each
/// `create_dir` means the first out-of-scope segment is detected
/// immediately, before any further creations happen — at worst one
/// stray empty dir can be created per call. Full mitigation would need
/// `openat(2)` with `O_NOFOLLOW` per component, but that is a larger
/// refactor; this approach closes the practical window for the
/// single-user desktop threat model.
///
/// # Why swallow `AlreadyExists`?
///
/// `exists()` + `create_dir` is not atomic: a concurrent process could
/// create the same directory in the gap. The `AlreadyExists` case is
/// benign — the directory we wanted to create is already there — so we
/// swallow it and let the subsequent canonicalize + scope check verify
/// the final state is still inside home. Any OTHER error is fatal.
/// Do not "clean up" this match arm into a `?` operator.
///
/// # Caller invariant
///
/// **Callers must call `reject_parent_refs` on `parent` before invoking
/// this function.** `canonicalize_within_home` does not re-check for
/// `..` components and relies on the upstream rejection to ensure that
/// lexical joins with the canonical ancestor cannot escape `home_canonical`.
/// Without the upstream check, a path containing `..` segments can trigger
/// surprising `strip_prefix` behavior and potentially skip the
/// `ensure_within_home` scope check on intermediate segments.
pub(super) fn canonicalize_within_home(
    parent: &Path,
    home_canonical: &Path,
) -> Result<PathBuf, String> {
    let mut ancestor = parent;
    let existing_ancestor = loop {
        if ancestor.exists() {
            break ancestor;
        }
        match ancestor.parent() {
            Some(next) => ancestor = next,
            None => {
                return Err(format!(
                    "access denied: path has no existing ancestor under home: {}",
                    parent.display()
                ));
            }
        }
    };

    let ancestor_canonical = fs::canonicalize(existing_ancestor)
        .map_err(|e| format!("invalid ancestor path '{}': {}", parent.display(), e))?;

    ensure_within_home(&ancestor_canonical, home_canonical)?;

    let relative_tail = parent.strip_prefix(existing_ancestor).map_err(|_| {
        format!(
            "invalid path: cannot rebase '{}' onto '{}'",
            parent.display(),
            existing_ancestor.display()
        )
    })?;

    let mut resolved_parent = ancestor_canonical.clone();
    for segment in relative_tail.components() {
        let next = resolved_parent.join(segment);
        if !next.exists() {
            match fs::create_dir(&next) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(format!(
                        "failed to create directory '{}': {}",
                        next.display(),
                        e
                    ));
                }
            }
        }
        let next_canonical = fs::canonicalize(&next)
            .map_err(|e| format!("failed to canonicalize '{}': {}", next.display(), e))?;
        ensure_within_home(&next_canonical, home_canonical)?;
        resolved_parent = next_canonical;
    }

    // Belt-and-braces final check in case the loop didn't run (no tail segments)
    ensure_within_home(&resolved_parent, home_canonical)?;

    Ok(resolved_parent)
}

/// Open a file with kernel-level refusal to follow symlinks on the leaf.
///
/// - **Unix:** `O_NOFOLLOW` makes `open(2)` return `ELOOP` if the final
///   component is a symlink. Atomic refusal, no TOCTOU window.
/// - **Windows:** `FILE_FLAG_OPEN_REPARSE_POINT` opens the reparse point
///   itself rather than following it. We then post-check the handle's
///   file type and reject if we landed on a symlink, giving the same
///   end-state as the Unix `ELOOP` path.
///
/// The caller supplies `options` pre-configured with read/write/create
/// as needed; this function only adds the symlink-refusal flags and
/// the Windows post-open metadata check. Keep all `cfg(unix)` and
/// `cfg(windows)` arms for this behavior in this single function — if
/// they ever drift across files, the Windows post-open check gets
/// silently skipped (this is exactly how finding #9 in the review
/// knowledge base slipped through the first time).
pub(crate) fn open_nofollow(path: &Path, mut options: OpenOptions) -> Result<File, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x00200000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    let file = options
        .open(path)
        .map_err(|e| format!("failed to open '{}': {}", path.display(), e))?;

    #[cfg(windows)]
    {
        let metadata = file
            .metadata()
            .map_err(|e| format!("failed to stat '{}': {}", path.display(), e))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "access denied: refusing to operate through symlink: {}",
                path.display()
            ));
        }
    }

    Ok(file)
}
