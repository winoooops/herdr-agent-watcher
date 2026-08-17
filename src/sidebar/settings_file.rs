//! Saving the settings panel's changes into `config.toml`.
//!
//! Keys, not the file. `Loaded` is not a lossless model of `config.toml`, so
//! serialising it back would delete values and every comment. `toml_edit`
//! changes the keys it is given and leaves the rest of the document exactly
//! as the operator wrote it.

use toml_edit::{value, DocumentMut, Item, Table};

use crate::sidebar::live::{Live, Setting};

fn table_and_key(setting: Setting) -> (&'static str, &'static str) {
    match setting {
        Setting::Sort => ("list", "sort"),
        Setting::Scope => ("list", "scope"),
        Setting::HideIdle => ("list", "hide_idle"),
        Setting::AutoExpand => ("cards", "auto_expand"),
        Setting::ToolCalls => ("cards", "tool_calls"),
        Setting::TraceLines => ("cards", "trace_lines"),
        Setting::Theme => ("appearance", "theme"),
        Setting::AgentMark => ("appearance", "agent_mark"),
        // The daemon's table. The sidebar's loader skips it entirely, which is
        // exactly why saving edits keys instead of rewriting the file.
        Setting::IntervalMs => ("daemon", "interval_ms"),
        Setting::PruneAfterDays => ("daemon", "prune_after_days"),
    }
}

fn item_for(live: &Live, setting: Setting) -> Item {
    match setting {
        Setting::HideIdle => value(live.hide_idle),
        Setting::TraceLines => value(i64::from(live.trace_lines)),
        Setting::IntervalMs => value(i64::from(live.interval_ms)),
        Setting::PruneAfterDays => value(i64::from(live.prune_after_days)),
        other => value(live.value(other)),
    }
}

/// The document with `dirty` applied, or why it will not be.
pub fn edit(current: &str, live: &Live, dirty: &[Setting]) -> Result<String, String> {
    let mut doc: DocumentMut = current
        .parse()
        .map_err(|error| format!("config.toml is not valid TOML: {error}"))?;

    for setting in dirty {
        let (table, key) = table_and_key(*setting);
        if doc.get(table).is_none() {
            // `doc[table][key] = …` would create an INLINE table at the top of
            // the file, which is not what a hand writes.
            let mut created = Table::new();
            created.set_implicit(false);
            doc.insert(table, Item::Table(created));
        }
        let Some(section) = doc.get_mut(table).and_then(Item::as_table_like_mut) else {
            return Err(format!(
                "[{table}] in config.toml is not a table, so this cannot write {table}.{key} \
                 without destroying it"
            ));
        };
        section.insert(key, item_for(live, *setting));
    }
    Ok(doc.to_string())
}

/// What the file looked like when the panel read it.
pub enum Expected {
    Missing,
    Contents(String),
}

pub fn save(path: &std::path::Path, expected: Expected, body: &str) -> Result<(), String> {
    save_hooked(path, expected, body, &|| {})
}

