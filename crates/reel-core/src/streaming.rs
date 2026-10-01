//! Temporary storage for a torrent that is only being watched.
//!
//! # Why this exists
//!
//! Playing something should not quietly turn into a download. A stream-only
//! torrent writes its pieces to storage that is thrown away when playback stops:
//! nothing lands in the download folder, nothing survives the stream.
//!
//! # Memory first, spill to scratch
//!
//! librqbit's reader trusts the chunk tracker's have-bit and reads storage
//! directly, so a piece that was evicted is a hard stream error and the tracker
//! is not reachable from outside the crate. A bounded cache that *evicts* is
//! therefore unsafe for a seekable player. Instead this storage keeps a whole
//! *file* in RAM while it fits in the budget, and spills whole files to a
//! scratch file once the budget is exhausted. Every piece stays readable, the
//! memory is bounded, and the scratch file is deleted when the storage is
//! dropped.
//!
//! The common case — an episode of a few hundred megabytes — stays entirely in
//! memory. A large film spills the parts that no longer fit.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use librqbit::storage::{BoxStorageFactory, StorageFactory, StorageFactoryExt, TorrentStorage};
use librqbit::{ManagedTorrentShared, TorrentMetadata};

/// Default RAM a single stream may use before spilling to scratch.
pub const DEFAULT_MEMORY_BUDGET: usize = 512 * 1024 * 1024;

/// Creates a [`StreamingStorage`] per torrent, under a shared scratch root.
#[derive(Clone)]
pub struct StreamingStorageFactory {
    root: PathBuf,
    budget: usize,
    counter: Arc<AtomicUsize>,
}

impl StreamingStorageFactory {
    pub fn new(root: impl Into<PathBuf>, budget: usize) -> Self {
        Self {
            root: root.into(),
            budget: budget.max(16 * 1024 * 1024),
            counter: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// The scratch root this process uses: one directory per PID, under the temp
/// dir, so a crashed run's leftovers are identifiable.
pub fn scratch_root() -> PathBuf {
    std::env::temp_dir().join(format!("reel-stream-{}", std::process::id()))
}

/// Remove scratch directories left by processes that are no longer running.
///
/// Spill files are unlinked as soon as they are created, so on Unix a crash
/// frees their space immediately. This clears the empty directories, and covers
/// platforms where a file cannot be unlinked while open.
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

impl StorageFactory for StreamingStorageFactory {
    type Storage = StreamingStorage;

    fn create(
        &self,
        _shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<StreamingStorage> {
        let lengths: Vec<u64> = metadata.file_infos.iter().map(|file| file.len).collect();
        let index = self.counter.fetch_add(1, Ordering::Relaxed);
        let dir = self.root.join(format!("stream-{index}"));
        StreamingStorage::new(dir, self.budget, lengths)
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

/// One file's scratch handle, opened lazily on the first spilled write.
struct Scratch {
    file: File,
}

struct Inner {
    dir: PathBuf,
    lengths: Vec<u64>,
    budget: usize,
    state: Mutex<State>,
}

struct State {
    /// File buffers held in RAM, indexed by file id. Each is the file's full
    /// length, so a write is a plain copy and a read never straddles.
    memory: HashMap<usize, Vec<u8>>,
    memory_bytes: usize,
    /// Files that have been spilled to scratch and are written there from now
    /// on. Once a file is here, every byte of it lives on disk.
    spilled: HashSet<usize>,
    scratch: HashMap<usize, Scratch>,
}

impl Inner {
    fn remove_dir(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.remove_dir();
    }
}

pub struct StreamingStorage {
    inner: Arc<Inner>,
}

impl StreamingStorage {
    pub(crate) fn new(dir: PathBuf, budget: usize, lengths: Vec<u64>) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            inner: Arc::new(Inner {
                dir,
                lengths,
                budget,
                state: Mutex::new(State {
                    memory: HashMap::new(),
                    memory_bytes: 0,
                    spilled: HashSet::new(),
                    scratch: HashMap::new(),
                }),
            }),
        })
    }

    fn length(&self, file_id: usize) -> anyhow::Result<usize> {
        self.inner
            .lengths
            .get(file_id)
            .map(|len| *len as usize)
            .ok_or_else(|| anyhow::anyhow!("file {file_id} is not in this torrent"))
    }

    /// Make room for `needed` bytes, spilling the largest in-memory file first.
    /// Returns false when the file simply cannot fit even with an empty cache.
    fn make_room(&self, state: &mut State, needed: usize) -> bool {
        if needed > self.inner.budget {
            return false;
        }
        while state.memory_bytes + needed > self.inner.budget {
            let Some((&victim, _)) = state
                .memory
                .iter()
                .max_by_key(|(_, buffer)| buffer.len())
            else {
                return false;
            };
            // Writing the buffer out cannot fail here; if it did, keeping it in
            // memory is the safer failure.
            if self.spill(state, victim).is_err() {
                return false;
            }
        }
        true
    }

    fn spill(&self, state: &mut State, file_id: usize) -> anyhow::Result<()> {
        if state.spilled.contains(&file_id) {
            return Ok(());
        }
        let Some(buffer) = state.memory.remove(&file_id) else {
            state.spilled.insert(file_id);
            return Ok(());
        };
        state.memory_bytes = state.memory_bytes.saturating_sub(buffer.len());
        let scratch = self.open_scratch(state, file_id)?;
        scratch.file.seek(SeekFrom::Start(0))?;
        scratch.file.write_all(&buffer)?;
        scratch.file.flush()?;
        state.spilled.insert(file_id);
        Ok(())
    }

    fn open_scratch<'a>(
        &self,
        state: &'a mut State,
        file_id: usize,
    ) -> anyhow::Result<&'a mut Scratch> {
        if !state.scratch.contains_key(&file_id) {
            let path = self.inner.dir.join(format!("file-{file_id}.bin"));
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)?;
            // Unlink the file but keep the handle. On Unix the data lives only
            // as long as the descriptor, so even `kill -9` cannot leave it on
            // disk: process death closes the descriptor and the kernel reclaims
            // it. On Windows the name stays and is removed on drop or by the
            // startup sweep.
            #[cfg(unix)]
            let _ = std::fs::remove_file(&path);
            state.scratch.insert(file_id, Scratch { file });
        }
        Ok(state
            .scratch
            .get_mut(&file_id)
            .expect("inserted just above"))
    }
}

