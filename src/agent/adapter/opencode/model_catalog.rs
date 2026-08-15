//! opencode model catalog — best-effort context-window lookup.
//!
//! The bridge event stream carries the active model as `{providerID, modelID}`
//! but NOT its context-window size: opencode resolves that internally from the
//! models.dev database it caches on disk. So to fill
//! [`StatusSnapshot::context_window_size`] the adapter reads that same cache,
//! `${XDG_CACHE_HOME:-$HOME/.cache}/opencode/models.json`, keyed
//! `models[providerID].models[modelID].limit.context`.
//!
//! This is the SAME category of opencode-owned path the adapter already manages
//! in [`super::install`] (which writes the bridge plugin into
//! `~/.config/opencode/plugins`); it is a read-only metadata cache, not session
//! state, so it does not reintroduce the SQLite/session coupling the bridge
//! design avoids.
//!
//! **Best-effort, never fatal.** A missing / unreadable / unparseable cache, or
//! a model the cache does not list, yields [`OPENCODE_CONTEXT_WINDOW_SIZE`]
//! (`0` = unknown) and the frontend renders the bar without a denominator
//! ("window unknown") — exactly the pre-lookup behavior. The first non-empty
//! catalog is cached for the process (opencode refreshes it rarely and a model's
//! context window is static per version), so lookups after that are a hashmap
//! hit while early refresh races can still recover.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;

use super::install::{home_dir, non_empty_env};
use super::types::OPENCODE_CONTEXT_WINDOW_SIZE;

/// Explicit override for the models.json path (tests + non-standard installs).
const MODELS_JSON_ENV: &str = "AGENT_WATCHER_OPENCODE_MODELS_JSON";

/// `(providerID, modelID) -> context-window size in tokens`.
type Catalog = HashMap<(String, String), u64>;

/// Resolve opencode's models.dev cache path:
/// `$AGENT_WATCHER_OPENCODE_MODELS_JSON` (override) else
/// `${XDG_CACHE_HOME:-$HOME/.cache}/opencode/models.json`. Note this uses the
/// XDG **cache** home, distinct from the bridge dir's XDG **data** home.
fn models_json_path() -> PathBuf {
    if let Some(override_path) = non_empty_env(MODELS_JSON_ENV) {
        return PathBuf::from(override_path);
    }

    let cache_home = match non_empty_env("XDG_CACHE_HOME") {
        Some(xdg) => PathBuf::from(xdg),
        None => home_dir().join(".cache"),
    };
    cache_home.join("opencode").join("models.json")
}

/// Flatten opencode's `models.json` into `(providerID, modelID) -> context`.
///
/// Shape: `{ <providerID>: { "models": { <modelID>: { "limit": { "context": N
/// } } } } }`. Every layer is probed defensively — a provider with no `models`
/// object, a model with no `limit.context`, or a non-integer context are all
/// skipped rather than erroring, so a partially-shaped cache still yields the
/// entries it can.
fn parse_catalog(bytes: &[u8]) -> Catalog {
    let mut catalog = Catalog::new();

    let Ok(root) = serde_json::from_slice::<Value>(bytes) else {
        return catalog;
    };
    let Some(providers) = root.as_object() else {
        return catalog;
    };

    for (provider_id, provider) in providers {
        let Some(models) = provider.get("models").and_then(Value::as_object) else {
            continue;
        };
        for (model_id, model) in models {
            if let Some(context) = model
                .get("limit")
                .and_then(|limit| limit.get("context"))
                .and_then(Value::as_u64)
            {
                catalog.insert((provider_id.clone(), model_id.clone()), context);
            }
        }
    }

    catalog
}

/// Look up `(providerID, modelID)` in a catalog, returning the unknown sentinel
/// (`0`) on a miss. Split out from [`context_window`] so the lookup is testable
/// against a hand-built map without the process-wide cache.
fn lookup(catalog: &Catalog, provider_id: &str, model_id: &str) -> u64 {
    catalog
        .get(&(provider_id.to_string(), model_id.to_string()))
        .copied()
        .unwrap_or(OPENCODE_CONTEXT_WINDOW_SIZE)
}

/// Load a catalog from disk. Missing, unreadable, malformed, or empty caches
/// return `None` so callers can retry after opencode finishes refreshing
/// `models.json`.
fn read_catalog(path: &Path) -> Option<Catalog> {
    let bytes = std::fs::read(path).ok()?;
    let catalog = parse_catalog(&bytes);
    (!catalog.is_empty()).then_some(catalog)
}

/// Catalog cache, populated only after a non-empty `models.json` load succeeds.
fn cached_catalog<'a>(cache: &'a OnceLock<Catalog>, path: &Path) -> Option<&'a Catalog> {
    if let Some(catalog) = cache.get() {
        return Some(catalog);
    }

    let catalog = read_catalog(path)?;
    let _ = cache.set(catalog);
    cache.get()
}

/// Process-wide catalog, loaded from the resolved `models.json` once a
/// successful non-empty parse is available. Earlier read/parse misses are not
/// cached so first-run and refresh-race sessions can recover without restart.
fn catalog() -> Option<&'static Catalog> {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    cached_catalog(&CATALOG, &models_json_path())
}

