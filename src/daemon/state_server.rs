use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::daemon::state_wire::{Delta, Hello, WIRE_VERSION};
use crate::daemon::store::TelemetryStore;

pub const HEARTBEAT_MS: u64 = 5_000;

pub struct StateServer {
    _thread: std::thread::JoinHandle<()>,
}

impl StateServer {
    pub fn start(path: &Path, store: Arc<TelemetryStore>) -> std::io::Result<Self> {
        Self::start_with_heartbeat(path, store, HEARTBEAT_MS)
    }

    fn start_with_heartbeat(
        path: &Path,
        store: Arc<TelemetryStore>,
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
                std::thread::spawn(move || handle_connection(stream, store, heartbeat_ms));
            }
        });
        Ok(Self { _thread: thread })
    }
}

fn handle_connection(mut stream: UnixStream, store: Arc<TelemetryStore>, heartbeat_ms: u64) {
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
        Some("snapshot") => {
            let (seq, panes) = store.snapshot();
            let _ = write_json_line(
                &mut stream,
                &Hello {
                    version: WIRE_VERSION,
                    seq,
                    panes,
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
        let _server = StateServer::start_with_heartbeat(&socket, store.clone(), 100).unwrap();

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
        let _server = StateServer::start_with_heartbeat(&socket, store, 100).unwrap();

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
}
