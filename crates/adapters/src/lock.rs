//! The run lock (`run.lock` in the state directory): one `--apply` at a time, held by `flock` so the kernel
//! drops it when the process dies, however it dies. The file holds the holder's pid for the `LockHeld` text.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use disk_cleanup_core::ports::RunGuard;
use disk_cleanup_core::{AbortFlag, FatalError};

use rustix::fs::{flock, FlockOperation};

use crate::signals::SignalState;
use crate::state::{chown_if, Owner};

#[derive(Debug)]
pub enum LockError {
    /// Another process holds the lock; `pid` is 0 when its pid line could not be read.
    Held {
        pid: u32,
    },
    Io(io::Error),
}

/// An exclusive hold on `run.lock`; released on drop.
#[derive(Debug)]
pub struct RunLock {
    _file: File,
}

fn holder_pid(file: &mut File) -> u32 {
    let mut s = String::new();
    let _ = file.seek(SeekFrom::Start(0));
    let _ = Read::by_ref(file).take(32).read_to_string(&mut s);
    s.trim().parse().unwrap_or(0)
}

impl RunLock {
    /// Takes the lock without waiting, creating the file 0600 (owned by `owner` under sudo) and recording this
    /// process's pid in it.
    pub fn try_acquire(path: &Path, owner: Option<Owner>) -> Result<RunLock, LockError> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(LockError::Io)?;
        chown_if(owner, path);
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {
                file.set_len(0).map_err(LockError::Io)?;
                file.seek(SeekFrom::Start(0)).map_err(LockError::Io)?;
                writeln!(file, "{}", std::process::id()).map_err(LockError::Io)?;
                Ok(RunLock { _file: file })
            }
            Err(e) if e == rustix::io::Errno::WOULDBLOCK => Err(LockError::Held {
                pid: holder_pid(&mut file),
            }),
            Err(e) => Err(LockError::Io(e.into())),
        }
    }

    /// Plan mode's look at the lock: the holder's pid if one holds it, `None` otherwise. It never creates the
    /// file and releases at once a lock it took, so the holder keeps its lock.
    pub fn probe(path: &Path) -> Option<u32> {
        let mut file = File::open(path).ok()?;
        match flock(&file, FlockOperation::NonBlockingLockShared) {
            Ok(()) => None,
            Err(e) if e == rustix::io::Errno::WOULDBLOCK => Some(holder_pid(&mut file)),
            Err(_) => None,
        }
    }
}

/// The `RunGuard` port: `run.lock` plus the abort flag the signal watcher sets. Build the `SignalState`
/// first, hand a clone to `signals::install`, and give the same one here.
#[derive(Debug)]
pub struct RealRunGuard {
    path: PathBuf,
    owner: Option<Owner>,
    signals: Arc<SignalState>,
    held: Mutex<Option<RunLock>>,
}

impl RealRunGuard {
    pub fn new(path: PathBuf, owner: Option<Owner>, signals: Arc<SignalState>) -> RealRunGuard {
        RealRunGuard {
            path,
            owner,
            signals,
            held: Mutex::new(None),
        }
    }
}

impl RunGuard for RealRunGuard {
    /// Holds the lock until the guard is dropped. A lock file that cannot be opened reads as `LockHeld { pid: 0 }`:
    /// no run proceeds without the lock.
    fn acquire(&self) -> Result<(), FatalError> {
        let lock = RunLock::try_acquire(&self.path, self.owner).map_err(|e| match e {
            LockError::Held { pid } => FatalError::LockHeld { pid },
            LockError::Io(_) => FatalError::LockHeld { pid: 0 },
        })?;
        *self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(lock);
        Ok(())
    }

    fn abort_flag(&self) -> &AbortFlag {
        &self.signals.abort
    }

    fn interrupted(&self) -> Option<i32> {
        self.signals.interrupted()
    }
}
