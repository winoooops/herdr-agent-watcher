//! Which key chords Herdr already answers to. Pure: everything here takes text
//! and returns a decision, so the rules in §3.3 of the design are a table of
//! inputs rather than a fixture tree.

use std::collections::{BTreeMap, BTreeSet};

/// Herdr's defaults, by action name, parsed from `herdr --default-config`
/// rather than frozen here: Herdr adds bindings, and a stale list refuses keys
/// that are free.
///
/// Every default is commented out in that output, so the lines this reads look
/// like `# toggle_sidebar = "prefix+b"`. Lines assigning `""` are options that
/// are unset by default and hold no key.
pub(crate) fn defaults(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut in_keys = false;
    for line in text.lines() {
        let line = line.trim_start();
        // Section headers may or may not be commented -- `[keys]` is bare,
        // `# [keys.indexed]` and `# [worktrees]` are not. Missing that keeps
        // the parser inside [keys] to the end of the file, where it collects
        // `# directory = "~/.herdr/worktrees"` and the commented
        // `[[keys.command]]` example's `key`, `width` and `height` as chords.
        let header = line.strip_prefix("# ").unwrap_or(line);
        if header.starts_with('[') {
            in_keys = header.starts_with("[keys]");
            continue;
        }
        if !in_keys {
            continue;
        }
        let Some(body) = line.strip_prefix("# ") else {
            continue;
        };
        let Some((name, rest)) = body.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || name.contains(char::is_whitespace) {
            continue;
        }
        // `"prefix+b"` or `"prefix+b" # trailing comment`
        let Some(open) = rest.find('"') else {
            continue;
        };
        let Some(close) = rest[open + 1..].find('"') else {
            continue;
        };
        let value = &rest[open + 1..open + 1 + close];
        if value.is_empty() {
            continue;
        }
        out.insert(name.to_string(), value.to_string());
    }
    out
}

/// `prefix+1..9` is nine chords, not one. An exact-string comparison against
/// that value misses every one of them.
pub(crate) fn expand(expression: &str) -> Vec<String> {
    let Some((head, tail)) = expression.rsplit_once("..") else {
        return vec![expression.to_string()];
    };
    let (prefix, first) = match head.rfind(|c: char| !c.is_ascii_digit()) {
        Some(index) => head.split_at(index + 1),
        None => ("", head),
    };
    let (Ok(first), Ok(last)) = (first.parse::<u32>(), tail.parse::<u32>()) else {
        return vec![expression.to_string()];
    };
    if last < first {
        return vec![expression.to_string()];
    }
    (first..=last).map(|n| format!("{prefix}{n}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/herdr-default-config.toml");

    #[test]
    fn defaults_are_read_by_action_name() {
        let d = defaults(FIXTURE);
        assert_eq!(
            d.get("toggle_sidebar").map(String::as_str),
            Some("prefix+b")
        );
        assert_eq!(
            d.get("split_horizontal").map(String::as_str),
            Some("prefix+minus")
        );
        assert_eq!(
            d.get("switch_tab").map(String::as_str),
            Some("prefix+1..9")
        );
        assert_eq!(d.get("prefix").map(String::as_str), Some("ctrl+b"));
    }

    #[test]
    fn an_optional_default_is_absent_not_empty() {
        // `# previous_workspace = "" # optional, unset by default`
        assert_eq!(defaults(FIXTURE).get("previous_workspace"), None);
    }

    #[test]
    fn only_the_keys_section_is_read() {
        // `[worktrees]` carries `# directory = "~/.herdr/worktrees"`, which is
        // not a chord and must not occupy one.
        assert_eq!(defaults(FIXTURE).get("directory"), None);
        // `# [keys.indexed]` is a COMMENTED header for a different table.
        assert_eq!(defaults(FIXTURE).get("tabs"), None);
        // and the commented `# [[keys.command]]` example must not contribute
        // `prefix+alt+g`, `80%`, or a `command` "action".
        assert_eq!(defaults(FIXTURE).get("command"), None);
        assert_eq!(defaults(FIXTURE).get("width"), None);
        assert!(
            !defaults(FIXTURE).values().any(|v| v == "prefix+alt+g"),
            "the commented example is documentation, not a binding"
        );
    }

    #[test]
    fn a_range_expands_to_every_member() {
        assert_eq!(
            expand("prefix+1..9"),
            (1..=9).map(|n| format!("prefix+{n}")).collect::<Vec<_>>()
        );
        assert_eq!(expand("prefix+b"), vec!["prefix+b".to_string()]);
        assert_eq!(
            expand("prefix+alt+1..9"),
            (1..=9)
                .map(|n| format!("prefix+alt+{n}"))
                .collect::<Vec<_>>()
        );
    }
}
