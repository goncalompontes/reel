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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-unique counter, so two streams never share a directory even after a
/// torrent is removed and re-added.
static NEXT_STREAM: AtomicU64 = AtomicU64::new(0);

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
        let Some(pid) = name.strip_prefix("reel-stream-").and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if name == current || pid_alive(pid) {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
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
}