/// `after_temp` runs between writing the temp file and the check. Production
/// passes a no-op; the ordering tests pass a closure that creates or changes
/// the target, which an implementation checking too early would not see.
fn save_hooked(
    path: &std::path::Path,
    expected: Expected,
    body: &str,
    after_temp: &dyn Fn(),
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
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
        ".config.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&tmp, body).map_err(|error| format!("write {}: {error}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("chmod {}: {error}", tmp.display()))?;
    after_temp();

    // Immediately before the rename, and covering both states.
    let now = std::fs::read_to_string(&target);
    let stale = match (&expected, &now) {
        (Expected::Missing, Ok(_)) => true,
        (Expected::Missing, Err(error)) => error.kind() != std::io::ErrorKind::NotFound,
        (Expected::Contents(want), Ok(found)) => want != found,
        (Expected::Contents(_), Err(_)) => true,
    };
    if stale {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "{} changed while the panel was open; nothing was written, try again",
            target.display()
        ));
    }
    std::fs::rename(&tmp, &target)
        .map_err(|error| format!("rename to {}: {error}", target.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::live::{Live, Setting};

    const EXISTING: &str = "\
# how often the daemon reconciles
[daemon]
interval_ms = 3000

[agent.claude]
label = \"CC\"

[list]
sort = \"position\"
";

    fn live_with(sort: crate::sidebar::select::Sort, scope: crate::sidebar::config::Scope) -> Live {
        let mut live = Live::from(&crate::sidebar::config::Loaded::from_missing());
        live.sort = sort;
        live.scope = scope;
        live
    }

    #[test]
    fn only_the_changed_keys_are_written_and_they_reload() {
        use crate::sidebar::config::{Loaded, Scope};
        use crate::sidebar::select::Sort;
        let live = live_with(Sort::Smart, Scope::Workspace);
        let out = edit(EXISTING, &live, &[Setting::Sort, Setting::Scope]).expect("edit");

        // The whole document, not three substrings: a saver that reorders
        // tables, rewrites the comment or reflows every untouched line passes
        // a `contains` check.
        assert_eq!(
            out,
            "\
# how often the daemon reconciles
[daemon]
interval_ms = 3000

[agent.claude]
label = \"CC\"

[list]
sort = \"smart\"
scope = \"workspace\"
"
        );
        // And the values are actually the new ones -- byte preservation alone
        // passes a saver that writes the wrong value or drops an edit.
        let reloaded = Loaded::from_toml(&out);
        assert_eq!(reloaded.sort, Sort::Smart);
        assert_eq!(reloaded.scope, Scope::Workspace);
        assert_eq!(
            reloaded.status.problems, 0,
            "{:?}",
            reloaded.problem_details
        );
    }

    #[test]
    fn changing_prune_after_days_preserves_every_other_key_and_comment() {
        let mut live = live_with(
            crate::sidebar::select::Sort::Position,
            crate::sidebar::config::Scope::All,
        );
        live.prune_after_days = 30;
        let out = edit(EXISTING, &live, &[Setting::PruneAfterDays]).expect("edit");

        assert_eq!(
            out,
            "\
# how often the daemon reconciles
[daemon]
interval_ms = 3000
prune_after_days = 30

[agent.claude]
label = \"CC\"

[list]
sort = \"position\"
"
        );
        assert_eq!(
            crate::sidebar::config::Loaded::from_toml(&out).prune_after_days,
            30
        );
    }

    #[test]
    fn an_untouched_setting_is_not_written_at_all() {
        use crate::sidebar::select::Sort;
        let live = live_with(Sort::Smart, crate::sidebar::config::Scope::All);
        let out = edit(EXISTING, &live, &[Setting::Sort]).expect("edit");
        assert!(
            !out.contains("scope"),
            "an unchanged row must not be inserted: {out}"
        );
        assert!(!out.contains("trace_lines"), "{out}");
    }

    #[test]
    fn a_missing_table_is_created_as_a_section_not_an_inline_table() {
        use crate::sidebar::select::Sort;
        let live = live_with(Sort::Smart, crate::sidebar::config::Scope::All);
        let out = edit("[daemon]\ninterval_ms = 1\n", &live, &[Setting::TraceLines]).expect("edit");
        assert!(out.contains("[cards]"), "{out}");
        assert!(
            !out.contains("cards = {"),
            "an inline table at the top is not what a hand writes: {out}"
        );
    }

    #[test]
    fn an_empty_document_is_written_from_nothing() {
        use crate::sidebar::select::Sort;
        let live = live_with(Sort::Group, crate::sidebar::config::Scope::All);
        let out = edit("", &live, &[Setting::Sort]).expect("edit");
        assert_eq!(
            crate::sidebar::config::Loaded::from_toml(&out).sort,
            Sort::Group
        );
    }

    #[test]
    fn an_unparseable_document_is_refused() {
        let live = live_with(
            crate::sidebar::select::Sort::Smart,
            crate::sidebar::config::Scope::All,
        );
        assert!(edit("this is not toml {{{", &live, &[Setting::Sort]).is_err());
    }

    /// `Loaded` accepts these by ignoring them, so the document is valid and
    /// the panel is showing defaults. Writing into them would destroy the
    /// operator's value.
    #[test]
    fn a_section_that_is_not_a_table_is_refused() {
        let live = live_with(
            crate::sidebar::select::Sort::Smart,
            crate::sidebar::config::Scope::All,
        );
        for document in [
            "list = 3\n",
            "list = [{ sort = \"smart\" }]\n",
            "[[list]]\nsort = \"smart\"\n",
        ] {
            let error = edit(document, &live, &[Setting::Sort]).expect_err(document);
            assert!(error.contains("list"), "{error}");
        }
    }

    #[test]
    fn dotted_and_inline_tables_are_ordinary_tables() {
        let live = live_with(
            crate::sidebar::select::Sort::Smart,
            crate::sidebar::config::Scope::All,
        );
        for document in ["list.hide_idle = true\n", "list = { hide_idle = true }\n"] {
            let out = edit(document, &live, &[Setting::Sort]).expect(document);
            assert_eq!(
                crate::sidebar::config::Loaded::from_toml(&out).sort,
                crate::sidebar::select::Sort::Smart
            );
            assert!(
                crate::sidebar::config::Loaded::from_toml(&out).hide_idle,
                "{out}"
            );
        }
    }

    #[test]
    fn writing_creates_an_absent_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save(&path, Expected::Missing, "[list]\nsort = \"smart\"\n").expect("create");
        assert!(path.exists());
    }

    /// The conflict has to arrive AFTER the temp file is written, or an
    /// implementation that checks first and writes second passes.
    #[test]
    fn a_file_that_appears_after_the_temp_write_aborts_the_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let error = save_hooked(&path, Expected::Missing, "mine\n", &|| {
            std::fs::write(&path, "someone else got here first\n").unwrap();
        })
        .expect_err("the late create must be seen");
        assert!(error.contains("changed"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "someone else got here first\n"
        );
    }

    #[test]
    fn an_unreadable_existing_file_is_not_treated_as_absent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "theirs\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let outcome = save(&path, Expected::Missing, "mine\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            outcome.is_err(),
            "a file this process cannot read still exists"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs\n");
    }

    #[test]
    fn a_change_after_the_temp_write_aborts_the_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "a = 1\n").unwrap();
        let error = save_hooked(
            &path,
            Expected::Contents("a = 1\n".into()),
            "a = 2\n",
            &|| {
                std::fs::write(&path, "a = 99\n").unwrap();
            },
        )
        .expect_err("abort");
        assert!(error.contains("changed"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a = 99\n");
    }

    #[test]
    fn writing_preserves_the_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "a = 1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        save(&path, Expected::Contents("a = 1\n".into()), "a = 2\n").expect("save");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
