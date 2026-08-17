use std::collections::HashMap;

use crate::daemon::state_wire::{Delta, Hello, WIRE_VERSION};
use crate::daemon::store::PaneTelemetry;

#[derive(Default)]
pub struct State {
    pub panes: HashMap<String, PaneTelemetry>,
    pub last_seq: u64,
    /// The build the daemon reported, or `None` from one too old to say.
    pub daemon_build: Option<String>,
}

pub fn apply_line(state: &mut State, line: &str) -> Result<(), String> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }
    let value: serde_json::Value = serde_json::from_str(line).map_err(|error| error.to_string())?;
    if value.get("version").is_some() {
        let hello: Hello = serde_json::from_value(value).map_err(|error| error.to_string())?;
        if hello.version != WIRE_VERSION {
            return Err(format!(
                "state-socket version {} (sidebar speaks {WIRE_VERSION}) — rebuild and restart the daemon",
                hello.version
            ));
        }
        state.panes = hello.panes;
        state.last_seq = hello.seq;
        state.daemon_build = hello.build;
    } else if value.get("pane_id").is_some() {
        let delta: Delta = serde_json::from_value(value).map_err(|error| error.to_string())?;
        if delta.seq <= state.last_seq {
            return Ok(());
        }
        state.last_seq = delta.seq;
        match delta.telemetry {
            Some(telemetry) => {
                state.panes.insert(delta.pane_id, telemetry);
            }
            None => {
                state.panes.remove(&delta.pane_id);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_delta_stale_removal_and_heartbeat() {
        let mut state = State::default();
        apply_line(
            &mut state,
            r#"{"version":2,"seq":10,"panes":{"p1":{"card_state":"running"}}}"#,
        )
        .unwrap();
        assert_eq!(state.panes.len(), 1);

        apply_line(
            &mut state,
            r#"{"seq":11,"pane_id":"p2","telemetry":{"card_state":"idle"}}"#,
        )
        .unwrap();
        assert_eq!(state.panes.len(), 2);

        apply_line(&mut state, r#"{"seq":5,"pane_id":"p1","telemetry":null}"#).unwrap();
        assert!(state.panes.contains_key("p1"));

        apply_line(&mut state, r#"{"seq":12,"pane_id":"p1","telemetry":null}"#).unwrap();
        assert!(!state.panes.contains_key("p1"));
        apply_line(&mut state, "").unwrap();
    }

    #[test]
    fn version_mismatch_is_a_hard_error() {
        let error = apply_line(
            &mut State::default(),
            r#"{"version":99,"seq":0,"panes":{}}"#,
        )
        .unwrap_err();
        assert!(error.contains("version"));
    }

    #[test]
    fn unknown_shapes_are_ignored() {
        let mut state = State::default();
        apply_line(&mut state, r#"{"future":true}"#).unwrap();
        assert!(state.panes.is_empty());
    }
    /// A daemon from before this field exists sends no `build`. "Cannot tell"
    /// has to stay silent: a notice that fires on every older daemon would
    /// train the reader to ignore the one that means something.
    #[test]
    fn a_hello_without_a_build_leaves_the_state_saying_nothing() {
        let mut state = State::default();
        apply_line(
            &mut state,
            &format!("{{\"version\":{WIRE_VERSION},\"seq\":1,\"panes\":{{}}}}"),
        )
        .expect("an older daemon is not an error");
        assert_eq!(state.daemon_build, None, "nothing to compare against");
    }

    /// And one that does send it is remembered verbatim, including when it
    /// matches -- the comparison belongs to the caller, not here.
    #[test]
    fn a_hello_with_a_build_is_remembered() {
        let mut state = State::default();
        apply_line(
            &mut state,
            &format!("{{\"version\":{WIRE_VERSION},\"seq\":1,\"panes\":{{}},\"build\":\"0.0.1\"}}"),
        )
        .expect("applies");
        assert_eq!(state.daemon_build.as_deref(), Some("0.0.1"));
    }
}
