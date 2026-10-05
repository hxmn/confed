//! Advisory workspace lock, so two confed processes cannot fight over the same
//! directory. It protects against concurrent confed runs, not against an editor
//! saving a file mid-push — that is handled by hashing at plan time.

use crate::error::{ConfedError, Result};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const LOCK_FILENAME: &str = ".confed.lock";

/// Held for the duration of a mutating command; released on drop.
#[derive(Debug)]
pub struct WorkspaceLock {
    path: PathBuf,
}

impl WorkspaceLock {
    /// Take the lock, or explain who holds it.
    pub fn acquire(dir: &Path) -> Result<Self> {
        let path = dir.join(LOCK_FILENAME);

        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                Ok(Self { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = read_holder(&path);
                if let Some(pid) = holder {
                    if !process_alive(pid) {
                        // Previous run died without cleaning up.
                        tracing::debug!(target: "confed::lock", pid, "removing stale lock");
                        let _ = fs::remove_file(&path);
                        return Self::acquire(dir);
                    }
                }
                Err(ConfedError::state_with_hint(
                    match holder {
                        Some(pid) => {
                            format!("another confed process (pid {pid}) is using this directory")
                        }
                        None => "another confed process is using this directory".to_string(),
                    },
                    format!("wait for it to finish, or remove {} if it is stale", path.display()),
                ))
            }
            Err(e) => Err(ConfedError::io(format!("creating {}", path.display()), e)),
        }
    }
}

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn read_holder(path: &Path) -> Option<u32> {
    let mut buf = String::new();
    fs::File::open(path).ok()?.read_to_string(&mut buf).ok()?;
    buf.trim().parse().ok()
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // `kill(0, …)` addresses the caller's whole process group, so it would
    // always report "alive"; confed never writes pid 0, so treat it as stale.
    if pid == 0 {
        return false;
    }
    // Signal 0 checks for existence without delivering anything.
    unsafe { libc_kill(pid as i32, 0) == 0 }
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    kill(pid, sig)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use std::ffi::c_void;

    // The kernel32 functions this needs, declared here rather than pulling in a
    // Windows bindings crate for three calls.
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    const ERROR_ACCESS_DENIED: u32 = 5;
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn GetExitCodeProcess(process: *mut c_void, code: *mut u32) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    // confed never writes pid 0 (the System Idle Process).
    if pid == 0 {
        return false;
    }
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            // No such process — unless it exists but belongs to someone we may
            // not inspect, which still means it is running.
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut code = 0u32;
        let queried = GetExitCodeProcess(process, &mut code) != 0;
        CloseHandle(process);
        // An exited process whose handle is still open somewhere reports its
        // exit code; a running one reports STILL_ACTIVE.
        !queried || code == STILL_ACTIVE
    }
}

#[cfg(not(any(unix, windows)))]
fn process_alive(_pid: u32) -> bool {
    // Without a check on this platform, assume the holder is alive: refusing to
    // run is safer than two processes writing the same files.
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let lock = WorkspaceLock::acquire(dir.path()).unwrap();

        let err = WorkspaceLock::acquire(dir.path()).unwrap_err();
        assert_eq!(err.exit_code(), crate::error::ExitCode::State);
        assert!(err.to_string().contains("another confed process"));
        assert!(err.hint().unwrap().contains(LOCK_FILENAME));

        drop(lock);
        assert!(WorkspaceLock::acquire(dir.path()).is_ok());
    }

    #[test]
    fn a_lock_left_by_a_dead_process_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        // A process that has already exited: its pid cannot be alive.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();

        fs::write(dir.path().join(LOCK_FILENAME), format!("{dead_pid}\n")).unwrap();
        assert!(
            WorkspaceLock::acquire(dir.path()).is_ok(),
            "a lock held by pid {dead_pid} (exited) should be reclaimed"
        );
    }

    #[test]
    fn a_garbage_lock_file_does_not_wedge_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(LOCK_FILENAME), "0\n").unwrap();
        assert!(WorkspaceLock::acquire(dir.path()).is_ok());
    }
}
