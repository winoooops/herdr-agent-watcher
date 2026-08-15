use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::daemon::state_wire::{Delta, Hello, StatusPath, WIRE_VERSION};
use crate::daemon::store::TelemetryStore;

pub const HEARTBEAT_MS: u64 = 5_000;

pub struct StateServer {
    _thread: std::thread::JoinHandle<()>,
}

impl StateServer {
    pub(crate) fn start(
        path: &Path,
        store: Arc<TelemetryStore>,
        routes: crate::daemon::routes::BridgeRoutes,
    ) -> std::io::Result<Self> {
        Self::start_with_heartbeat(path, store, routes, HEARTBEAT_MS)
    }

    fn start_with_heartbeat(
        path: &Path,
        store: Arc<TelemetryStore>,
        routes: crate::daemon::routes::BridgeRoutes,
        heartbeat_ms: u64,
    ) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(path)?;
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else {
                    break;
                };
                let store = store.clone();
                let routes = routes.clone();
                std::thread::spawn(move || handle_connection(stream, store, routes, heartbeat_ms));
            }
        });
        Ok(Self { _thread: thread })
    }
}

fn handle_connection(
    mut stream: UnixStream,
    store: Arc<TelemetryStore>,
    routes: crate::daemon::routes::BridgeRoutes,
    heartbeat_ms: u64,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let Ok(reader_stream) = stream.try_clone() else {
        return;
    };
    let mut request = String::new();
    if BufReader::new(reader_stream)
        .read_line(&mut request)
        .ok()
        .filter(|read| *read > 0)
        .is_none()
    {
        return;
    }
    let Ok(request) = serde_json::from_str::<Value>(&request) else {
        return;
    };

    match request["method"].as_str() {
        Some("status-path") => {
            let pane = request["pane_id"].as_str().unwrap_or_default();
            let session = request["session_id"].as_str().unwrap_or_default();
            let path = routes
                .resolve(pane, session)
                .map(|path| path.to_string_lossy().into_owned());
            let _ = write_json_line(
                &mut stream,
                &StatusPath {
                    version: WIRE_VERSION,
                    path,
                },
            );
        }
        Some("snapshot") => {
            let (seq, panes) = store.snapshot();
            let _ = write_json_line(
                &mut stream,
                &Hello {
                    version: WIRE_VERSION,
                    seq,
                    panes,
                    refused: routes.refusals(),
                },
            );
        }
        Some("subscribe") => {
            let (id, receiver, seq, panes) = store.subscribe_full();
            if write_json_line(
                &mut stream,
                &Hello {
                    version: WIRE_VERSION,
                    seq,
                    panes,
                    // The sidebar does not read this; `doctor` asks for a
                    // snapshot, not a subscription.
                    refused: Default::default(),
                },
            )
            .is_ok()
            {
                loop {
                    match receiver.recv_timeout(Duration::from_millis(heartbeat_ms)) {
                        Ok(update) => {
                            if write_json_line(
                                &mut stream,
                                &Delta {
                                    seq: update.seq,
                                    pane_id: update.pane_id,
                                    telemetry: update.telemetry,
                                },
                            )
                            .is_err()
                            {
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            if stream
                                .write_all(b"\n")
                                .and_then(|_| stream.flush())
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            }
            store.unsubscribe(id);
        }
        _ => {}
    }
}

fn write_json_line(stream: &mut UnixStream, value: &impl serde::Serialize) -> std::io::Result<()> {
    serde_json::to_writer(&mut *stream, value)?;
    stream.write_all(b"\n")?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    #[test]
    fn snapshot_subscribe_and_heartbeat_reaping() {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("state.sock");
        let store = Arc::new(TelemetryStore::default());
        store.set_agent("p1", "claude");
        store.record(
            "p1",
            "agent-lifecycle",
            serde_json::json!({"sessionId":"p1","phase":"running"}),
        );
        let _server = StateServer::start_with_heartbeat(
            &socket,
            store.clone(),
            crate::daemon::routes::BridgeRoutes::default(),
            100,
        )
        .unwrap();

        let mut client = UnixStream::connect(&socket).unwrap();
        client.write_all(b"{\"method\":\"snapshot\"}\n").unwrap();
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        let hello: Hello = serde_json::from_str(&line).unwrap();
        assert_eq!(hello.version, WIRE_VERSION);
        assert!(hello.panes.contains_key("p1"));

        let mut client = UnixStream::connect(&socket).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client.write_all(b"{\"method\":\"subscribe\"}\n").unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let mut first = String::new();
        reader.read_line(&mut first).unwrap();
        let hello: Hello = serde_json::from_str(&first).unwrap();
        store.record(
            "p1",
            "agent-lifecycle",
            serde_json::json!({"sessionId":"p1","phase":"idle"}),
        );
        let mut second = String::new();
        reader.read_line(&mut second).unwrap();
        let delta: Delta = serde_json::from_str(&second).unwrap();
        assert!(delta.seq > hello.seq);

        drop(reader);
        drop(client);
        std::thread::sleep(Duration::from_millis(350));
        assert_eq!(store.subscriber_count(), 0);
    }

    #[test]
    fn malformed_request_does_not_stop_the_server() {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("state.sock");
        let store = Arc::new(TelemetryStore::default());
        let _server = StateServer::start_with_heartbeat(
            &socket,
            store,
            crate::daemon::routes::BridgeRoutes::default(),
            100,
        )
        .unwrap();

        let mut malformed = UnixStream::connect(&socket).unwrap();
        malformed.write_all(b"not json\n").unwrap();
        drop(malformed);

        let mut client = UnixStream::connect(&socket).unwrap();
        client.write_all(b"{\"method\":\"snapshot\"}\n").unwrap();
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).unwrap();
        let hello: Hello = serde_json::from_str(&line).unwrap();
        assert_eq!(hello.version, WIRE_VERSION);
    }

    fn seed_watcher(routes: &crate::daemon::routes::BridgeRoutes, pane: &str) {
        use crate::agent::adapter::base::WatcherHandle;
        let handle = WatcherHandle::new_for_test(
            crate::agent::adapter::base::TranscriptState::default(),
            pane.to_string(),
        );
        routes.watchers().insert(
            pane.to_string(),
            handle,
            crate::agent::types::AgentType::ClaudeCode,
        );
    }

    fn ask(socket: &std::path::Path, session: &str) -> serde_json::Value {
        let mut stream = UnixStream::connect(socket).expect("connect");
        writeln!(
            stream,
            r#"{{"method":"status-path","pane_id":"w1:p1","session_id":"{session}"}}"#
        )
        .expect("request");
        let mut line = String::new();
        BufReader::new(stream.try_clone().expect("clone"))
            .read_line(&mut line)
            .expect("reply");
        serde_json::from_str(&line).expect("json")
    }

    #[test]
    fn the_socket_answers_status_path_for_a_matching_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("state.sock");
        let routes = crate::daemon::routes::BridgeRoutes::default();
        seed_watcher(&routes, "w1:p1");
        routes.bind("w1:p1", "s1");
        let _server = StateServer::start(&socket, Arc::new(TelemetryStore::default()), routes)
            .expect("start");
        let reply = ask(&socket, "s1");
        assert_eq!(reply["version"], 2);
        assert!(!reply["path"].is_null());
    }

    #[test]
    fn the_socket_answers_null_for_a_mismatched_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("state.sock");
        let routes = crate::daemon::routes::BridgeRoutes::default();
        seed_watcher(&routes, "w1:p1");
        routes.bind("w1:p1", "s1");
        let _server = StateServer::start(&socket, Arc::new(TelemetryStore::default()), routes)
            .expect("start");
        assert!(ask(&socket, "OTHER")["path"].is_null());
    }
}
