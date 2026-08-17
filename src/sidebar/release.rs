//! What GitHub says the newest release is, and whether it is newer than this
//! build. Deliberately free of IO so the comparison -- the part that can be
//! wrong in a way nobody notices -- is testable without a network.

/// The version a tag names. Releases here are tagged `v0.1.5`; accepting the
/// bare form too costs nothing and means a retag cannot silently compare a
/// `v` against a digit.
pub fn version_of_tag(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

/// The numeric components of a version, ignoring anything that is not a digit.
///
/// `0.2.0-rc1` reads as `[0, 2, 0]`, which makes it equal to `0.2.0` rather
/// than before it. That is wrong in general and harmless here: GitHub's
/// "latest release" excludes pre-releases, so one never reaches this.
fn parts(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

/// Whether `latest` is a later release than `current`.
///
/// Compares component by component rather than as text: `0.1.10` is after
/// `0.1.9`, which string ordering gets backwards.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let (a, b) = (
        parts(version_of_tag(latest)),
        parts(version_of_tag(current)),
    );
    let len = a.len().max(b.len());
    for i in 0..len {
        // A missing component is zero: `0.2` and `0.2.0` are the same release.
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

/// The tag in GitHub's release JSON.
///
/// Takes the whole body rather than a parsed value so the failure is one
/// message the panel can print, whatever went wrong on the way.
pub fn tag_from_release_json(body: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("unreadable reply: {error}"))?;
    // A rate-limited or not-found reply is valid JSON with a `message` and no
    // `tag_name`; saying "no tag_name" for it would hide what GitHub said.
    if let Some(message) = value.get("message").and_then(|m| m.as_str()) {
        if value.get("tag_name").is_none() {
            return Err(format!("github: {message}"));
        }
    }
    value
        .get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| "no tag_name in the reply".to_string())
}

/// The repository these releases come from. The same one
/// `scripts/fetch-or-build.sh` fetches assets from; a mismatch here would
/// offer an upgrade the build step cannot install.
pub const REPO: &str = "winoooops/herdr-agent-watcher";

/// How the plugin was installed, which decides whether an upgrade is even a
/// thing that can happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Installed from GitHub. `herdr plugin install` can replace it.
    Github,
    /// Linked from a working directory. Herdr refuses to install over a link,
    /// and it should: the tree belongs to whoever is editing it.
    Linked(String),
    /// Herdr did not say. Better than guessing "github" and offering an
    /// upgrade that fails.
    Unknown,
}

/// What a check found out. One value so the panel renders from a single
/// match rather than a spread of optional fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Checking,
    Ready {
        latest: String,
        source: Source,
    },
    /// The check itself failed -- offline, rate limited, unreadable reply.
    Failed(String),
    Upgrading,
    /// Whatever `herdr plugin install` said, verbatim.
    Finished(Result<String, String>),
}

impl Check {
    /// Whether the upgrade key does anything in this state, and the version
    /// it would install.
    pub fn upgradable(&self, current: &str) -> Option<&str> {
        match self {
            Check::Ready {
                latest,
                source: Source::Github,
            } if is_newer(latest, current) => Some(latest),
            _ => None,
        }
    }
}

/// Everything below talks to the network or to `herdr`, so it only exists in
/// the runtime build. The comparison above stays testable without either.
#[cfg(all(feature = "runtime", unix))]
mod io {
    use super::*;
    use std::sync::mpsc::{channel, Receiver};
    use std::time::Duration;

    const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
    const CALL_TIMEOUT: Duration = Duration::from_secs(10);

    /// Start a check and hand back the end of a channel to poll.
    ///
    /// A thread rather than a call: the panel is drawn by the same loop that
    /// reads the keyboard, and ten seconds of a blocked socket would freeze
    /// both. Nothing here runs unless a key asked for it.
    pub fn check() -> Receiver<Check> {
        let (sender, receiver) = channel();
        std::thread::spawn(move || {
            let outcome = match latest_tag() {
                Ok(latest) => Check::Ready {
                    latest,
                    source: installed_source(),
                },
                Err(error) => Check::Failed(error),
            };
            let _ = sender.send(outcome);
        });
        receiver
    }

    /// Run the upgrade and hand back its result, herdr's own words either way.
    pub fn upgrade(tag: &str) -> Receiver<Check> {
        let (sender, receiver) = channel();
        let tag = tag.to_string();
        std::thread::spawn(move || {
            let _ = sender.send(Check::Finished(install(&tag)));
        });
        receiver
    }

