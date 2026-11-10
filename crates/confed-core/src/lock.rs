//! Advisory workspace lock, so two confed processes cannot fight over the same
//! directory. It protects against concurrent confed runs, not against an editor
//! saving a file mid-push — that is handled by hashing at plan time.

use crate::error::{ConfedError, Result};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, Write};
use std::path::Path;

pub const LOCK_FILENAME: &str = ".confed.lock";

/// Held for the duration of a mutating command; released on drop.
///
/// An operating-system lock on [`LOCK_FILENAME`] (`flock` on Unix,
/// `LockFileEx` on Windows). The system releases it when the holder exits,
/// however it exits, so there is no stale lock to judge or reclaim, and no
/// window in which two processes both decide the old holder is gone. The file
/// itself stays: removing it would let a process lock a fresh file while
/// another still holds the old one.
#[derive(Debug)]
pub struct WorkspaceLock {
    _file: File,
}

impl WorkspaceLock {
    /// Take the lock, or explain who holds it.
    pub fn acquire(dir: &Path) -> Result<Self> {
        let path = dir.join(LOCK_FILENAME);
        let opened =
            OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path);
        let mut file =
            opened.map_err(|e| ConfedError::io(format!("opening {}", path.display()), e))?;
        match file.try_lock() {
            Ok(()) => {
                // Who holds it, for the message another run prints. Only a
                // courtesy: the lock is what counts, not this number.
                let _ = file.set_len(0);
                let _ = file.rewind();
                let _ = writeln!(file, "{}", std::process::id());
                let _ = file.flush();
                Ok(Self { _file: file })
            }
            Err(TryLockError::WouldBlock) => {
                let message = match read_holder(&mut file) {
                    Some(pid) => {
                        format!("another confed process (pid {pid}) is using this directory")
                    }
                    None => "another confed process is using this directory".to_string(),
                };
                Err(ConfedError::state_with_hint(
                    message,
                    "wait for it to finish; the lock is released the moment it exits",
                ))
            }
            Err(TryLockError::Error(e)) => {
                Err(ConfedError::io(format!("locking {}", path.display()), e))
            }
        }
    }
}

fn read_holder(file: &mut File) -> Option<u32> {
    let mut buf = String::new();
    file.rewind().ok()?;
    file.read_to_string(&mut buf).ok()?;
    buf.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Held by every test here. A process spawned while another test holds a
    /// lock carries a copy of its descriptor until it execs, and keeps the
    /// lock taken for that instant after the holder lets go.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn the_lock_is_exclusive_and_released_on_drop() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let lock = WorkspaceLock::acquire(dir.path()).unwrap();

        let err = WorkspaceLock::acquire(dir.path()).unwrap_err();
        assert_eq!(err.exit_code(), crate::error::ExitCode::State);
        assert!(err.to_string().contains("another confed process"));
        assert!(err.hint().unwrap().contains("wait for it"));

        drop(lock);
        assert!(WorkspaceLock::acquire(dir.path()).is_ok());
    }

    /// A lock file left behind — a crash, a kill, an old confed that wrote
    /// a pid — locks nothing: only a live holder does.
    #[test]
    fn a_leftover_lock_file_does_not_wedge_the_workspace() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        for leftover in ["0\n", "999999\n", "garbage", ""] {
            std::fs::write(dir.path().join(LOCK_FILENAME), leftover).unwrap();
            assert!(WorkspaceLock::acquire(dir.path()).is_ok(), "left over: {leftover:?}");
        }
    }

    /// Another process's lock — whoever owns it — is honoured.
    #[cfg(unix)]
    #[test]
    fn a_lock_held_by_another_process_is_refused_until_it_exits() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOCK_FILENAME);
        std::fs::write(&path, "").unwrap();
        // flock(1) holds the lock until it exits; `-o` keeps the descriptor
        // from its child, so killing flock is enough to release it.
        let Ok(mut holder) = std::process::Command::new("flock")
            .arg("-o")
            .arg(&path)
            .args(["sleep", "10"])
            // Detached from the test's output, which the harness reads to
            // the end: the `sleep` outlives the `flock` killed below.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            return; // no flock(1) on this machine
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while WorkspaceLock::acquire(dir.path()).is_ok() {
            assert!(std::time::Instant::now() < deadline, "the other process never took the lock");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let err = WorkspaceLock::acquire(dir.path()).unwrap_err();
        assert!(err.to_string().contains("another confed process"), "{err}");

        holder.kill().unwrap();
        holder.wait().unwrap();
        assert!(WorkspaceLock::acquire(dir.path()).is_ok(), "released when the holder died");
    }
}
