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

/// Removes exactly `appended`, or explains why it will not.
///
/// Byte equality identifies content, not the position it occupies: the same
/// bytes could sit inside a multi-line string, and if the real block had been
/// deleted by hand that string would be the only match. So the textual
/// removal is checked semantically before it is returned — the caller writes
/// text, but only text whose meaning is "one `keys.command` entry fewer".
pub(crate) fn remove_block(current: &str, appended: &str) -> Result<String, String> {
    let mut matches = current.match_indices(appended);
    let Some((at, _)) = matches.next() else {
        return Err(format!(
            "the managed block is not in this file exactly as it was written; \
             it was edited or already removed. Delete it by hand:\n{appended}"
        ));
    };
    if matches.next().is_some() {
        return Err("the managed block appears more than once; refusing to guess".into());
    }

    let mut out = String::with_capacity(current.len() - appended.len());
    out.push_str(&current[..at]);
    out.push_str(&current[at + appended.len()..]);

    // What the block itself says it is. `appended` is self-describing: it
    // parses to exactly one `keys.command` entry, and that entry is the one
    // whose disappearance we require.
    let ours = match command_entries(appended)?.as_slice() {
        [one] => one.clone(),
        other => {
            return Err(format!(
                "the recorded block describes {} keys.command entries, not one; \
                 the record is corrupt",
                other.len()
            ))
        }
    };

    let before = command_entries(current)?;
    let after = command_entries(&out)?;
    let mut gone = before.clone();
    for entry in &after {
        if let Some(index) = gone.iter().position(|e| e == entry) {
            gone.remove(index);
        }
    }
    match gone.as_slice() {
        [only] if *only == ours => Ok(out),
        [] => Err(
            "removing those bytes would not remove any keys.command entry — they are \
             inside a string or a comment, not the block; refusing"
                .into(),
        ),
        other => Err(format!(
            "removing those bytes would remove {} keys.command entries, or not the one \
             that was installed; refusing",
            other.len()
        )),
    }
}

