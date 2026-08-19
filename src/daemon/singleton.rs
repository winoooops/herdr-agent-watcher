use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn try_flock(file: &File) -> bool {
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

fn request_shutdown(path: &std::path::Path, budget: Duration) -> bool {
    let Ok(stream) = UnixStream::connect(path) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(budget.min(Duration::from_secs(2))));
    let Ok(mut writer) = stream.try_clone() else {
        return false;
    };
    if writer.write_all(b"shutdown\n").is_err() {
        return false;
    }
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).is_ok() && line.trim() == "ok"
}

pub struct Singleton {
    _lock: File,
    pub shutdown: Arc<AtomicBool>,
    control_path: std::path::PathBuf,
    listener: Option<std::thread::JoinHandle<()>>,
}

pub fn claim() -> Option<Singleton> {
    claim_with(
        &crate::daemon::DaemonOptions::from_env(),
        Arc::new(AtomicBool::new(false)),
    )
}

pub(crate) fn claim_with(
    options: &crate::daemon::DaemonOptions,
    shutdown: Arc<AtomicBool>,
) -> Option<Singleton> {
    let path = options.singleton_lock_path();
    std::fs::create_dir_all(path.parent()?).ok()?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
        .ok()?;

    if !try_flock(&file) {
        let deadline = Instant::now() + Duration::from_secs(4);
        request_shutdown(
            &options.control_socket_path(),
            deadline.saturating_duration_since(Instant::now()),
        );
        if !wait_until(deadline, || try_flock(&file)) {
            eprintln!("another herdr-agent-watcher daemon holds the lock and did not exit");
            return None;
        }
    }

    let control_path = options.control_socket_path();
    let _ = std::fs::remove_file(&control_path);
    let listener = UnixListener::bind(&control_path).ok()?;
    listener.set_nonblocking(true).ok()?;
    let flag = shutdown.clone();
    let listener_thread = std::thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                    let mut line = String::new();
                    let mut reader =
                        BufReader::new(stream.try_clone().expect("clone control stream"));
                    if reader.read_line(&mut line).is_ok() && line.trim() == "shutdown" {
                        flag.store(true, Ordering::Relaxed);
                        let mut writer = stream;
                        let _ = writer.write_all(b"ok\n");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    Some(Singleton {
        _lock: file,
        shutdown,
        control_path,
        listener: Some(listener_thread),
    })
}

impl Drop for Singleton {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.listener.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.control_path);
    }
}

pub fn sleep_interruptible(shutdown: &AtomicBool, total: Duration) {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline && !shutdown.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_until(deadline: Instant, mut acquire: impl FnMut() -> bool) -> bool {
    loop {
        if acquire() {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

pub fn stop() -> i32 {
    if request_shutdown(
        &crate::daemon::DaemonOptions::from_env().control_socket_path(),
        Duration::from_secs(2),
    ) {
        0
    } else {
        eprintln!("no running daemon (or it did not acknowledge)");
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_until_never_oversleeps_the_deadline() {
        let budget = Duration::from_millis(250);
        let start = Instant::now();
        assert!(!wait_until(Instant::now() + budget, || false));
        let elapsed = start.elapsed();
        assert!(elapsed >= budget);
        assert!(elapsed < budget + Duration::from_millis(50));
    }
}
