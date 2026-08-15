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

/// Every chord Herdr would answer to, given its defaults and the operator's
/// config. One overlay, not two sets: an operator value REPLACES the default
/// for that action, and an empty one releases it entirely (verified against
/// Herdr 0.8.0 — `toggle_sidebar = ""` is `config: ok` and frees `prefix+b`).
pub(crate) fn occupied(default_config: &str, operator_config: &str) -> BTreeSet<String> {
    let mut by_action = defaults(default_config);
    by_action.remove("prefix"); // names the prefix itself, not a chord

    let parsed: toml::Value = operator_config
        .parse()
        .unwrap_or(toml::Value::Table(toml::map::Map::new()));
    let keys = parsed.get("keys").and_then(toml::Value::as_table);

    let mut out = BTreeSet::new();
    if let Some(keys) = keys {
        for (action, value) in keys {
            match value {
                toml::Value::String(s) if s.is_empty() => {
                    by_action.remove(action.as_str());
                }
                toml::Value::String(s) => {
                    by_action.insert(action.clone(), s.clone());
                }
                toml::Value::Array(items) => {
                    by_action.remove(action.as_str());
                    for chord in items.iter().filter_map(toml::Value::as_str) {
                        out.extend(expand(chord));
                    }
                }
                _ => {}
            }
        }
    }
    for expression in by_action.values() {
        out.extend(expand(expression));
    }

    // `[[keys.command]]` is an array of tables under the same `keys` table.
    if let Some(commands) = keys
        .and_then(|k| k.get("command"))
        .and_then(toml::Value::as_array)
    {
        for chord in commands
            .iter()
            .filter_map(|entry| entry.get("key")?.as_str())
        {
            out.extend(expand(chord));
        }
    }
    out
}

/// The action name holding `chord`, for an error message that tells the
/// operator what they would have to give up.
pub(crate) fn holder(
    default_config: &str,
    operator_config: &str,
    chord: &str,
) -> Option<String> {
    let mut by_action = defaults(default_config);
    by_action.remove("prefix");
    if let Ok(parsed) = operator_config.parse::<toml::Value>() {
        if let Some(keys) = parsed.get("keys").and_then(toml::Value::as_table) {
            for (action, value) in keys {
                match value {
                    toml::Value::String(s) if s.is_empty() => {
                        by_action.remove(action.as_str());
                    }
                    toml::Value::String(s) => {
                        by_action.insert(action.clone(), s.clone());
                    }
                    toml::Value::Array(items) => {
                        by_action.remove(action.as_str());
                        if items
                            .iter()
                            .filter_map(toml::Value::as_str)
                            .any(|c| expand(c).iter().any(|k| k == chord))
                        {
                            return Some(action.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    by_action
        .into_iter()
        .find(|(_, expression)| expand(expression).iter().any(|k| k == chord))
        .map(|(action, _)| action)
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

    fn occupied_for(config: &str) -> BTreeSet<String> {
        occupied(FIXTURE, config)
    }

    #[test]
    fn defaults_alone_occupy_the_expected_chords() {
        let o = occupied_for("");
        assert!(o.contains("prefix+b")); // toggle_sidebar
        assert!(o.contains("prefix+minus")); // split_horizontal
        assert!(o.contains("prefix+1")); // switch_tab range
        assert!(o.contains("prefix+9"));
        assert!(!o.contains("prefix+a"));
    }

    #[test]
    fn an_empty_operator_value_releases_a_default() {
        let o = occupied_for("[keys]\ntoggle_sidebar = \"\"\n");
        assert!(!o.contains("prefix+b"), "an empty value unbinds it");
    }

    #[test]
    fn an_operator_scalar_replaces_the_default_it_overrides() {
        let o = occupied_for("[keys]\ntoggle_sidebar = \"prefix+a\"\n");
        assert!(o.contains("prefix+a"), "the new key is taken");
        assert!(!o.contains("prefix+b"), "and the old one is free");
    }

    #[test]
    fn an_operator_array_takes_every_member() {
        let o = occupied_for("[keys]\nfocus_pane_left = [\"prefix+h\", \"ctrl+shift+h\"]\n");
        assert!(o.contains("prefix+h"));
        assert!(o.contains("ctrl+shift+h"));
    }

    #[test]
    fn existing_command_bindings_are_occupied_too() {
        let o = occupied_for(
            "[[keys.command]]\nkey = \"prefix+a\"\ntype = \"shell\"\ncommand = \"ls\"\n",
        );
        assert!(o.contains("prefix+a"));
    }

    #[test]
    fn who_holds_it_names_the_action() {
        assert_eq!(
            holder(FIXTURE, "", "prefix+b").as_deref(),
            Some("toggle_sidebar")
        );
        assert_eq!(
            holder(FIXTURE, "", "prefix+1").as_deref(),
            Some("switch_tab")
        );
        assert_eq!(holder(FIXTURE, "", "prefix+a"), None);
    }
}