/// Every `[[keys.command]]` entry, whole, in order. Whole rather than just the
/// `key`: an entry with our key but someone else's command is not ours.
fn command_entries(text: &str) -> Result<Vec<toml::Value>, String> {
    let parsed: toml::Value = text
        .parse()
        .map_err(|error| format!("not valid TOML: {error}"))?;
    Ok(parsed
        .get("keys")
        .and_then(|k| k.get("command"))
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// The same discipline as `bridge_settings::write_settings`, with the re-read
/// moved to just before the rename.
///
/// `expected` is an OPTIMISTIC check, not a compare-and-swap: an editor saving
/// between the read and the rename is still lost. `write_settings` leaves a
/// window of several syscalls (`bridge_settings.rs:62-91`); this narrows it to
/// one, and the remedy is the one that error already gives — say so and tell
/// the operator to run it again.
pub(crate) fn write_config(
    path: &Path,
    body: &str,
    expected: Option<&str>,
) -> Result<(), String> {
    write_config_hooked(path, body, expected, &|| {})
}

/// `after_temp` runs between writing the temp file and the re-read. Production
/// passes a no-op; the ordering test passes a closure that changes the target,
/// which an implementation re-reading too early would not see.
fn write_config_hooked(
    path: &Path,
    body: &str,
    expected: Option<&str>,
    after_temp: &dyn Fn(),
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    // Follow the symlink, so a config living in a dotfiles repo is updated
    // rather than replaced by a regular file.
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mode = std::fs::metadata(&target)
        .map(|meta| meta.permissions().mode() & 0o777)
        .unwrap_or(0o644);
    let dir = target
        .parent()
        .ok_or_else(|| format!("no parent for {}", target.display()))?;
    std::fs::create_dir_all(dir).map_err(|error| format!("create {}: {error}", dir.display()))?;

    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".herdr-config.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&tmp, body).map_err(|error| format!("write {}: {error}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("chmod {}: {error}", tmp.display()))?;
    after_temp();

    if let Some(expected) = expected {
        let now = std::fs::read_to_string(&target).unwrap_or_default();
        if now != expected {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!(
                "{} changed while we were editing it; nothing was written, run this again",
                target.display()
            ));
        }
    }
    std::fs::rename(&tmp, &target)
        .map_err(|error| format!("rename to {}: {error}", target.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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

    fn bound(original: &str) -> (String, String) {
        append_block(original, "prefix+a")
    }

    #[test]
    fn removal_restores_the_file_byte_for_byte() {
        for original in ["a = 1\n", "a = 1", "a = 1\r\n", ""] {
            let (text, appended) = bound(original);
            assert_eq!(
                remove_block(&text, &appended).unwrap(),
                original,
                "{original:?}"
            );
        }
    }

    #[test]
    fn a_copied_marker_that_is_not_our_bytes_is_refused() {
        let (_, appended) = bound("");
        // Same marker, same key, different description — an operator's own copy.
        let theirs = appended.replace("Open the Agent Watcher sidebar", "mine");
        assert!(remove_block(&theirs, &appended).is_err());
    }

    #[test]
    fn two_identical_blocks_are_refused() {
        let (text, appended) = bound("a = 1\n");
        let doubled = format!("{text}{appended}");
        assert!(remove_block(&doubled, &appended).is_err());
    }

    /// The reason the semantic check exists: the textual search finds exactly
    /// one match here, and it is inside a string the operator owns.
    #[test]
    fn a_match_inside_a_multiline_string_is_not_our_block() {
        let (_, appended) = bound("");
        let config = format!("note = \"\"\"\n{appended}\"\"\"\n");
        let error = remove_block(&config, &appended).expect_err("must refuse");
        assert!(error.contains("keys.command"), "{error}");
    }

    #[test]
    fn a_neighbouring_entry_survives() {
        let (text, appended) = bound("a = 1\n");
        let other =
            "\n[[keys.command]]\nkey = \"prefix+z\"\ntype = \"shell\"\ncommand = \"ls\"\n";
        let with_other = format!("{text}{other}");
        assert_eq!(
            remove_block(&with_other, &appended).unwrap(),
            format!("a = 1\n{other}"),
            "only ours comes out"
        );
    }

    /// A record whose block describes two entries is not something this ever
    /// wrote. Counting "one entry disappeared" would accept it and delete both.
    #[test]
    fn a_record_describing_two_entries_is_refused() {
        let (_, ours) = bound("");
        let theirs =
            "[[keys.command]]\nkey = \"prefix+z\"\ntype = \"shell\"\ncommand = \"ls\"\n";
        let two = format!("{ours}{theirs}");
        let config = format!("a = 1\n{two}");
        let error = remove_block(&config, &two).expect_err("must refuse");
        assert!(error.contains("record is corrupt"), "{error}");
    }

    #[test]
    fn the_mode_is_preserved_and_a_new_file_is_644() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("config.toml");
        std::fs::write(&existing, "a = 1\n").unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_config(&existing, "a = 2\n", None).unwrap();
        assert_eq!(
            std::fs::metadata(&existing)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let fresh = dir.path().join("fresh.toml");
        write_config(&fresh, "a = 1\n", None).unwrap();
        assert_eq!(
            std::fs::metadata(&fresh)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }

    #[test]
    fn a_symlink_is_followed_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        let link = dir.path().join("config.toml");
        std::fs::write(&real, "a = 1\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_config(&link, "a = 2\n", None).unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "a = 2\n");
    }

    /// The ordering §4 specifies, and the part of it that is testable. An
    /// implementation that re-reads BEFORE writing the temp file passes
    /// `a_changed_file_aborts_the_write` — it only fails this one, because the
    /// change lands after the point where it had already looked.
    #[test]
    fn the_reread_happens_after_the_temp_file_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "a = 1\n").unwrap();
        let error = write_config_hooked(&path, "a = 2\n", Some("a = 1\n"), &|| {
            std::fs::write(&path, "someone else got here first\n").unwrap();
        })
        .expect_err("the late change must be seen");
        assert!(error.contains("changed"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "someone else got here first\n"
        );
    }

    #[test]
    fn a_changed_file_aborts_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "a = 1\n").unwrap();
        let error =
            write_config(&path, "a = 2\n", Some("something else")).expect_err("abort");
        assert!(error.contains("changed"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a = 1\n");
    }
}
