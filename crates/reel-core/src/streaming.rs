//! Scratch directories for stream-only torrents.
//!
//! A stream does not need a custom storage engine. librqbit already ships a
//! filesystem storage that is exactly what a stream wants: files are created at
//! their full length (`set_len`) and written at their offsets with positioned
//! `pwritev`, so unwritten ranges stay sparse holes and only the fetched bytes
//! allocate. It uses one lock-free positioned write per chunk and no
//! whole-file buffers.
//!
//! reel's job is only the *lifetime*: each stream gets a unique directory under
//! a per-process scratch root, and that directory is deleted when the stream is
//! released. A previous version hand-rolled a `TorrentStorage` to keep pieces in
//! RAM and spill whole files to a second file — that allocated and zero-filled
//! whole files and never justified either. The OS page cache is the RAM cache.
//!
//! Scratch lives under the system temp directory, one root per PID, so a run
//! that is killed before it can clean up is identifiable and swept on the next
//! start.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-unique counter, so two streams never share a directory even after a
/// torrent is removed and re-added.
static NEXT_STREAM: AtomicU64 = AtomicU64::new(0);

/// Marks a scratch root as belonging to a running process.
const LOCK_NAME: &str = ".reel-lock";

/// Held open (and exclusively locked) for the lifetime of the process, so a
/// later run can tell this scratch root belongs to a live process.
///
/// The liveness test has to work on every platform: an OS file lock is portable,
/// while `/proc` is Linux-only. Without it, macOS would sweep a *running*
/// process's scratch (Unix lets you unlink files that are still open), which is
/// how a second instance could corrupt a live stream.
static ROOT_LOCK: OnceLock<Option<File>> = OnceLock::new();

/// The default scratch root for this process.
pub fn scratch_root() -> PathBuf {
    std::env::temp_dir().join(format!("reel-stream-{}", std::process::id()))
}

/// Create a fresh, empty directory for one stream under `root`.
///
/// The name is unique within the process; a leftover from an earlier failed
/// attempt is cleared so `create_new` in librqbit's storage always succeeds.
pub fn new_stream_dir(root: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    claim_root(root);
    let index = NEXT_STREAM.fetch_add(1, Ordering::Relaxed);
    let dir = root.join(format!("stream-{index}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Remove scratch directories left by processes that are no longer running.
pub fn sweep_stale_scratch() {
    let parent = std::env::temp_dir();
    let current = format!("reel-stream-{}", std::process::id());
    let Ok(entries) = std::fs::read_dir(&parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name
            .strip_prefix("reel-stream-")
            .and_then(|s| s.parse::<u32>().ok())
            .is_none()
        {
            continue;
        }
        if name == current || root_in_use(&entry.path()) {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

/// Claim `root` for this process, once. Best-effort: a root that cannot be
/// locked is still usable, it just cannot be told apart from a stale one.
fn claim_root(root: &Path) {
    ROOT_LOCK.get_or_init(|| {
        let file = File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(root.join(LOCK_NAME))
            .ok()?;
        // Holding the handle open is the claim; the lock is what the sweeper
        // tests. If the lock fails we keep the handle anyway.
        let _ = file.try_lock();
        Some(file)
    });
}

/// Whether another live process still owns `dir`.
///
/// A held lock means the owner is running; taking the lock means it is gone. A
/// directory with no lock file is from an older build, or a crash before the
/// claim, so it is treated as stale.
fn root_in_use(dir: &Path) -> bool {
    let Ok(file) = File::open(dir.join(LOCK_NAME)) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => false,
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_directories_are_unique_and_empty() {
        let root = std::env::temp_dir().join(format!(
            "reel-scratch-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);

        let a = new_stream_dir(&root).unwrap();
        let b = new_stream_dir(&root).unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read_dir(&a).unwrap().count(), 0);
        assert!(a.is_dir() && b.is_dir());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_scratch_from_a_dead_process_is_swept() {
        let parent = std::env::temp_dir();
        // A PID that cannot be running.
        let stale = parent.join("reel-stream-4294967294");
        let _ = std::fs::create_dir_all(&stale);
        assert!(stale.exists());

        sweep_stale_scratch();
        assert!(!stale.exists(), "a dead process's scratch should be removed");
    }

    #[test]
    fn a_live_process_scratch_is_kept() {
        let parent = std::env::temp_dir();
        // A root that is *not* this process's own, so the name-equality skip
        // does not apply and the liveness check is what protects it.
        let live = parent.join("reel-stream-4294967293");
        let _ = std::fs::remove_dir_all(&live);
        std::fs::create_dir_all(&live).unwrap();
        // Hold the lock the way a running process would.
        let lock = File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(live.join(LOCK_NAME))
            .unwrap();
        lock.try_lock().unwrap();

        sweep_stale_scratch();
        assert!(live.exists(), "a held lock means the owner is alive");

        let _ = std::fs::remove_dir_all(&live);
    }
}
