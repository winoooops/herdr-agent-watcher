//! Resolve a runner-emitted relative label to an absolute, contained path.

use std::path::{Component, Path};

/// Returns Some(absolute_path_string) when:
///
/// - label has no `..` path components (parent-dir traversal)
/// - label is not absolute
/// - the canonical resolved path is inside the canonical CWD
///
/// Returns None otherwise — the row will render non-clickable.
///
/// We check `..` as an actual path COMPONENT, not as a substring, so legitimate
/// filenames containing the two-character sequence `..` (e.g.
/// `reconnect..server.test.ts`) aren't falsely rejected.
///
/// Convenience wrapper for one-shot callers. Per-snapshot parsers should
/// canonicalize CWD ONCE up front and call `resolve_group_path_with_cwd_canonical`
/// in the loop, since `cwd.canonicalize()` is an OS round-trip and a snapshot
/// can have up to MAX_GROUPS=500 groups.
pub fn resolve_group_path(cwd: &Path, label: &str) -> Option<String> {
    let cwd_canonical = cwd.canonicalize().ok()?;
    resolve_group_path_with_cwd_canonical(&cwd_canonical, label)
}

/// Same as `resolve_group_path` but skips canonicalizing CWD on every call.
/// Use this from within a per-group loop after canonicalizing CWD once.
pub fn resolve_group_path_with_cwd_canonical(cwd_canonical: &Path, label: &str) -> Option<String> {
    let label_path = Path::new(label);
    if label_path.is_absolute()
        || label_path
            .components()
            .any(|c| matches!(c, Component::ParentDir))
    {
        return None;
    }
    let candidate = cwd_canonical.join(label_path).canonicalize().ok()?;
    if !candidate.starts_with(cwd_canonical) {
        return None;
    }
    Some(candidate.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn rejects_dotdot_escape() {
        let dir = tempdir().unwrap();
        assert_eq!(resolve_group_path(dir.path(), "../etc/passwd"), None);
    }

    #[test]
    fn rejects_absolute_label() {
        let dir = tempdir().unwrap();
        assert_eq!(resolve_group_path(dir.path(), "/etc/passwd"), None);
    }

    #[test]
    fn resolves_valid_relative_path() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("foo.test.ts");
        fs::write(&file, "").unwrap();
        let resolved = resolve_group_path(dir.path(), "foo.test.ts");
        assert!(resolved.is_some());
        assert!(resolved.unwrap().contains("foo.test.ts"));
    }

    #[test]
    fn accepts_filenames_containing_double_dots() {
        // Regression: substring `label.contains("..")` would falsely reject
        // legitimate filenames whose name happens to contain `..` (not the
        // parent-dir component). The component-based check distinguishes
        // them. The canonicalize+containment guard below handles the actual
        // escape case.
        let dir = tempdir().unwrap();
        let file = dir.path().join("reconnect..server.test.ts");
        fs::write(&file, "").unwrap();
        let resolved = resolve_group_path(dir.path(), "reconnect..server.test.ts");
        assert!(
            resolved.is_some(),
            "should resolve a filename with .. characters"
        );
        assert!(resolved.unwrap().contains("reconnect..server.test.ts"));
    }

    #[test]
    fn rejects_dotdot_in_middle_of_path() {
        // Component-based check catches `..` whether it's the leading
        // segment or buried in the middle.
        let dir = tempdir().unwrap();
        assert_eq!(resolve_group_path(dir.path(), "src/../../etc/passwd"), None);
    }

    #[test]
    fn returns_none_for_missing_file() {
        let dir = tempdir().unwrap();
        // canonicalize fails for non-existent paths
        assert_eq!(resolve_group_path(dir.path(), "missing.test.ts"), None);
    }

    #[test]
    fn rejects_symlink_pointing_outside_cwd() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outer = tempdir().unwrap();
            let inner = tempdir().unwrap();
            let target = outer.path().join("secret.test.ts");
            fs::write(&target, "").unwrap();
            symlink(&target, inner.path().join("link.test.ts")).unwrap();
            // The symlink resolves outside the inner CWD → reject.
            assert_eq!(resolve_group_path(inner.path(), "link.test.ts"), None);
        }
    }
}