/// Best-effort context-window size (tokens) for `(providerID, modelID)` from
/// opencode's models.dev cache. Returns [`OPENCODE_CONTEXT_WINDOW_SIZE`]
/// (`0` = unknown) when the cache is absent or the model is not listed.
///
/// The signature matches the `Fn(&str, &str) -> u64` resolver
/// [`super::parser::parse_bridge_snapshot`] injects, so the production decode
/// path passes this function directly while tests pass a deterministic stub.
pub(crate) fn context_window(provider_id: &str, model_id: &str) -> u64 {
    catalog()
        .map(|catalog| lookup(catalog, provider_id, model_id))
        .unwrap_or(OPENCODE_CONTEXT_WINDOW_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::adapter::opencode::OpencodeEnvGuard;

    const SAMPLE_MODELS_JSON: &str = r#"{
        "opencode": {
            "models": {
                "deepseek-v4-flash-free": { "limit": { "context": 200000, "output": 128000 } },
                "no-limit-model": { "name": "missing limit" }
            }
        },
        "anthropic": {
            "models": {
                "claude-sonnet-4": { "limit": { "context": 1000000 } }
            }
        },
        "broken-provider": { "models": "not-an-object" }
    }"#;

    #[test]
    fn parse_catalog_extracts_provider_scoped_context_windows() {
        let catalog = parse_catalog(SAMPLE_MODELS_JSON.as_bytes());
        assert_eq!(
            catalog.get(&("opencode".into(), "deepseek-v4-flash-free".into())),
            Some(&200_000)
        );
        // The SAME modelID under a different provider is a distinct entry — this
        // is why the lookup is keyed by (provider, model), not model alone.
        assert_eq!(
            catalog.get(&("anthropic".into(), "claude-sonnet-4".into())),
            Some(&1_000_000)
        );
    }

    #[test]
    fn parse_catalog_skips_models_without_a_context_limit() {
        let catalog = parse_catalog(SAMPLE_MODELS_JSON.as_bytes());
        assert!(catalog
            .get(&("opencode".into(), "no-limit-model".into()))
            .is_none());
    }

    #[test]
    fn parse_catalog_tolerates_garbage_and_bad_shapes() {
        assert!(parse_catalog(b"not json at all").is_empty());
        assert!(parse_catalog(b"[1,2,3]").is_empty());
        // A provider whose `models` is not an object is skipped, not fatal.
        let catalog = parse_catalog(SAMPLE_MODELS_JSON.as_bytes());
        assert!(catalog
            .get(&("broken-provider".into(), "anything".into()))
            .is_none());
    }

    #[test]
    fn lookup_returns_unknown_sentinel_on_miss() {
        let catalog = parse_catalog(SAMPLE_MODELS_JSON.as_bytes());
        assert_eq!(
            lookup(&catalog, "opencode", "deepseek-v4-flash-free"),
            200_000
        );
        assert_eq!(
            lookup(&catalog, "opencode", "no-such-model"),
            OPENCODE_CONTEXT_WINDOW_SIZE
        );
        assert_eq!(lookup(&Catalog::new(), "opencode", "anything"), 0);
    }

    #[test]
    fn cached_catalog_retries_after_initial_missing_cache() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("models.json");
        let cache = OnceLock::new();

        assert!(cached_catalog(&cache, &path).is_none());
        std::fs::write(&path, SAMPLE_MODELS_JSON).expect("write models cache");

        let catalog = cached_catalog(&cache, &path).expect("catalog after cache appears");
        assert_eq!(
            lookup(catalog, "opencode", "deepseek-v4-flash-free"),
            200_000
        );
    }

    #[test]
    fn cached_catalog_retries_after_empty_or_malformed_cache() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("models.json");
        let cache = OnceLock::new();

        std::fs::write(&path, r#"{"opencode":{"models":{}}}"#).expect("write empty cache");
        assert!(cached_catalog(&cache, &path).is_none());

        std::fs::write(&path, "not json").expect("write malformed cache");
        assert!(cached_catalog(&cache, &path).is_none());

        std::fs::write(&path, SAMPLE_MODELS_JSON).expect("write valid cache");
        let catalog = cached_catalog(&cache, &path).expect("catalog after valid cache");
        assert_eq!(lookup(catalog, "anthropic", "claude-sonnet-4"), 1_000_000);
    }

    #[test]
    fn models_json_path_honors_explicit_override() {
        let _guard = OpencodeEnvGuard::acquire();
        std::env::set_var(MODELS_JSON_ENV, "/custom/models.json");
        assert_eq!(models_json_path(), PathBuf::from("/custom/models.json"));
    }

    #[test]
    fn models_json_path_uses_xdg_cache_home_when_no_override() {
        let _guard = OpencodeEnvGuard::acquire();
        std::env::remove_var(MODELS_JSON_ENV);
        std::env::set_var("XDG_CACHE_HOME", "/xdg/cache");
        assert_eq!(
            models_json_path(),
            PathBuf::from("/xdg/cache/opencode/models.json")
        );
    }

    #[test]
    fn models_json_path_falls_back_to_home_cache() {
        let _guard = OpencodeEnvGuard::acquire();
        std::env::remove_var(MODELS_JSON_ENV);
        std::env::remove_var("XDG_CACHE_HOME");
        assert!(models_json_path().ends_with(".cache/opencode/models.json"));
    }
}
