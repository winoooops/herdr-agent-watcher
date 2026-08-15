//! Resolve the kimi managed-provider credential and fetch `/usages`.
//!
//! The only outbound network call in the backend. kimi writes no plan-usage
//! data to disk, so — unlike claude/codex which read local files — the
//! 5-hour / weekly limits come from `GET ${base}/usages` with the user's
//! api_key. The api_key + base_url live in `<kimi_home>/config.toml` under
//! `[providers."managed:kimi-code"]`, alongside sibling `search` / `fetch`
//! providers with their OWN keys, so resolution is section-scoped. Response
//! PII (`userId` / `region` / `membership`) and the token are never logged.

use std::path::Path;
use std::time::Duration;

use crate::agent::types::RateLimits;

use super::usage::parse_usage_payload;

// Bound the call so a hung kimi API never stalls the background thread: the
// last cached value simply stays until the next turn retries.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
const FALLBACK_VERSION: &str = "0.0.0";

// The managed-provider table key in config.toml. The file also holds
// `providers."<other>"` entries (web search / fetch) with different keys.
const MANAGED_PROVIDER: &str = "managed:kimi-code";

/// The resolved managed-provider endpoint: where to GET and what to send.
pub(super) struct UsageEndpoint {
    base_url: String,
    token: String,
}

/// Resolve `(base_url, token)` for the managed provider. `base_url` comes from
/// `<home>/config.toml`'s `[providers."managed:kimi-code"]` section. The token
/// is the section's `api_key` (API-key installs) or, failing that, the OAuth
/// bearer token under `<home>/credentials/<profile>.json` (OAuth installs leave
/// `api_key` out). `None` — with a warning, not a silent skip — when the
/// section is present but no token resolves; `None` (no warning) when there is
/// no managed section / config at all.
pub(super) fn resolve_usage_endpoint(home: &Path) -> Option<UsageEndpoint> {
    let raw = std::fs::read_to_string(home.join("config.toml")).ok()?;
    let root: toml::Value = toml::from_str(&raw).ok()?;
    let provider = root.get("providers")?.get(MANAGED_PROVIDER)?;
    let base_url = provider.get("base_url")?.as_str()?.trim().to_string();
    if base_url.is_empty() {
        return None;
    }
    let Some(token) = config_api_key(provider).or_else(|| oauth_token(home)) else {
        log::warn!(
            "kimi usage: managed provider has no api_key and no resolvable OAuth token; \
             plan-usage fetch skipped",
        );
        return None;
    };
    Some(UsageEndpoint { base_url, token })
}

/// The managed provider's `api_key` from config.toml, when present + non-empty.
fn config_api_key(provider: &toml::Value) -> Option<String> {
    let key = provider.get("api_key")?.as_str()?.trim();
    (!key.is_empty()).then(|| key.to_string())
}

