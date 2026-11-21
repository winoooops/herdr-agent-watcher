use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::ipc;

// Herdr reads can block for three seconds, and a full live shutdown was
// measured at 4.36 seconds after the remaining iteration work. Eight seconds
// leaves 3.64 seconds of headroom; the alternative failure leaves no daemon.
const TAKEOVER_TIMEOUT: Duration = Duration::from_secs(8);

#[cfg(unix)]
fn try_flock(file: &File) -> bool {
    use std::os::unix::io::AsRawFd;
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

#[cfg(windows)]
fn try_flock(file: &File) -> bool {
    use std::os::windows::io::AsRawHandle;
    let handle = file.as_raw_handle();
    let mut overlapped: windows_sys::Win32::System::IO::OVERLAPPED = unsafe { std::mem::zeroed() };
    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x00000002;
    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x00000001;
    unsafe {
        windows_sys::Win32::Storage::FileSystem::LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        ) != 0
    }
}

fn request_shutdown(path: &std::path::Path, budget: Duration) -> bool {
    let Ok(mut stream) = ipc::connect(path) else {
        return false;
    };
    let _ = ipc::set_read_timeout(&stream, Some(budget.min(Duration::from_secs(2))));
    if stream.write_all(b"shutdown\n").is_err() {
        return false;
    }
    let _ = stream.flush();
    // Read response line
    match ipc::read_line(&mut stream) {
        Ok(line) => line.trim() == "ok",
        Err(_) => false,
    }
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
        let deadline = Instant::now() + TAKEOVER_TIMEOUT;
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
    let listener = ipc::bind(&control_path).ok()?;
    ipc::set_listener_nonblocking(&listener, true).ok()?;
    let flag = shutdown.clone();
    let listener_thread = std::thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            match ipc::accept(&listener) {
                Ok(mut stream) => {
                    let _ = ipc::set_nonblocking(&stream, false);
                    let _ = ipc::set_read_timeout(&stream, Some(Duration::from_millis(500)));
                    match ipc::read_line(&mut stream) {
                        Ok(line) if line.trim() == "shutdown" => {
                            flag.store(true, Ordering::Relaxed);
                            let _ = stream.write_all(b"ok\n");
                        }
                        _ => {}
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

    #[test]
    fn takeover_budget_has_headroom_over_the_measured_shutdown() {
        assert!(TAKEOVER_TIMEOUT >= Duration::from_secs(8));
        assert!(TAKEOVER_TIMEOUT > Duration::from_millis(4_360));
    }
}
