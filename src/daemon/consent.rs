use std::path::PathBuf;

/// Reloads externally persisted Kimi consent, including revocation by deletion.
pub struct ConsentReloader {
    path: PathBuf,
    last: Option<Vec<u8>>,
    primed: bool,
}

impl ConsentReloader {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            last: None,
            primed: false,
        }
    }

    pub fn reload_if_changed(&mut self) -> bool {
        let current = std::fs::read(&self.path).ok();
        let changed = (!self.primed && current.is_some()) || (self.primed && current != self.last);
        self.primed = true;
        if changed {
            crate::agents::consent::load_into_memory(&self.path);
            crate::agents::consent::request_refresh();
        }
        self.last = current;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_detects_external_grant_and_revocation() {
        let _guard = crate::agents::consent::test_serial_guard();
        crate::agents::consent::set_for_test(false);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("kimi-usage-consent.json");
        let mut watcher = ConsentReloader::new(path.clone());

        assert!(!watcher.reload_if_changed());
        std::fs::write(&path, r#"{"enabled":true}"#).unwrap();
        assert!(watcher.reload_if_changed());
        assert!(crate::agents::consent::enabled());
        assert!(!watcher.reload_if_changed());

        std::fs::remove_file(&path).unwrap();
        assert!(watcher.reload_if_changed());
        assert!(!crate::agents::consent::enabled());
        crate::agents::consent::set_for_test(false);
    }
}
