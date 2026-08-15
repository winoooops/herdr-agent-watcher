use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

#[derive(Debug, thiserror::Error)]
pub enum HerdrClientError {
    #[error("herdr socket unavailable at {path}: {source}")]
    Connect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("io error talking to herdr: {0}")]
    Io(#[from] std::io::Error),
    #[error("herdr returned malformed json: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("herdr error response: {0}")]
    Api(String),
}

pub fn decode_response(line: &str) -> Result<Value, HerdrClientError> {
    let value: Value = serde_json::from_str(line)?;
    if let Some(error) = value.get("error") {
        return Err(HerdrClientError::Api(error.to_string()));
    }
    Ok(value)
}

pub struct HerdrClient {
    socket_path: PathBuf,
    next_id: AtomicU64,
}

impl HerdrClient {
    pub fn from_env() -> Self {
        let socket_path = std::env::var_os("HERDR_SOCKET_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .or_else(|| dirs::home_dir().map(|home| home.join(".config")))
                    .unwrap_or_else(|| PathBuf::from(".config"))
                    .join("herdr/herdr.sock")
            });
        Self {
            socket_path,
            next_id: AtomicU64::new(1),
        }
    }

    /// One request per connection. The three-second read bound stays below
    /// the daemon's five-second singleton takeover deadline.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, HerdrClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let request = json!({ "id": id, "method": method, "params": params });
        let stream =
            UnixStream::connect(&self.socket_path).map_err(|source| HerdrClientError::Connect {
                path: self.socket_path.clone(),
                source,
            })?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut writer = stream.try_clone()?;
        writer.write_all(serde_json::to_string(&request)?.as_bytes())?;
        writer.write_all(b"\n")?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        decode_response(&line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_captured_ping_success() {
        let line = include_str!("../../tests/fixtures/ping-response.json");
        let value = decode_response(line).expect("ping fixture decodes as success");
        assert_eq!(value["id"], "probe-1");
    }

    #[test]
    fn error_response_maps_to_api_error() {
        let line = r#"{"id":"x","error":{"code":"unknown_method","message":"nope"}}"#;
        match decode_response(line) {
            Err(HerdrClientError::Api(message)) => {
                assert!(message.contains("unknown_method"));
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }
}