/// OAuth bearer token from `<home>/credentials/<profile>.json`, where
/// `<profile>` is the managed provider name without its `managed:` prefix
/// (`kimi-code`). Lenient on the token field name across kimi versions.
fn oauth_token(home: &Path) -> Option<String> {
    let profile = MANAGED_PROVIDER
        .strip_prefix("managed:")
        .unwrap_or(MANAGED_PROVIDER);
    let path = home.join("credentials").join(format!("{profile}.json"));
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    for field in ["access_token", "accessToken", "token", "bearer"] {
        if let Some(token) = json.get(field).and_then(|value| value.as_str()) {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    None
}

/// Resolve the endpoint, GET `/usages`, and map the response to
/// `RateLimits`. `None` on any failure (missing creds, network, auth, parse)
/// — usage is best-effort and never blocks the local status path.
pub(super) fn fetch_rate_limits(home: &Path, version: &str) -> Option<RateLimits> {
    let endpoint = resolve_usage_endpoint(home)?;
    let user_agent_version = usage_user_agent_version(home, version);
    match http_get_usages(&endpoint, &user_agent_version) {
        Ok(body) => parse_usage_payload(&body),
        Err(reason) => {
            log::warn!("kimi usage fetch failed: {reason}");
            None
        }
    }
}

fn usage_user_agent_version(home: &Path, transcript_version: &str) -> String {
    clean_version(transcript_version)
        .or_else(|| version_from_update_metadata(home))
        .unwrap_or_else(|| FALLBACK_VERSION.to_string())
}

fn clean_version(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    raw.split_whitespace()
        .find_map(|token| {
            let token = token.trim_start_matches('v').trim_matches(|ch: char| {
                !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
            });
            token.chars().any(|ch| ch.is_ascii_digit()).then_some(token)
        })
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

fn version_from_update_metadata(home: &Path) -> Option<String> {
    let install = read_json(&home.join("updates").join("install.json"));
    if let Some(version) = install.as_ref().and_then(|root| {
        string_at(root, &["active", "version"])
            .or_else(|| string_at(root, &["lastSuccess", "version"]))
            .or_else(|| string_at(root, &["version"]))
    }) {
        if let Some(version) = clean_version(version) {
            return Some(version);
        }
    }

    let latest = read_json(&home.join("updates").join("latest.json"));
    latest.as_ref().and_then(|root| {
        string_at(root, &["latest"])
            .or_else(|| string_at(root, &["manifest", "version"]))
            .and_then(clean_version)
    })
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn string_at<'a>(value: &'a serde_json::Value, path: &[&str]) -> Option<&'a str> {
    path.iter()
        .try_fold(value, |current, key| current.get(*key))
        .and_then(serde_json::Value::as_str)
}

/// `${base_url}/usages` — the endpoint is a `.../v1` base, plural `usages`.
fn usages_url(base_url: &str) -> String {
    format!("{}/usages", base_url.trim_end_matches('/'))
}

/// GET `/usages` with the Bearer api_key and the mandatory `kimi-code/<ver>`
/// User-Agent (a non-kimi UA is rejected with `access_terminated_error`).
/// Errors carry only a status code or transport kind — never the token or
/// the PII-bearing body.
fn http_get_usages(endpoint: &UsageEndpoint, version: &str) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout(CALL_TIMEOUT)
        .build();
    let response = agent
        .get(&usages_url(&endpoint.base_url))
        .set("Authorization", &format!("Bearer {}", endpoint.token))
        .set("User-Agent", &format!("kimi-code/{version}"))
        .call();
    match response {
        Ok(response) => response
            .into_string()
            .map_err(|_| "response body read failed".to_string()),
        Err(ureq::Error::Status(code, _)) => Err(format!("http {code}")),
        Err(ureq::Error::Transport(transport)) => Err(format!("transport {:?}", transport.kind())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A config.toml shaped like the real one: the managed provider sits
    // alongside `search` / `fetch` providers, each with its OWN api_key, so a
    // naive first-key scan would grab the wrong credential.
    const MULTI_PROVIDER: &str = r#"
[providers."web:search"]
base_url = "https://api.kimi.com/coding/v1/search"
api_key = "WRONG-search-key"

[providers."web:fetch"]
base_url = "https://api.kimi.com/coding/v1/fetch"
api_key = "WRONG-fetch-key"

[providers."managed:kimi-code"]
base_url = "https://api.kimi.com/coding/v1"
api_key = "RIGHT-managed-key"

[agent]
provider = "managed:kimi-code"
"#;

    fn write_config(contents: &str) -> tempfile::TempDir {
        let home = tempfile::tempdir().expect("home");
        std::fs::write(home.path().join("config.toml"), contents).expect("write config");
        home
    }

    #[test]
    fn resolves_managed_provider_over_sibling_providers() {
        let home = write_config(MULTI_PROVIDER);
        let endpoint = resolve_usage_endpoint(home.path()).expect("managed provider resolves");
        assert_eq!(endpoint.base_url, "https://api.kimi.com/coding/v1");
        assert_eq!(endpoint.token, "RIGHT-managed-key");
    }

    #[test]
    fn missing_managed_section_yields_none() {
        let home = write_config(
            r#"
[providers."web:search"]
base_url = "https://api.kimi.com/coding/v1/search"
api_key = "only-search"
"#,
        );
        assert!(resolve_usage_endpoint(home.path()).is_none());
    }

    #[test]
    fn empty_api_key_yields_none() {
        let home = write_config(
            r#"
[providers."managed:kimi-code"]
base_url = "https://api.kimi.com/coding/v1"
api_key = ""
"#,
        );
        assert!(resolve_usage_endpoint(home.path()).is_none());
    }

    #[test]
    fn absent_config_file_yields_none() {
        let home = tempfile::tempdir().expect("home");
        assert!(resolve_usage_endpoint(home.path()).is_none());
    }

    fn write_oauth_credential(home: &std::path::Path, contents: &str) {
        let creds = home.join("credentials");
        std::fs::create_dir_all(&creds).expect("creds dir");
        std::fs::write(creds.join("kimi-code.json"), contents).expect("creds file");
    }

    #[test]
    fn falls_back_to_oauth_token_when_config_has_no_api_key() {
        // OAuth install: managed section carries base_url but no api_key; the
        // bearer token lives in credentials/kimi-code.json.
        let home = write_config(
            r#"
[providers."managed:kimi-code"]
base_url = "https://api.kimi.com/coding/v1"
"#,
        );
        write_oauth_credential(home.path(), r#"{"access_token":"oauth-xyz"}"#);
        let endpoint = resolve_usage_endpoint(home.path()).expect("oauth resolves");
        assert_eq!(endpoint.base_url, "https://api.kimi.com/coding/v1");
        assert_eq!(endpoint.token, "oauth-xyz");
    }

    #[test]
    fn prefers_config_api_key_over_oauth_credential() {
        let home = write_config(MULTI_PROVIDER);
        write_oauth_credential(home.path(), r#"{"access_token":"oauth-should-not-win"}"#);
        let endpoint = resolve_usage_endpoint(home.path()).expect("api key resolves");
        assert_eq!(endpoint.token, "RIGHT-managed-key");
    }

    #[test]
    fn usages_url_appends_plural_segment_without_double_slash() {
        assert_eq!(
            usages_url("https://api.kimi.com/coding/v1"),
            "https://api.kimi.com/coding/v1/usages"
        );
        assert_eq!(
            usages_url("https://api.kimi.com/coding/v1/"),
            "https://api.kimi.com/coding/v1/usages"
        );
    }

    #[test]
    fn user_agent_version_prefers_transcript_version() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join("updates")).expect("updates");
        std::fs::write(
            home.path().join("updates").join("install.json"),
            r#"{"lastSuccess":{"version":"0.15.0"}}"#,
        )
        .expect("install metadata");

        assert_eq!(usage_user_agent_version(home.path(), " 0.14.2 "), "0.14.2");
    }

    #[test]
    fn user_agent_version_falls_back_to_install_metadata() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join("updates")).expect("updates");
        std::fs::write(
            home.path().join("updates").join("install.json"),
            r#"{"lastSuccess":{"version":"0.15.0"}}"#,
        )
        .expect("install metadata");

        assert_eq!(usage_user_agent_version(home.path(), ""), "0.15.0");
    }

    #[test]
    fn user_agent_version_falls_back_to_latest_metadata() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join("updates")).expect("updates");
        std::fs::write(
            home.path().join("updates").join("latest.json"),
            r#"{"latest":"0.16.0","manifest":{"version":"0.16.0"}}"#,
        )
        .expect("latest metadata");

        assert_eq!(usage_user_agent_version(home.path(), ""), "0.16.0");
    }

    #[test]
    fn user_agent_version_keeps_kimi_shaped_default_when_everything_is_missing() {
        let home = tempfile::tempdir().expect("home");

        assert_eq!(usage_user_agent_version(home.path(), ""), FALLBACK_VERSION);
    }
}
