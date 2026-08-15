//! Process-global kimi plan-usage consent + its durable flag.
//!
//! Consent to call `/usages` (which sends the api_key over the network) is a
//! single app-wide, user-granted, PERSISTED setting — not per-session. This
//! module owns the one in-memory flag every kimi locator reads and the JSON
//! file that survives restarts. It lives in the agent layer (not runtime) so
//! the kimi adapter reads it without a layering inversion, while
//! `BackendState` drives load-at-startup and persist-on-change.
//!
//! It is a process global rather than threaded state because consent is a
//! mutable, app-wide setting — not an attach-time fact — so it does not belong
//! in the per-attach `AttachContext` (which is documented as immutable).

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

// The live, app-wide consent flag. Default OFF: no `/usages` call is ever made
// until the user explicitly opts in.
static USAGE_CONSENT: AtomicBool = AtomicBool::new(false);

// Bumped by the refresh IPC. Each kimi supervisor tracks the last generation it
// acted on; a newer one forces one fetch (bypassing the turn debounce), so a UI
// "Retry" re-attempts a failed fetch without waiting for a new turn.
static REFRESH_GEN: AtomicU64 = AtomicU64::new(0);

/// Request a one-shot usage refresh on every live kimi session. The supervisors
/// pick it up on their next poll.
pub(crate) fn request_refresh() {
    REFRESH_GEN.fetch_add(1, Ordering::Relaxed);
}

/// The current refresh generation, compared by each supervisor against the last
/// it acted on.
pub(crate) fn refresh_gen() -> u64 {
    REFRESH_GEN.load(Ordering::Relaxed)
}

/// The persisted shape: a single opt-in bool.
#[derive(Serialize, Deserialize)]
struct ConsentFile {
    enabled: bool,
}

/// Whether the user has consented to network plan-usage fetches. Read by the
/// kimi locator before any `/usages` call and by the consent getter IPC.
pub(crate) fn usage_consent_enabled() -> bool {
    USAGE_CONSENT.load(Ordering::Relaxed)
}

/// Overwrite the in-memory flag only (no persistence).
fn set_in_memory(enabled: bool) {
    USAGE_CONSENT.store(enabled, Ordering::Relaxed);
}

/// Load the persisted flag into memory at startup. A missing or unreadable
/// file leaves the default (OFF) — consent must be an explicit, durable
/// opt-in, so anything but a stored `true` reads as not-consented.
pub(crate) fn load_into_memory(path: &Path) {
    let enabled = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<ConsentFile>(&raw).ok())
        .map(|file| file.enabled)
        .unwrap_or(false);
    set_in_memory(enabled);
}

/// Persist the flag durably and update memory.
///
/// A **revoke** (`enabled == false`) clears memory IMMEDIATELY — once the user
/// says stop, the key must never be sent again even if the disk op fails — then
/// REMOVES the file. Deleting is durable where writing `false` is not: it can't
/// fail on a full disk (the realistic revoke failure), and it uses the same
/// directory/permissions as the enable that created the file, so a delete error
/// is near-impossible. A missing file loads as OFF, so the opt-out survives a
/// restart rather than being silently re-enabled by a stale `true` file.
///
/// An **enable** writes the file, then flips memory ON only after the write
/// lands, so a non-durable opt-in is never half-applied. Both return the disk
/// error so the caller can surface a non-durable choice.
pub(crate) fn set_and_persist(path: &Path, enabled: bool) -> std::io::Result<()> {
    if !enabled {
        set_in_memory(false);

        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        };
    }

    let body =
        serde_json::to_string(&ConsentFile { enabled: true }).map_err(std::io::Error::other)?;
    std::fs::write(path, body)?;
    set_in_memory(true);
    Ok(())
}

/// Serialize the few tests that mutate the process-global flag so they don't
/// race each other (mirrors the env-guard pattern used elsewhere).
#[cfg(test)]
pub(crate) fn test_serial_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Flip the in-memory flag directly in tests (no persistence). Callers must
/// hold [`test_serial_guard`] and restore the default before releasing it.
#[cfg(test)]
pub(crate) fn set_for_test(enabled: bool) {
    set_in_memory(enabled);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_reads_persisted_true_and_defaults_off() {
        let _guard = test_serial_guard();
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("kimi-usage-consent.json");

        // Missing file → default OFF.
        set_in_memory(true);
        load_into_memory(&path);
        assert!(!usage_consent_enabled(), "missing file must read as OFF");

        // A persisted `true` loads as ON.
        std::fs::write(&path, r#"{"enabled":true}"#).expect("write");
        load_into_memory(&path);
        assert!(usage_consent_enabled(), "persisted true must load ON");

        set_in_memory(false);
    }

    #[test]
    fn set_and_persist_round_trips_through_file_and_memory() {
        let _guard = test_serial_guard();
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("kimi-usage-consent.json");

        set_and_persist(&path, true).expect("persist true");
        assert!(usage_consent_enabled());
        // A fresh load from the same file recovers the value.
        set_in_memory(false);
        load_into_memory(&path);
        assert!(usage_consent_enabled(), "value survives via the file");

        set_and_persist(&path, false).expect("persist false");
        assert!(!usage_consent_enabled());
    }

    #[test]
    fn revoke_removes_the_file_so_it_loads_off() {
        let _guard = test_serial_guard();
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("kimi-usage-consent.json");

        set_and_persist(&path, true).expect("enable");
        assert!(path.exists(), "enable writes the file");

        set_and_persist(&path, false).expect("revoke");
        assert!(!path.exists(), "revoke removes the file (durable opt-out)");
        assert!(!usage_consent_enabled(), "revoke clears memory");

        // A revoke on an already-absent file is a no-op, not an error.
        set_and_persist(&path, false).expect("revoke when already absent");

        // A fresh load with the file gone reads as OFF — no stale re-enable.
        set_in_memory(true);
        load_into_memory(&path);
        assert!(!usage_consent_enabled(), "missing file loads as OFF");

        set_for_test(false);
    }

    #[test]
    fn request_refresh_bumps_the_generation() {
        let before = refresh_gen();
        request_refresh();
        assert!(refresh_gen() > before, "a refresh request advances the gen");
    }
}
