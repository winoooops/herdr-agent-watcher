//! Path trust checks for adapter-provided status sources.

use std::fs;
use std::path::Path;

/// Ensure the adapter status source stays under the caller's trust root.
///
/// Threat model: Vimeflow is a single-user desktop app, so this protects
/// against accidental path escape and same-user symlink substitution in the
/// workspace tree. Safe `std::fs` has no portable `openat`/`O_NOFOLLOW`
/// directory-creation API, so the function uses a best-effort two-phase
/// check: canonicalize the deepest existing ancestor before `create_dir_all`,
/// then canonicalize the created parent again after creation. A same-user
/// attacker that wins the tiny replacement window can cause `create_dir_all`
/// to create directories outside `trust_root`, but the post-create check
/// aborts before Vimeflow watches or reads from that path. Multi-user or
/// shared-volume deployments should revisit this with fd-pinned traversal
/// such as `cap-std`.
pub(crate) fn ensure_status_source_under_trust_root(
    status_path: &Path,
    trust_root: &Path,
) -> Result<(), String> {
    let canonical_root = fs::canonicalize(trust_root)
        .map_err(|e| format!("trust_root not resolvable: {}: {}", trust_root.display(), e))?;

    let parent = status_path
        .parent()
        .ok_or_else(|| "status file path has no parent directory".to_string())?;

    // Phase 1: pre-check the deepest existing ancestor.
    let mut probe = parent;
    let ancestor_to_check = loop {
        if probe.exists() {
            break probe;
        }
        probe = probe
            .parent()
            .ok_or_else(|| format!("status path escapes filesystem root: {}", parent.display()))?;
    };
    canonicalize_and_verify_under_root(ancestor_to_check, &canonical_root, "status source path")?;

    fs::create_dir_all(parent).map_err(|e| format!("failed to create status directory: {}", e))?;

    // Phase 2: post-create recheck against the now-fully-existing parent.
    // Same helper as the pre-check — see `canonicalize_and_verify_under_root`
    // for the directly-tested behavior. Tests can exercise THIS branch
    // deterministically by pre-creating a symlink at `parent` pointing
    // outside the root and calling the helper directly, without needing
    // fault injection between the `create_dir_all` and the canonicalize
    // (PR #302 cycle 12 F2).
    canonicalize_and_verify_under_root(parent, &canonical_root, "status parent")?;

    Ok(())
}

/// Canonicalize `path` and verify it lives under `canonical_root`. Used by
/// BOTH the pre-check and post-create recheck of
/// `ensure_status_source_under_trust_root` — same canonicalize-+-`starts_with`
/// logic, same error-message shape, single test surface.
///
/// `context_label` is interpolated into the error message so the same
/// helper produces distinct diagnostics for the pre vs. post phase.
/// Extracted in PR #302 cycle 12 F2 to make the post-create-recheck
/// branch unit-testable: a single-threaded test pre-creates a symlink
/// at the path pointing outside the root, then calls this helper
/// directly to assert the symlink is rejected — covering the load-
/// bearing code path that was previously documented as untested.
pub(crate) fn canonicalize_and_verify_under_root(
    path: &Path,
    canonical_root: &Path,
    context_label: &str,
) -> Result<(), String> {
    let canonical = fs::canonicalize(path)
        .map_err(|e| format!("failed to canonicalize {}: {}", context_label, e))?;
    if !canonical.starts_with(canonical_root) {
        return Err(format!(
            "{} escapes trust_root: {} not under {}",
            context_label,
            canonical.display(),
            canonical_root.display()
        ));
    }
    Ok(())
}

mod trusted {
    use crate::agent::adapter::types::LocatedStatusSource;
    use std::path::Path;

    pub(crate) struct TrustedLocatedSource(LocatedStatusSource);

    pub(crate) fn ensure_trusted(
        located: LocatedStatusSource,
    ) -> Result<TrustedLocatedSource, String> {
        super::ensure_status_source_under_trust_root(&located.status_path, &located.trust_root)?;
        Ok(TrustedLocatedSource(located))
    }

    impl TrustedLocatedSource {
        pub(crate) fn status_path(&self) -> &Path {
            &self.0.status_path
        }

