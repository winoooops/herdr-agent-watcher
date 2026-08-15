//! Installing and removing the sidebar keybinding in Herdr's own config.
//!
//! Text in, text out. Parsing the operator's `config.toml` and writing it back
//! would reorder tables, drop comments and reflow values across a file this
//! plugin has no business rewriting — the Claude bridge needed
//! `serde_json`'s `preserve_order` for the same reason, and TOML has no
//! equivalent that keeps comments.

use std::path::{Path, PathBuf};

pub(crate) const MARKER: &str =
    "# herdr-agent-watcher: managed keybinding. Remove with `unbind-sidebar-key`.";

/// What was written, so removal never has to guess. Same role as
/// `bridge_settings::Sidecar`.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Record {
    pub config_path: PathBuf,
    /// The exact bytes appended, including the separator. Removing this string
    /// restores the file to what it was.
    pub appended: String,
    pub created_file: bool,
    pub key: String,
}

/// `\r\n` only when the file is unambiguously CRLF; a file we create is `\n`.
fn line_ending(existing: &str) -> &'static str {
    if existing.contains("\r\n") && !existing.replace("\r\n", "").contains('\n') {
        "\r\n"
    } else {
        "\n"
    }
}

/// Returns the whole new file and, separately, the exact bytes added — the
/// second is what goes into the record, and removing it is what `unbind` does.
pub(crate) fn append_block(existing: &str, key: &str) -> (String, String) {
    let nl = line_ending(existing);
    let mut appended = String::new();
    // A file ending mid-line would otherwise absorb the marker into its last
    // line; a file ending in a comment would comment the block out.
    if !existing.is_empty() && !existing.ends_with(nl) {
        appended.push_str(nl);
    }
    if !existing.is_empty() {
        appended.push_str(nl);
    }
    for line in [
        MARKER,
        "[[keys.command]]",
        &format!("key = \"{key}\""),
        "type = \"plugin_action\"",
        "command = \"open-sidebar\"",
        "description = \"Open the Agent Watcher sidebar\"",
    ] {
        appended.push_str(line);
        appended.push_str(nl);
    }
    (format!("{existing}{appended}"), appended)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_is_appended_after_one_blank_line() {
        let (text, appended) = append_block("a = 1\n", "prefix+a");
        assert!(text.starts_with("a = 1\n"));
        assert!(text.ends_with(&appended));
        assert!(appended.starts_with('\n'), "{appended:?}");
        assert!(appended.contains("key = \"prefix+a\""));
        assert!(appended.contains("[[keys.command]]"));
    }

    #[test]
    fn a_file_without_a_final_newline_gets_one_first() {
        let (text, appended) = append_block("a = 1", "prefix+a");
        assert!(text.starts_with("a = 1\n"), "{text:?}");
        // and the added newline belongs to `appended`, so removing it restores
        // the original exactly
        assert_eq!(text.replace(&appended, ""), "a = 1");
    }

    #[test]
    fn a_crlf_file_stays_crlf() {
        let (text, appended) = append_block("a = 1\r\n", "prefix+a");
        // Strip every CRLF pair; a bare LF surviving that is a mixed ending.
        // `appended.contains("\r\n")` alone is true even when the rest of the
        // block uses bare newlines.
        assert!(
            !appended.replace("\r\n", "").contains('\n'),
            "mixed line endings: {appended:?}"
        );
        assert!(!text.replace("\r\n", "").contains('\n'), "{text:?}");
        assert_eq!(text.replace(&appended, ""), "a = 1\r\n");
    }

    #[test]
    fn an_empty_file_gets_no_leading_blank_line_run() {
        let (text, appended) = append_block("", "prefix+a");
        assert_eq!(text, appended);
        assert_eq!(text.replace(&appended, ""), "");
    }
}
