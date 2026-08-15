use super::sanitiser::sanitize_for_command;

const MAX_PREVIEW_LEN: usize = 120;

pub fn build_command_preview(stripped_tokens: &[String]) -> String {
    let joined = stripped_tokens.join(" ");
    // Use the command-specific sanitiser — narrower than the output one
    // so benign env vars like NODE_ENV=test stay visible in the header.
    let sanitized = sanitize_for_command(&joined);
    truncate(&sanitized, MAX_PREVIEW_LEN)
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let end = s
            .char_indices()
            .nth(max_len.saturating_sub(3))
            .map_or(s.len(), |(i, _)| i);
        format!("{}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec_of(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn joins_tokens() {
        assert_eq!(
            build_command_preview(&vec_of(&["vitest", "run", "src/foo.test.ts"])),
            "vitest run src/foo.test.ts"
        );
    }

    #[test]
    fn applies_sanitiser() {
        let p = build_command_preview(&vec_of(&[
            "vitest",
            "--key",
            "sk_live_1234567890abcdef1234",
        ]));
        assert!(!p.contains("sk_live_1234567890abcdef1234"));
        assert!(p.contains("[REDACTED]"));
    }

    #[test]
    fn truncates_long_input() {
        let long = "a".repeat(200);
        let preview = build_command_preview(&vec_of(&[&long]));
        assert!(preview.chars().count() <= 120);
        assert!(preview.ends_with("..."));
    }
}
