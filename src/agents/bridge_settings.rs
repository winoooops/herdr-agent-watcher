//! Lossless installation and removal in Claude's user settings.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

const EVENTS: [(&str, &str, Option<&str>); 5] = [
    ("UserPromptSubmit", "name-only", None),
    ("Stop", "append", None),
    ("StopFailure", "append", None),
    ("PermissionRequest", "append", None),
    ("PreToolUse", "append", Some("AskUserQuestion")),
];

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Sidecar {
    pub settings_path: PathBuf,
    pub previous_status_line: Option<Value>,
    pub installed_status_line: Value,
    pub installed_hooks: Vec<(String, Value)>,
    pub created_event_keys: Vec<String>,
    pub created_hooks_object: bool,
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn hook_entry(attention: &str, event: &str, mode: &str, matcher: Option<&str>) -> Value {
    let hooks = json!([{
        "type": "command",
        "command": format!("{} {} {}", shell_quote(attention), shell_quote(event), shell_quote(mode)),
    }]);
    match matcher {
        Some(matcher) => json!({ "matcher": matcher, "hooks": hooks }),
        None => json!({ "hooks": hooks }),
    }
}

fn read_object(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let value: Value = serde_json::from_str(&text)
                .map_err(|error| format!("{} is not valid JSON: {error}", path.display()))?;
            if !value.is_object() {
                return Err(format!("{} is not a JSON object", path.display()));
            }
            Ok(value)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

pub(crate) fn write_settings(
    path: &Path,
    value: &Value,
    expected: Option<&str>,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(expected) = expected {
        let now = std::fs::read_to_string(&target)
            .map_err(|error| format!("cannot re-read {}: {error}", target.display()))?;
        if now != expected {
            return Err(format!(
                "{} changed while we were editing it; nothing was written, run this again",
                target.display()
            ));
        }
    }
    let mode = std::fs::metadata(&target)
        .map(|meta| meta.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    let dir = target
        .parent()
        .ok_or_else(|| format!("no parent for {}", target.display()))?;
    std::fs::create_dir_all(dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = dir.join(format!(
        ".settings.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let body =
        serde_json::to_string_pretty(value).map_err(|error| format!("serialize: {error}"))?;
    std::fs::write(&tmp, format!("{body}\n"))
        .map_err(|error| format!("write {}: {error}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("chmod {}: {error}", tmp.display()))?;
    std::fs::rename(&tmp, &target)
        .map_err(|error| format!("rename to {}: {error}", target.display()))
}

pub(crate) fn enable(
    settings_path: &Path,
    sidecar_path: &Path,
    statusline: &str,
    attention: &str,
) -> Result<(), String> {
    let before = std::fs::read_to_string(settings_path).ok();
    let mut settings = read_object(settings_path)?;
    let installed_status_line = json!({
        "type": "command", "command": shell_quote(statusline), "refreshInterval": 5,
    });
    let prior = match std::fs::read_to_string(sidecar_path) {
        Ok(text) => Some(
            serde_json::from_str::<Sidecar>(&text)
                .map_err(|error| format!("record unreadable: {error}"))?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("cannot read {}: {error}", sidecar_path.display())),
    };
    let already = settings["statusLine"]["command"]
        .as_str()
        .is_some_and(|command| command.contains(statusline));
    if already {
        return prior
            .map(|_| ())
            .ok_or_else(|| "bridge is installed but its install record is missing".to_string());
    }
    let previous_status_line = settings.get("statusLine").cloned();
    settings["statusLine"] = installed_status_line.clone();

    let created_hooks_object = settings.get("hooks").is_none();
    let root = settings.as_object_mut().expect("object checked above");
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let table = hooks
        .as_object_mut()
        .ok_or_else(|| "hooks is not an object".to_string())?;
    let mut installed_hooks = Vec::new();
    let mut created_event_keys = Vec::new();
    for (event, mode, matcher) in EVENTS {
        let entry = hook_entry(attention, event, mode, matcher);
        let created_key = !table.contains_key(event);
        let array = table
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("hooks.{event} is not an array"))?;
        if array.iter().any(|existing| existing == &entry) {
            continue;
        }
        array.push(entry.clone());
        installed_hooks.push((event.to_string(), entry));
        if created_key {
            created_event_keys.push(event.to_string());
        }
    }

    let sidecar = Sidecar {
        settings_path: settings_path.to_path_buf(),
        previous_status_line,
        installed_status_line,
        installed_hooks,
        created_event_keys,
        created_hooks_object,
    };
    let record =
        serde_json::to_string_pretty(&sidecar).map_err(|error| format!("record: {error}"))?;
    std::fs::write(sidecar_path, record)
        .map_err(|error| format!("write {}: {error}", sidecar_path.display()))?;
    if let Err(error) = write_settings(settings_path, &settings, before.as_deref()) {
        let _ = std::fs::remove_file(sidecar_path);
        return Err(error);
    }
    Ok(())
}

pub(crate) fn disable(settings_path: &Path, sidecar_path: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(sidecar_path).map_err(|error| {
        format!(
            "no install record at {}: {error} — refusing to guess which statusLine is ours",
            sidecar_path.display()
        )
    })?;
    let sidecar: Sidecar =
        serde_json::from_str(&text).map_err(|error| format!("record unreadable: {error}"))?;
    let before = std::fs::read_to_string(settings_path).ok();
    let mut settings = read_object(settings_path)?;

    if settings.get("statusLine") == Some(&sidecar.installed_status_line) {
        match sidecar.previous_status_line {
            Some(previous) => settings["statusLine"] = previous,
            None => {
                settings
                    .as_object_mut()
                    .expect("object checked above")
                    .remove("statusLine");
            }
        }
    }
    if let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) {
        for (event, entry) in &sidecar.installed_hooks {
            if let Some(array) = hooks.get_mut(event).and_then(Value::as_array_mut) {
                array.retain(|existing| existing != entry);
            }
        }
        for event in &sidecar.created_event_keys {
            if hooks
                .get(event)
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                hooks.remove(event);
            }
        }
    }
    if sidecar.created_hooks_object
        && settings["hooks"]
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    {
        settings
            .as_object_mut()
            .expect("object checked above")
            .remove("hooks");
    }
    write_settings(settings_path, &settings, before.as_deref())?;
    std::fs::remove_file(sidecar_path)
        .map_err(|error| format!("remove {}: {error}", sidecar_path.display()))
}

pub(crate) fn user_settings_path() -> Result<PathBuf, String> {
    let root = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
        .ok_or_else(|| "cannot resolve the Claude config directory".to_string())?;
    Ok(root.join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(dir: &Path, body: Value) -> PathBuf {
        let path = dir.join("settings.json");
        std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap()).unwrap();
        path
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn enable_disable_is_json_equivalent_and_restores_unrelated_values() {
        let dir = tempfile::tempdir().unwrap();
        let original = json!({"model":"opus","theme":"dark"});
        let path = seed(dir.path(), original.clone());
        let sidecar = dir.path().join("bridge-install.json");
        enable(&path, &sidecar, "/bin/statusline.sh", "/bin/attention.sh").unwrap();
        disable(&path, &sidecar).unwrap();
        assert_eq!(read(&path), original);
    }

    #[test]
    fn existing_status_line_and_user_hooks_are_restored_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let mine = json!({"hooks":[{"type":"command","command":"echo mine"}]});
        let original = json!({
            "statusLine":{"type":"command","command":"jq -r '.model'","refreshInterval":3},
            "hooks":{"SessionStart":[mine.clone()],"Stop":[mine]}
        });
        let path = seed(dir.path(), original.clone());
        let sidecar = dir.path().join("bridge-install.json");
        enable(&path, &sidecar, "/bin/statusline.sh", "/bin/attention.sh").unwrap();
        disable(&path, &sidecar).unwrap();
        assert_eq!(read(&path), original);
    }

    #[test]
    fn identical_preexisting_hook_is_not_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let identical = hook_entry("/bin/attention.sh", "Stop", "append", None);
        let original = json!({"hooks":{"Stop":[identical]}});
        let path = seed(dir.path(), original.clone());
        let sidecar = dir.path().join("bridge-install.json");
        enable(&path, &sidecar, "/bin/statusline.sh", "/bin/attention.sh").unwrap();
        assert_eq!(read(&path)["hooks"]["Stop"].as_array().unwrap().len(), 1);
        disable(&path, &sidecar).unwrap();
        assert_eq!(read(&path), original);
    }

    #[test]
    fn all_five_events_are_installed() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed(dir.path(), json!({}));
        let sidecar = dir.path().join("bridge-install.json");
        enable(&path, &sidecar, "/bin/statusline.sh", "/bin/attention.sh").unwrap();
        let settings = read(&path);
        for event in [
            "UserPromptSubmit",
            "Stop",
            "StopFailure",
            "PermissionRequest",
            "PreToolUse",
        ] {
            assert!(settings["hooks"][event].is_array(), "{event}");
        }
        assert_eq!(
            settings["hooks"]["PreToolUse"][0]["matcher"],
            "AskUserQuestion"
        );
    }

    #[test]
    fn enabling_twice_is_a_no_op_and_disable_restores_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let original = json!({"statusLine":{"type":"command","command":"echo user"}});
        let path = seed(dir.path(), original.clone());
        let sidecar = dir.path().join("bridge-install.json");
        enable(&path, &sidecar, "/bin/statusline.sh", "/bin/attention.sh").unwrap();
        let once = std::fs::read_to_string(&path).unwrap();
        enable(&path, &sidecar, "/bin/statusline.sh", "/bin/attention.sh").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), once);
        disable(&path, &sidecar).unwrap();
        assert_eq!(read(&path), original);
    }

    #[test]
    fn changed_status_line_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed(dir.path(), json!({}));
        let sidecar = dir.path().join("bridge-install.json");
        enable(&path, &sidecar, "/bin/statusline.sh", "/bin/attention.sh").unwrap();
        let mut current = read(&path);
        current["statusLine"]["command"] = json!("the user changed this");
        std::fs::write(&path, serde_json::to_string_pretty(&current).unwrap()).unwrap();
        disable(&path, &sidecar).unwrap();
        assert_eq!(
            read(&path)["statusLine"]["command"],
            "the user changed this"
        );
    }

    #[test]
    fn malformed_non_object_and_unreadable_documents_are_hard_errors() {
        for body in ["{ not json", "[1,2,3]", "\"a string\""] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("settings.json");
            std::fs::write(&path, body).unwrap();
            assert!(enable(&path, &dir.path().join("record"), "/s", "/a").is_err());
            assert_eq!(std::fs::read_to_string(path).unwrap(), body);
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = seed(dir.path(), json!({"model":"opus"}));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = enable(&path, &dir.path().join("record"), "/s", "/a");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(result.is_err());
        assert_eq!(read(&path), json!({"model":"opus"}));
    }

    #[test]
    fn changed_before_rename_aborts() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed(dir.path(), json!({"model":"opus"}));
        assert!(write_settings(&path, &json!({"model":"ours"}), Some("stale")).is_err());
        assert_eq!(read(&path), json!({"model":"opus"}));
    }

    #[test]
    fn missing_file_mode_existing_mode_and_symlink_are_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.json");
        let sidecar = dir.path().join("one.record");
        enable(&missing, &sidecar, "/s", "/a").unwrap();
        assert_eq!(
            std::fs::metadata(&missing).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let real = seed(dir.path(), json!({"model":"opus"}));
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.path().join("linked.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        enable(&link, &dir.path().join("two.record"), "/s", "/a").unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn record_is_written_first_and_disable_never_guesses() {
        let dir = tempfile::tempdir().unwrap();
        let original = json!({"statusLine":{"command":"echo user"}});
        let path = seed(dir.path(), original.clone());
        let unwritable = dir.path().join("absent").join("record");
        assert!(enable(&path, &unwritable, "/s", "/a").is_err());
        assert_eq!(read(&path), original);
        assert!(disable(&path, &dir.path().join("missing.record")).is_err());
    }
}
