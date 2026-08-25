use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

const RECONNECT_EVERY: Duration = Duration::from_millis(400);
const READ_POLL_EVERY: Duration = Duration::from_millis(100);

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Line(String),
    Disconnected,
    Reconnected,
}

pub struct StateStream {
    events: Receiver<Event>,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StateStream {
    pub fn start(socket: impl Into<PathBuf>) -> Result<Self, String> {
        let socket = socket.into();
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = shutdown.clone();
        let (startup_tx, startup_rx) = channel();
        let (events_tx, events) = channel();
        let thread = std::thread::spawn(move || {
            let mut reader = match subscribe(&socket) {
                Ok(reader) => {
                    let _ = startup_tx.send(Ok(()));
                    reader
                }
                Err(error) => {
                    let _ = startup_tx.send(Err(error));
                    return;
                }
            };
            let mut line = String::new();

            while !stop.load(Ordering::Relaxed) {
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        line.clear();
                    }
                    Ok(_) => {
                        if events_tx
                            .send(Event::Line(std::mem::take(&mut line)))
                            .is_err()
                        {
                            return;
                        }
                        continue;
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        continue;
                    }
                    Err(_) => {}
                }

                if events_tx.send(Event::Disconnected).is_err() {
                    return;
                }
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    match subscribe(&socket) {
                        Ok(fresh) => {
                            reader = fresh;
                            line.clear();
                            if events_tx.send(Event::Reconnected).is_err() {
                                return;
                            }
                            break;
                        }
                        Err(_) => std::thread::sleep(RECONNECT_EVERY),
                    }
                }
            }
        });

        match startup_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                events,
                shutdown,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err("state-stream worker stopped during startup".into())
            }
        }
    }

    pub fn try_recv(&self) -> Result<Event, TryRecvError> {
        self.events.try_recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<Event, RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    pub fn join(mut self) -> std::thread::Result<()> {
        self.thread
            .take()
            .expect("state-stream thread already joined")
            .join()
    }
}

impl Drop for StateStream {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn subscribe(socket: &Path) -> Result<BufReader<UnixStream>, String> {
    let mut stream = UnixStream::connect(socket).map_err(|error| {
        format!(
            "herdr-agent-watcher daemon is not running\n(no state socket at {}: {error})",
            socket.display()
        )
    })?;
    stream
        .write_all(b"{\"method\":\"subscribe\"}\n")
        .map_err(|_| "herdr-agent-watcher daemon closed the state socket".to_string())?;
    stream
        .set_read_timeout(Some(READ_POLL_EVERY))
        .map_err(|error| format!("cannot configure the state socket: {error}"))?;
    Ok(BufReader::new(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::time::Instant;

    fn listener() -> (tempfile::TempDir, PathBuf, UnixListener) {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("state.sock");
        let listener = UnixListener::bind(&socket).expect("bind fake state socket");
        (dir, socket, listener)
    }

    fn serve_once(listener: UnixListener, line: &'static str) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept subscriber");
            let mut request = String::new();
            BufReader::new(stream.try_clone().expect("clone stream"))
                .read_line(&mut request)
                .expect("read subscription");
            assert_eq!(request, "{\"method\":\"subscribe\"}\n");
            stream.write_all(line.as_bytes()).expect("write state line");
        })
    }

    #[test]
    fn subscribes_and_delivers_lines_to_the_public_reducer() {
        let (_dir, socket, listener) = listener();
        let server = serve_once(listener, "{\"version\":2,\"seq\":7,\"panes\":{}}\n");
        let stream = StateStream::start(&socket).expect("start state stream");
        let Event::Line(line) = stream
            .recv_timeout(Duration::from_secs(1))
            .expect("receive state line")
        else {
            panic!("first event was not a line")
        };
        let mut state = crate::sidebar::reducer::State::default();
        crate::sidebar::reducer::apply_line(&mut state, &line).expect("fold state line");
        assert_eq!(state.last_seq, 7);
        stream.shutdown();
        stream.join().expect("join state stream");
        server.join().expect("join fake server");
    }

    #[test]
    fn reconnects_and_resubscribes_after_the_server_restarts() {
        let (dir, socket, first_listener) = listener();
        let first = serve_once(first_listener, "{\"version\":2,\"seq\":1,\"panes\":{}}\n");
        let stream = StateStream::start(&socket).expect("start state stream");
        assert!(matches!(
            stream.recv_timeout(Duration::from_secs(1)),
            Ok(Event::Line(_))
        ));
        first.join().expect("join first server");
        assert_eq!(
            stream
                .recv_timeout(Duration::from_secs(1))
                .expect("disconnect event"),
            Event::Disconnected
        );

        std::fs::remove_file(&socket).expect("remove first socket");
        let second_listener = UnixListener::bind(&socket).expect("bind restarted state socket");
        let second = serve_once(second_listener, "{\"version\":2,\"seq\":2,\"panes\":{}}\n");
        assert_eq!(
            stream
                .recv_timeout(Duration::from_secs(2))
                .expect("reconnect event"),
            Event::Reconnected
        );
        let Event::Line(line) = stream
            .recv_timeout(Duration::from_secs(1))
            .expect("line after reconnect")
        else {
            panic!("reconnected stream did not deliver a line")
        };
        let mut state = crate::sidebar::reducer::State::default();
        crate::sidebar::reducer::apply_line(&mut state, &line).expect("fold restarted snapshot");
        assert_eq!(state.last_seq, 2);
        stream.shutdown();
        stream.join().expect("join state stream");
        second.join().expect("join second server");
        std::fs::remove_file(&socket).expect("remove restarted socket");
        assert!(!socket.exists());
        drop(dir);
    }

    #[test]
    fn shutdown_joins_a_connected_worker_without_leaks() {
        let (_dir, socket, listener) = listener();
        let (subscribed_tx, subscribed_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept subscriber");
            let mut request = String::new();
            BufReader::new(stream)
                .read_line(&mut request)
                .expect("read subscription");
            subscribed_tx.send(()).expect("report subscription");
        });
        let stream = StateStream::start(&socket).expect("start state stream");
        subscribed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("server saw subscription");
        let started = Instant::now();
        stream.shutdown();
        stream.join().expect("join state stream");
        assert!(started.elapsed() < Duration::from_secs(1));
        server.join().expect("join fake server");
    }
}
