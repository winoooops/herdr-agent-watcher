//! The one canonical agent-id table (§2.5, §4.4). Portable: config validation
//! and the runtime registry both consume it, so they cannot drift.

use crate::sidebar::style::AgentAppearance;

pub const CANONICAL_IDS: &[&str] = &["claude", "codex", "kimi", "opencode"];

/// Every id herdr may report, including aliases.
pub const ACCEPTED_IDS: &[&str] = &["claude", "claude-code", "codex", "kimi", "opencode"];

pub fn canonical(id: &str) -> Option<&'static str> {
    match id {
        "claude" | "claude-code" => Some("claude"),
        "codex" => Some("codex"),
        "kimi" => Some("kimi"),
        "opencode" => Some("opencode"),
        _ => None,
    }
}

/// Built-in brand appearance. `color` overrides in config replace `rgb` only —
/// the ANSI fallback stays fixed so two agents cannot collide (§2.5).
pub fn appearance(canonical_id: &str) -> Option<AgentAppearance> {
    let (label, rgb, ansi) = match canonical_id {
        "claude" => ("CLAUDE", (0xd9, 0x77, 0x57), 1),
        "codex" => ("CODEX", (0x10, 0xa3, 0x7f), 2),
        "kimi" => ("KIMI", (0x6d, 0x5a, 0xe6), 5),
        "opencode" => ("OPENCODE", (0xe5, 0xb5, 0x67), 3),
        _ => return None,
    };
    Some(AgentAppearance {
        label: label.to_string(),
        rgb,
        ansi,
        symbol: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_normalise_and_unknown_ids_are_rejected() {
        assert_eq!(canonical("claude-code"), Some("claude"));
        assert_eq!(canonical("claude"), Some("claude"));
        assert_eq!(canonical("codex"), Some("codex"));
        assert_eq!(canonical("claud"), None);
    }

    #[test]
    fn every_canonical_id_has_a_distinct_ansi_fallback() {
        let mut seen = std::collections::HashSet::new();
        for id in CANONICAL_IDS {
            let a = appearance(id).expect("known id");
            assert!(seen.insert(a.ansi), "ANSI fallback {} collides", a.ansi);
            assert!((1..=6).contains(&a.ansi), "fallbacks use ANSI 1-6 only");
        }
    }

    #[test]
    fn accepted_ids_include_every_alias_the_registry_reports() {
        for id in ["claude", "claude-code", "codex", "kimi", "opencode"] {
            assert!(canonical(id).is_some(), "{id} must be accepted");
        }
    }
}