        pub(crate) fn into_inner(self) -> LocatedStatusSource {
            self.0
        }
    }
}

pub(crate) use trusted::{ensure_trusted, TrustedLocatedSource};

#[cfg(test)]
mod tests {
    use super::*;
    // Symlink helper is Unix-only; the symlink-driven tests are gated
    // with `#[cfg(unix)]` below so Windows `cargo test` still compiles.
    // Same gating pattern as other test modules in this crate that use
    // `std::os::unix::fs::symlink` (Codex verify cycle 5 follow-up).
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    /// Happy path: status path nested below trust root, all components
    /// canonicalize under it, no symlinks involved. Directory creation
    /// succeeds; both pre- and post-checks pass.
    #[test]
    fn status_path_nested_under_trust_root_succeeds() {
        let root = tempfile::tempdir().expect("tempdir");
        let status = root
            .path()
            .join(".vimeflow")
            .join("sessions")
            .join("sid-1")
            .join("status.json");
        ensure_status_source_under_trust_root(&status, root.path()).expect("nested path");
        assert!(
            status.parent().unwrap().exists(),
            "create_dir_all should run"
        );
    }

    /// Status path resolves exactly TO the trust root's parent directory
    /// (i.e. `..` traversal). The pre-check should refuse because the
    /// deepest existing ancestor canonicalizes outside the root.
    #[test]
    fn parent_traversal_via_dotdot_rejected() {
        let outer = tempfile::tempdir().expect("outer");
        let trust = outer.path().join("workspace");
        std::fs::create_dir(&trust).expect("create trust");
        // Construct a path that uses .. to escape. canonicalize on the
        // deepest existing ancestor (`outer`) resolves outside `trust`.
        let escape = trust.join("..").join("escape").join("status.json");
        let result = ensure_status_source_under_trust_root(&escape, &trust);
        assert!(result.is_err(), "expected ../escape to be rejected");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("escapes trust_root"),
            "expected escape diagnostic, got: {}",
            msg
        );
    }

    /// trust_root itself is a symlink pointing into a real directory.
    /// canonicalization resolves through the symlink, so paths under
    /// the symlink-target should still validate. The check operates on
    /// canonical paths, so this is a sanity test that the symlink-as-
    /// trust-root case doesn't false-reject.
    #[cfg(unix)]
    #[test]
    fn trust_root_as_symlink_resolves_correctly() {
        let outer = tempfile::tempdir().expect("outer");
        let real = outer.path().join("real-workspace");
        std::fs::create_dir(&real).expect("create real");
        let link = outer.path().join("link-workspace");
        symlink(&real, &link).expect("symlink");

        // Use the symlink path as trust_root; status path is under the
        // symlink as well, which canonicalizes through to the real dir.
        let status = link
            .join(".vimeflow")
            .join("sessions")
            .join("s")
            .join("status.json");
        ensure_status_source_under_trust_root(&status, &link).expect("symlinked trust ok");
    }

    /// Pre-existing intermediate-directory symlink to outside the trust
    /// root: covers the case where the path's deepest existing ancestor
    /// is itself a symlink whose target is outside the trust root. The
    /// **pre-check** (lines 17-27 of `ensure_status_source_under_trust_root`)
    /// catches this — it canonicalizes the deepest existing ancestor and
    /// asserts it `starts_with(canonical_root)`.
    ///
    /// **What this test does NOT cover:** the post-create recheck path
    /// at lines 39-47. To exercise that path deterministically, the
    /// pre-check must pass AND a symlink must be injected between the
    /// `create_dir_all` and the post-create canonicalize — a single-
    /// threaded test can't do that without function-level refactoring
    /// (split into pre-check / create / post-check primitives) or a
    /// fault-injection hook. Codex verify cycle 5 (follow-up to F10)
    /// flagged this gap; deterministic post-create-recheck coverage
    /// is tracked as a follow-up. The post-create branch IS still
    /// load-bearing — it remains the only defense against an attacker
    /// who wins the race between `create_dir_all` and the post-check —
    /// and the production code's two-phase canonicalize implementation
    /// exists exactly for this case. This test just doesn't exercise it.
    #[cfg(unix)]
    #[test]
    fn intermediate_symlink_to_outside_rejected_by_pre_check() {
        let outer = tempfile::tempdir().expect("outer");
        let trust = outer.path().join("trust");
        std::fs::create_dir(&trust).expect("create trust");
        let outside = outer.path().join("outside");
        std::fs::create_dir(&outside).expect("create outside");

        symlink(&outside, trust.join(".vimeflow")).expect("plant symlink");

        let status = trust
            .join(".vimeflow")
            .join("sessions")
            .join("s")
            .join("status.json");
        let result = ensure_status_source_under_trust_root(&status, &trust);
        assert!(result.is_err(), "expected symlink-out to be rejected");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("escapes trust_root"),
            "expected escape diagnostic, got: {}",
            msg
        );
    }

    /// Trust root that doesn't exist: the function should fail in the
    /// initial canonicalize, before doing anything else. This is the
    /// caller-misuse path (start_for would have already validated cwd
    /// from PtyState, but defense in depth here).
    #[test]
    fn missing_trust_root_returns_err() {
        let outer = tempfile::tempdir().expect("outer");
        let nonexistent_root = outer.path().join("does-not-exist");
        let status = nonexistent_root.join("status.json");
        let result = ensure_status_source_under_trust_root(&status, &nonexistent_root);
        assert!(
            result.is_err(),
            "expected missing trust_root to be rejected"
        );
        assert!(
            result.unwrap_err().contains("trust_root not resolvable"),
            "expected resolvable diagnostic"
        );
    }

    /// PR #302 cycle 12 F2 — directly exercise the
    /// `canonicalize_and_verify_under_root` helper that backs BOTH the
    /// pre-check and the post-create recheck in
    /// `ensure_status_source_under_trust_root`. A single-threaded test
    /// can't inject a symlink between `create_dir_all` and the
    /// post-create canonicalize, but it CAN call the helper directly
    /// with a pre-existing symlink at the path → covering the same
    /// canonicalize + `starts_with` logic the post-create branch
    /// runs. Closes the "post-create recheck is untested" coverage
    /// gap the cycle-12 review flagged.
    #[cfg(unix)]
    #[test]
    fn canonicalize_and_verify_under_root_rejects_symlink_to_outside() {
        let outer = tempfile::tempdir().expect("outer");
        let trust = outer.path().join("trust");
        std::fs::create_dir(&trust).expect("create trust");
        let outside = outer.path().join("outside");
        std::fs::create_dir(&outside).expect("create outside");

        // Plant a symlink at trust/escape that points outside the root.
        let symlinked_parent = trust.join("escape");
        symlink(&outside, &symlinked_parent).expect("plant symlink");

        let canonical_root = std::fs::canonicalize(&trust).expect("canonical root");
        // Call the helper directly. canonicalize follows the symlink to
        // `outside`, which doesn't start_with `canonical_root` →
        // returns Err with the "escapes trust_root" diagnostic.
        let result =
            canonicalize_and_verify_under_root(&symlinked_parent, &canonical_root, "test phase");
        assert!(result.is_err(), "symlink-to-outside should be rejected");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("test phase escapes trust_root"),
            "expected context-labeled escape diagnostic, got: {}",
            msg
        );
    }

    /// Companion to the previous test — the helper accepts paths that
    /// canonicalize under the root (including paths that go through
    /// in-root symlinks). Pins that the helper isn't over-strict.
    #[cfg(unix)]
    #[test]
    fn canonicalize_and_verify_under_root_accepts_paths_under_root() {
        let outer = tempfile::tempdir().expect("outer");
        let trust = outer.path().join("trust");
        std::fs::create_dir(&trust).expect("create trust");
        let real_subdir = trust.join("real");
        std::fs::create_dir(&real_subdir).expect("create real");
        // In-root symlink pointing to an in-root target.
        let link = trust.join("link");
        symlink(&real_subdir, &link).expect("plant in-root symlink");

        let canonical_root = std::fs::canonicalize(&trust).expect("canonical root");
        canonicalize_and_verify_under_root(&link, &canonical_root, "test phase")
            .expect("in-root symlink should be accepted");
    }
}