    fn latest_tag() -> Result<String, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout(CALL_TIMEOUT)
            .build();
        let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
        let response = agent
            .get(&url)
            // GitHub rejects a request with no User-Agent outright.
            .set(
                "User-Agent",
                &format!("herdr-agent-watcher/{}", env!("CARGO_PKG_VERSION")),
            )
            .set("Accept", "application/vnd.github+json")
            .call();
        let body = match response {
            Ok(response) => response
                .into_string()
                .map_err(|_| "could not read the reply".to_string())?,
            // A status error still carries GitHub's JSON, which says more than
            // the code does -- "rate limit exceeded" rather than "http 403".
            Err(ureq::Error::Status(code, response)) => match response.into_string() {
                Ok(body) => return tag_from_release_json(&body).or(Err(format!("http {code}"))),
                Err(_) => return Err(format!("http {code}")),
            },
            Err(ureq::Error::Transport(transport)) => {
                return Err(format!("cannot reach github: {}", transport.kind()))
            }
        };
        tag_from_release_json(&body)
    }

    /// Ask herdr how this plugin was installed. Anything unexpected reads as
    /// `Unknown`, which offers nothing, rather than as `Github`, which would
    /// offer an upgrade that cannot run.
    fn installed_source() -> Source {
        let Ok(herdr) = std::env::var("HERDR_BIN_PATH") else {
            return Source::Unknown;
        };
        let id =
            std::env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| "herdr-agent-watcher".to_string());
        let Ok(out) = std::process::Command::new(herdr)
            .args(["plugin", "list", "--json"])
            .output()
        else {
            return Source::Unknown;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
            return Source::Unknown;
        };
        let plugin = value
            .get("result")
            .and_then(|r| r.get("plugins"))
            .and_then(|p| p.as_array())
            .and_then(|plugins| {
                plugins
                    .iter()
                    .find(|p| p.get("plugin_id").and_then(|i| i.as_str()) == Some(id.as_str()))
            });
        match plugin.and_then(|p| p.get("source")) {
            Some(source) => match source.get("kind").and_then(|k| k.as_str()) {
                Some("local") => Source::Linked(
                    plugin
                        .and_then(|p| p.get("plugin_root"))
                        .and_then(|r| r.as_str())
                        .unwrap_or("a working directory")
                        .to_string(),
                ),
                Some("github") => Source::Github,
                _ => Source::Unknown,
            },
            None => Source::Unknown,
        }
    }

    fn install(tag: &str) -> Result<String, String> {
        let herdr =
            std::env::var("HERDR_BIN_PATH").map_err(|_| "HERDR_BIN_PATH is not set".to_string())?;
        let out = std::process::Command::new(herdr)
            .args(["plugin", "install", REPO, "--ref", tag, "--yes"])
            .output()
            .map_err(|error| format!("cannot run herdr: {error}"))?;
        let say = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim().to_string();
        if out.status.success() {
            // The daemon is replaced by the install; this sidebar is not, and
            // cannot be -- it is executing the file that was just swapped.
            Ok(format!("{} — reopen this sidebar", say(&out.stdout)))
        } else {
            let said = say(&out.stderr);
            Err(if said.is_empty() {
                say(&out.stdout)
            } else {
                said
            })
        }
    }
}

#[cfg(all(feature = "runtime", unix))]
pub use io::{check, upgrade};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_is_a_version_with_or_without_its_v() {
        assert_eq!(version_of_tag("v0.1.5"), "0.1.5");
        assert_eq!(version_of_tag("0.1.5"), "0.1.5");
    }

    /// The case text ordering gets backwards, and the reason this compares
    /// numbers: `"0.1.10" < "0.1.9"` as strings.
    #[test]
    fn ten_comes_after_nine() {
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.9", "0.1.10"));
    }

    #[test]
    fn the_same_version_is_not_newer_however_it_is_written() {
        assert!(!is_newer("0.1.5", "0.1.5"));
        assert!(!is_newer("v0.1.5", "0.1.5"));
        assert!(!is_newer("0.2", "0.2.0"), "a missing component is zero");
        assert!(!is_newer("0.2.0", "0.2"));
    }

    #[test]
    fn a_later_major_or_minor_wins_over_a_larger_patch() {
        assert!(is_newer("0.2.0", "0.1.99"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.99", "0.2.0"));
    }

    #[test]
    fn a_tag_is_read_from_the_release_body() {
        let body = r#"{"tag_name":"v0.1.6","name":"0.1.6","draft":false}"#;
        assert_eq!(tag_from_release_json(body).unwrap(), "v0.1.6");
    }

    /// GitHub answers rate limits and unknown repos with 200-shaped JSON that
    /// has a `message`. Reporting "no tag_name" would throw away the only
    /// sentence that says what to do about it.
    #[test]
    fn githubs_own_complaint_is_what_gets_reported() {
        let body = r#"{"message":"API rate limit exceeded","documentation_url":"..."}"#;
        let error = tag_from_release_json(body).expect_err("not a release");
        assert!(error.contains("rate limit"), "{error}");
    }

    #[test]
    fn a_reply_that_is_not_json_says_so() {
        let error = tag_from_release_json("<html>502</html>").expect_err("not json");
        assert!(error.contains("unreadable"), "{error}");
    }
    /// The upgrade key is only live when an upgrade would actually work. A
    /// linked tree is the case that made this a decision rather than a
    /// version comparison: herdr refuses to install over one, so offering it
    /// would produce an error the reader cannot act on.
    #[test]
    fn only_a_newer_release_on_a_github_install_can_be_upgraded_to() {
        let github = |latest: &str| Check::Ready {
            latest: latest.into(),
            source: Source::Github,
        };
        assert_eq!(github("0.1.6").upgradable("0.1.5"), Some("0.1.6"));
        assert_eq!(github("0.1.5").upgradable("0.1.5"), None, "same version");
        assert_eq!(github("0.1.4").upgradable("0.1.5"), None, "older release");

        for source in [Source::Linked("/tmp/tree".into()), Source::Unknown] {
            let check = Check::Ready {
                latest: "0.1.6".into(),
                source: source.clone(),
            };
            assert_eq!(
                check.upgradable("0.1.5"),
                None,
                "{source:?} cannot be installed over"
            );
        }

        for check in [
            Check::Checking,
            Check::Failed("offline".into()),
            Check::Upgrading,
            Check::Finished(Ok("done".into())),
        ] {
            assert_eq!(check.upgradable("0.1.5"), None, "{check:?} has no version");
        }
    }
}