impl TorrentStorage for StreamingStorage {
    fn init(
        &mut self,
        _shared: &ManagedTorrentShared,
        _metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(memory) = state.memory.get(&file_id) {
            let start = offset as usize;
            let end = start + buf.len();
            if end > memory.len() {
                anyhow::bail!("read past the end of file {file_id}");
            }
            buf.copy_from_slice(&memory[start..end]);
            return Ok(());
        }
        if !state.scratch.contains_key(&file_id) {
            anyhow::bail!("file {file_id} has not been written yet");
        }
        let scratch = self.open_scratch(&mut state, file_id)?;
        scratch.file.seek(SeekFrom::Start(offset))?;
        scratch.file.read_exact(buf)?;
        Ok(())
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let length = self.length(file_id)?;
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());

        if state.spilled.contains(&file_id) {
            let scratch = self.open_scratch(&mut state, file_id)?;
            scratch.file.seek(SeekFrom::Start(offset))?;
            scratch.file.write_all(buf)?;
            return Ok(());
        }

        if !state.memory.contains_key(&file_id) {
            if self.make_room(&mut state, length) {
                state.memory.insert(file_id, vec![0u8; length]);
                state.memory_bytes += length;
            } else {
                // Too large for the budget: go straight to scratch.
                state.spilled.insert(file_id);
                let scratch = self.open_scratch(&mut state, file_id)?;
                scratch.file.seek(SeekFrom::Start(offset))?;
                scratch.file.write_all(buf)?;
                return Ok(());
            }
        }

