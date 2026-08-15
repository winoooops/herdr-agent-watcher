//! Sanitize agent-emitted title strings per spec §3.2.1.
//!
//! Server-side rule: replace C0/DEL with space; collapse whitespace;
//! trim; truncate <=200 bytes on a UTF-8 char boundary; return None
//! when the sanitized result is empty.

const CAP_BYTES: usize = 200;

/// Returns `Some(sanitized)` when the result is non-empty, `None` otherwise.
pub fn sanitize_title(raw: &str) -> Option<String> {
    let mut without_controls = String::with_capacity(raw.len());
    for ch in raw.chars() {
        let code = ch as u32;
        if code <= 0x1f || code == 0x7f {
            without_controls.push(' ');
        } else {
            without_controls.push(ch);
        }
    }

    let mut sanitized = without_controls
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if sanitized.is_empty() {
        return None;
    }

    if sanitized.len() > CAP_BYTES {
        let mut cut = CAP_BYTES;
        while cut > 0 && !sanitized.is_char_boundary(cut) {
            cut -= 1;
        }
        sanitized.truncate(cut);
        sanitized = sanitized.trim_end().to_string();
    }

    Some(sanitized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_normal_title_returns_unchanged() {
        assert_eq!(
            sanitize_title("Fix CI pipeline"),
            Some("Fix CI pipeline".to_string())
        );
    }

    #[test]
    fn sanitize_with_newline_replaces_with_space() {
        assert_eq!(
            sanitize_title("Line1\nLine2"),
            Some("Line1 Line2".to_string())
        );
    }

    #[test]
    fn sanitize_with_tab_and_cr_collapses() {
        assert_eq!(sanitize_title("a\t\r\nb"), Some("a b".to_string()));
    }

    #[test]
    fn sanitize_empty_returns_none() {
        assert_eq!(sanitize_title(""), None);
    }

    #[test]
    fn sanitize_whitespace_only_returns_none() {
        assert_eq!(sanitize_title("   \t\n"), None);
    }

    #[test]
    fn sanitize_over_200_bytes_truncates_on_char_boundary() {
        let raw = "𝕏".repeat(51);
        let result = sanitize_title(&raw).expect("non-empty");

        assert!(result.len() <= CAP_BYTES, "len was {}", result.len());
        assert!(result.is_char_boundary(result.len()));
    }

    #[test]
    fn sanitize_trims_space_left_by_truncation() {
        let raw = format!("{} b", "a".repeat(CAP_BYTES - 1));
        let result = sanitize_title(&raw).expect("non-empty");

        assert_eq!(result, "a".repeat(CAP_BYTES - 1));
    }
}
