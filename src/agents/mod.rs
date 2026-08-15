pub(crate) mod bridge_scripts;
pub(crate) mod bridge_settings;
pub mod claude_bridge;
pub mod consent;
pub(crate) mod doctor;
pub mod keybinding;
pub(crate) mod keys;
pub mod sidecar;

use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct BoundPane {
    pub pane_id: String,
    pub agent_id: String,
    pub agent_session: String,
}

pub trait AgentAdapter: Send + Sync + 'static {
    fn ids(&self) -> &[&str];
    fn bind(&self, pane: BoundPane) -> Result<(), String>;
    fn unbind(&self, pane_id: &str) -> Result<(), String>;
}

pub struct AgentRegistry {
    by_id: HashMap<String, Arc<dyn AgentAdapter>>,
}

impl AgentRegistry {
    pub fn new(adapters: Vec<Arc<dyn AgentAdapter>>) -> Self {
        let mut by_id = HashMap::new();
        for adapter in adapters {
            for id in adapter.ids() {
                by_id.insert((*id).to_string(), adapter.clone());
            }
        }
        Self { by_id }
    }

    pub fn adapter_for(&self, agent_id: &str) -> Option<&Arc<dyn AgentAdapter>> {
        self.by_id.get(agent_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeAdapter;

    impl AgentAdapter for FakeAdapter {
        fn ids(&self) -> &[&str] {
            &["claude", "claude-code"]
        }

        fn bind(&self, _pane: BoundPane) -> Result<(), String> {
            Ok(())
        }

        fn unbind(&self, _pane_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn resolves_aliases_and_falls_through_unknown() {
        let registry = AgentRegistry::new(vec![Arc::new(FakeAdapter)]);
        assert!(registry.adapter_for("claude").is_some());
        assert!(registry.adapter_for("claude-code").is_some());
        assert!(registry.adapter_for("some-future-agent").is_none());
    }

    #[test]
    fn sidecar_ids_cover_all_captured_live_agents() {
        for fixture in [
            include_str!("../../tests/fixtures/pane-codex.json"),
            include_str!("../../tests/fixtures/pane-kimi.json"),
            include_str!("../../tests/fixtures/pane-opencode.json"),
        ] {
            let pane: crate::herdr::api::PaneInfo = serde_json::from_str(fixture).unwrap();
            let agent = pane.agent.expect("captured pane has an agent id");
            assert!(
                crate::sidebar::agent_ids::ACCEPTED_IDS.contains(&agent.as_str()),
                "registry must claim live agent id {agent}"
            );
        }
    }
}