        let memory = state
            .memory
            .get_mut(&file_id)
            .expect("created or already present");
        let start = offset as usize;
        let end = start + buf.len();
        if end > memory.len() {
            anyhow::bail!("write past the end of file {file_id}");
        }
        memory[start..end].copy_from_slice(buf);
        Ok(())
    }

    fn remove_file(&self, file_id: usize, _filename: &Path) -> anyhow::Result<()> {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(buffer) = state.memory.remove(&file_id) {
            state.memory_bytes = state.memory_bytes.saturating_sub(buffer.len());
        }
        state.spilled.remove(&file_id);
        state.scratch.remove(&file_id);
        let path = self.inner.dir.join(format!("file-{file_id}.bin"));
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        let _ = std::fs::remove_dir(path);
        Ok(())
    }

    fn ensure_file_length(&self, _file_id: usize, _length: u64) -> anyhow::Result<()> {
        // Files are created at their full length the first time they are read
        // or written, so there is nothing to do here.
        Ok(())
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        // Sharing the same inner keeps the data alive across a pause; the last
        // handle dropped removes the scratch directory.
        Ok(Box::new(Self {
            inner: self.inner.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "reel-streaming-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn storage(dir: &Path, budget: usize, lengths: &[u64]) -> StreamingStorage {
        StreamingStorage::new(dir.to_path_buf(), budget, lengths.to_vec()).expect("storage")
    }

    #[test]
    fn a_small_file_stays_in_memory() {
        let dir = scratch_dir("memory");
        let storage = storage(&dir, 1_000, &[100]);
        storage.pwrite_all(0, 0, &[7u8; 100]).unwrap();

        assert_eq!(storage.inner.state.lock().unwrap().memory_bytes, 100);
        assert!(
            !storage.inner.dir.join("file-0.bin").exists(),
            "nothing should have spilled to disk"
        );

        let mut out = [0u8; 100];
        storage.pread_exact(0, 0, &mut out).unwrap();
        assert!(out.iter().all(|byte| *byte == 7));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_larger_than_the_budget_spills_whole() {
        let dir = scratch_dir("spill");
        let storage = storage(&dir, 100, &[400]);
        storage.pwrite_all(0, 0, &[9u8; 400]).unwrap();

        let state = storage.inner.state.lock().unwrap();
        assert_eq!(state.memory_bytes, 0, "the file cannot fit, so it spilled");
        assert!(state.spilled.contains(&0));
        drop(state);

        let mut out = [0u8; 400];
        storage.pread_exact(0, 0, &mut out).unwrap();
        assert!(out.iter().all(|byte| *byte == 9));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_file_spills_the_first_to_stay_under_budget() {
        let dir = scratch_dir("evict");
        let storage = storage(&dir, 1_000, &[600, 600]);
        storage.pwrite_all(0, 0, &[1u8; 600]).unwrap();
        storage.pwrite_all(1, 0, &[2u8; 600]).unwrap();

        let state = storage.inner.state.lock().unwrap();
        assert!(
            state.memory_bytes <= 1_000,
            "budget must be respected, got {}",
            state.memory_bytes
        );
        assert!(state.spilled.contains(&0), "the older file made room");
        assert!(state.memory.contains_key(&1), "the new file is in memory");
        drop(state);

        // Both files remain fully readable after the spill.
        let mut first = [0u8; 600];
        storage.pread_exact(0, 0, &mut first).unwrap();
        assert!(first.iter().all(|byte| *byte == 1));
        let mut second = [0u8; 600];
        storage.pread_exact(1, 0, &mut second).unwrap();
        assert!(second.iter().all(|byte| *byte == 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_writes_and_reads_line_up() {
        let dir = scratch_dir("partial");
        let storage = storage(&dir, 1_000, &[10]);
        storage.pwrite_all(0, 3, &[1, 2, 3]).unwrap();
        storage.pwrite_all(0, 0, &[9, 9, 9]).unwrap();

        let mut out = [0u8; 6];
        storage.pread_exact(0, 0, &mut out).unwrap();
        assert_eq!(out, [9, 9, 9, 1, 2, 3]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_spilled_file_is_unlinked_while_still_readable() {
        let dir = scratch_dir("unlink");
        let storage = storage(&dir, 100, &[400]);
        storage.pwrite_all(0, 0, &[5u8; 400]).unwrap();

        // The name is gone, so a crash cannot leave the bytes on disk...
        let names: Vec<String> = std::fs::read_dir(&storage.inner.dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.is_empty(),
            "the spill file must be unlinked, found {names:?}"
        );

        // ...but the open handle still serves the data.
        let mut out = [0u8; 400];
        storage.pread_exact(0, 0, &mut out).unwrap();
        assert!(out.iter().all(|byte| *byte == 5));
        let _ = std::fs::remove_dir_all(&dir);
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
    fn the_scratch_directory_is_removed_when_storage_is_dropped() {
        let dir = scratch_dir("cleanup");
        let storage = storage(&dir, 100, &[400]);
        storage.pwrite_all(0, 0, &[0u8; 400]).unwrap();
        let path = storage.inner.dir.clone();
        assert!(path.exists());

        drop(storage);
        assert!(!path.exists(), "the scratch directory is temporary");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
