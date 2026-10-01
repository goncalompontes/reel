//! A library that outlives the torrent session.
//!
//! The library used to *be* the session: every torrent stayed in librqbit, which
//! is also what made it reappear after a restart. That is wrong once streaming
//! is temporary — a streamed torrent is removed when playback stops, so the
//! title would vanish with it.
//!
//! This store is the durable part. It remembers what was added, what it looks
//! like, whether the user asked to keep it, and — by way of a saved `.torrent`
//! in the backend — enough to bring it back on demand. The engine then only
//! holds what is actively streaming or downloading.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::media::{is_audio_file, is_subtitle_file, is_video_file};
use crate::model::FileView;

/// One file, as the store remembers it. Deliberately not a [`FileView`]: a
/// `FileView` carries a stream URL with a session id that will not exist next
/// run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredFile {
    pub id: usize,
    pub path: String,
    pub name: String,
    pub length: u64,
    pub is_video: bool,
    pub is_audio: bool,
    pub is_subtitle: bool,
}

impl StoredFile {
    pub fn from_view(file: &FileView) -> Self {
        Self {
            id: file.id,
            path: file.path.clone(),
            name: file.name.clone(),
            length: file.length,
            is_video: is_video_file(&file.name),
            is_audio: is_audio_file(&file.name),
            is_subtitle: is_subtitle_file(&file.name),
        }
    }

    pub fn is_media(&self) -> bool {
        self.is_video || self.is_audio
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryEntry {
    /// Stable across restarts and across a torrent being removed and re-added.
    pub id: usize,
    pub info_hash: String,
    /// How to re-add when no `.torrent` was saved, e.g. a magnet URI.
    #[serde(default)]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub files: Vec<StoredFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_file_id: Option<usize>,
    /// Files to fetch when this is a download. Empty means "stream only".
    #[serde(default)]
    pub selected_files: Vec<usize>,
    /// The user asked to keep this on disk, so it is re-added at startup.
    #[serde(default)]
    pub downloading: bool,
    pub added_at: i64,
}

impl LibraryEntry {
    pub fn file(&self, file_id: usize) -> Option<&StoredFile> {
        self.files.iter().find(|file| file.id == file_id)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LibraryStore {
    #[serde(default)]
    pub next_id: usize,
    #[serde(default)]
    pub entries: Vec<LibraryEntry>,
}

impl LibraryStore {
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "ignoring unreadable library");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &LibraryEntry> {
        self.entries.iter()
    }

    pub fn entry(&self, id: usize) -> Option<&LibraryEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    pub fn entry_mut(&mut self, id: usize) -> Option<&mut LibraryEntry> {
        self.entries.iter_mut().find(|entry| entry.id == id)
    }

    pub fn by_hash(&self, info_hash: &str) -> Option<&LibraryEntry> {
        self.entries
            .iter()
            .find(|entry| entry.info_hash.eq_ignore_ascii_case(info_hash))
    }

    pub fn remove(&mut self, id: usize) -> Option<LibraryEntry> {
        let index = self.entries.iter().position(|entry| entry.id == id)?;
        Some(self.entries.remove(index))
    }

    /// The entry for this info hash, assigning a stable id if it is new.
    pub fn upsert(
        &mut self,
        info_hash: &str,
        source: &str,
        name: Option<String>,
        added_at: i64,
    ) -> usize {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|entry| entry.info_hash.eq_ignore_ascii_case(info_hash))
        {
            if !source.is_empty() {
                existing.source = source.to_string();
            }
            if name.is_some() {
                existing.name = name;
            }
            return existing.id;
        }

        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(LibraryEntry {
            id,
            info_hash: info_hash.to_string(),
            source: source.to_string(),
            name,
            files: Vec::new(),
            primary_file_id: None,
            selected_files: Vec::new(),
            downloading: false,
            added_at,
        });
        id
    }

    /// Where a saved `.torrent` for an entry lives, so it can be re-added
    /// offline.
    pub fn torrent_path(data_dir: &Path, entry: &LibraryEntry) -> PathBuf {
        data_dir
            .join("torrents")
            .join(format!("{}.torrent", entry.info_hash.to_ascii_lowercase()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("reel-library-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn ids_are_stable_and_upsert_is_idempotent() {
        let mut store = LibraryStore::default();
        let first = store.upsert("aaa", "magnet:?aaa", Some("A".into()), 1);
        let again = store.upsert("aaa", "magnet:?aaa", Some("A".into()), 2);
        let other = store.upsert("bbb", "magnet:?bbb", None, 3);

        assert_eq!(first, again, "the same hash keeps its id");
        assert_ne!(first, other);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn a_store_round_trips_through_disk() {
        let dir = scratch("roundtrip");
        let path = dir.join("library.json");

        let mut store = LibraryStore::default();
        let id = store.upsert("ccc", "magnet:?ccc", Some("C".into()), 7);
        store.entry_mut(id).unwrap().downloading = true;
        store.entry_mut(id).unwrap().selected_files = vec![0, 2];
        store.save(&path).unwrap();

        let loaded = LibraryStore::load(&path);
        assert_eq!(loaded.entry(id).unwrap().info_hash, "ccc");
        assert!(loaded.entry(id).unwrap().downloading);
        assert_eq!(loaded.entry(id).unwrap().selected_files, vec![0, 2]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removing_an_entry_frees_its_id_but_keeps_the_counter() {
        let mut store = LibraryStore::default();
        let id = store.upsert("ddd", "", None, 0);
        assert!(store.remove(id).is_some());
        let next = store.upsert("eee", "", None, 0);
        assert_ne!(next, id, "a removed id is not reused");
    }

    #[test]
    fn a_corrupt_store_starts_empty_rather_than_failing() {
        let dir = scratch("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("library.json"), b"not json").unwrap();
        assert!(LibraryStore::load(&dir.join("library.json")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
