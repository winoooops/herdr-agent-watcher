//! Cross-platform local-socket IPC.
//!
//! On Unix: Unix domain sockets (file-path namespace).
//! On Windows: named pipes (kernel namespace).
//!
//! Follows the same pattern proven in `vimeflow-terminal/src/ipc.rs`.

use std::io::{self, Write};
use std::path::Path;

use interprocess::local_socket::traits::{Listener as _, Stream as _};

pub type LocalListener = interprocess::local_socket::Listener;
pub type LocalStream = interprocess::local_socket::Stream;

/// Connect to a local socket at the given path.
pub fn connect(path: &Path) -> io::Result<LocalStream> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::{prelude::*, GenericFilePath};
        let name = path.to_fs_name::<GenericFilePath>()?;
        LocalStream::connect(name)
    }

    #[cfg(windows)]
    {
        use interprocess::local_socket::{prelude::*, GenericNamespaced};
        let name = path.to_string_lossy().to_string();
        let name = name.to_ns_name::<GenericNamespaced>()?;
        LocalStream::connect(name)
    }
}

/// Bind a local listener at the given path.
pub fn bind(path: &Path) -> io::Result<LocalListener> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::{prelude::*, GenericFilePath, ListenerOptions};
        let name = path.to_fs_name::<GenericFilePath>()?;
        ListenerOptions::new().name(name).create_sync()
    }

    #[cfg(windows)]
    {
        use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};
        let name = path.to_string_lossy().to_string();
        let name = name.to_ns_name::<GenericNamespaced>()?;
        ListenerOptions::new().name(name).create_sync()
    }
}

/// Accept a connection from a local listener.
pub fn accept(listener: &LocalListener) -> io::Result<LocalStream> {
    listener.accept()
}

/// Read a single `\n`-terminated line from a stream without needing `BufReader`.
///
/// This avoids `try_clone()` (unavailable on Windows named pipes) by doing a
/// byte-at-a-time read for the initial request line, keeping the stream
/// available for subsequent writes.
pub fn read_line(stream: &mut LocalStream) -> io::Result<String> {
    use std::io::Read;
    let mut line = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                if line.is_empty() {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed"));
                }
                break;
            }
            Ok(_) => {
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(e) => return Err(e),
        }
    }
    String::from_utf8(line)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 line"))
}

/// Write a JSON value followed by a newline.
pub fn write_json_line(
    stream: &mut LocalStream,
    value: &impl serde::Serialize,
) -> io::Result<()> {
    serde_json::to_writer(&mut *stream, value)?;
    stream.write_all(b"\n")?;
    stream.flush()
}

/// Set the stream to nonblocking mode.
pub fn set_nonblocking(stream: &LocalStream, nonblocking: bool) -> io::Result<()> {
    stream.set_nonblocking(nonblocking)
}

/// Set the listener to nonblocking mode.
pub fn set_listener_nonblocking(listener: &LocalListener, nonblocking: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        listener.set_nonblocking(nonblocking)
    }

    #[cfg(windows)]
    {
        let _ = (listener, nonblocking);
        Ok(())
    }
}

/// Platform-aware read timeout. On Unix, uses the native socket timeout.
/// On Windows, this is a no-op: named pipes do not support read timeouts
/// and setting non-blocking mode breaks callers that do a synchronous
/// write-then-read (like HerdrClient::request), because BufReader::read_line
/// would return immediately with WouldBlock before the response arrives.
/// Callers that need non-blocking behaviour should call set_nonblocking directly.
pub fn set_read_timeout(
    stream: &LocalStream,
    _timeout: Option<std::time::Duration>,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        stream.set_read_timeout(_timeout)
    }

    #[cfg(windows)]
    {
        let _ = (stream, _timeout);
        Ok(())
    }
}

/// Platform-aware write timeout.
pub fn set_write_timeout(
    stream: &LocalStream,
    _timeout: Option<std::time::Duration>,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        stream.set_write_timeout(_timeout)
    }

    #[cfg(windows)]
    {
        let _ = (stream, _timeout);
        Ok(())
    }
}
