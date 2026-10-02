//! The unit the user actually thinks in: a film, an episode, a season.
//!
//! Download intent and watch state belong to *this*, not to a torrent. A
//! torrent is a source that happens to hold the bytes today; the app may
//! re-add it, replace it, or find the same episode in a different release. The
//! store here is what survives all of that.
//!
//! # Stable references
//!
//! A file is referenced by `(info hash, path inside the torrent)`. The engine's
//! `file_id` is a positional index that can change when a torrent is re-added
//! (and is meaningless across releases), so it is never persisted. Resolving a
//! reference to the live `file_id` happens at the point of use.
//!
//! The module is pure: no engine, no network, no UI. It is persisted by the
//! desktop backend through [`MediaStore::load`] / [`MediaStore::save`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::WatchProgress;
use crate::work::{Work, WorkEpisode};

/// A file that backs some media, identified by what does not change across a
/// re-add: the torrent's info hash and the file's `/`-separated path in it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MediaRef {
    pub info_hash: String,
    pub path: String,
}

impl MediaRef {
    pub fn new(info_hash: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            info_hash: info_hash.into().into(),
            path: path.into().into(),
        }
    }
}

/// One media's download: the files it is kept as, and which copy was chosen.
///
/// `files` is normally one file (a film, an episode) but a "whole season"
/// action may fold several under one record. It is a set of references, so it
/// can survive the torrent being removed and brought back.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DownloadRecord {
    #[serde(default)]
    pub files: Vec<MediaRef>,
    /// The copy the user picked last, so a source switch keeps the choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen: Option<MediaRef>,
    #[serde(default)]
    pub updated_at: i64,
}

impl DownloadRecord {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn references(&self, info_hash: &str) -> bool {
        self.files
            .iter()
            .any(|file| file.info_hash.eq_ignore_ascii_case(info_hash))
    }
}

/// Persisted, media-keyed download and watch state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MediaStore {
    #[serde(default)]
    pub downloads: BTreeMap<String, DownloadRecord>,
    /// Where the user got to, by media key. This is what makes resume follow
    /// an episode to whichever release now holds it.
    #[serde(default)]
    pub watch: BTreeMap<String, WatchProgress>,
}

impl MediaStore {
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "ignoring unreadable media store");
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

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("media.json")
    }

    pub fn is_empty(&self) -> bool {
        self.downloads.is_empty() && self.watch.is_empty()
    }

    pub fn len(&self) -> usize {
        self.downloads.len()
    }

    /// Record where the user got to for a media, replacing any previous value.
    pub fn record_watch(&mut self, key: impl Into<String>, progress: WatchProgress) {
        self.watch.insert(key.into(), progress);
    }

    /// Where the user got to for a media, if it has been watched.
    pub fn watch(&self, key: &str) -> Option<&WatchProgress> {
        self.watch.get(key)
    }

    pub fn forget_watch(&mut self, key: &str) {
        self.watch.remove(key);
    }

    /// Whether any media has a saved position (used by the migration check).
    pub fn has_downloads(&self) -> bool {
        !self.downloads.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&DownloadRecord> {
        self.downloads.get(key)
    }

    /// Whether some copy of this media is being kept.
    pub fn is_downloaded(&self, key: &str) -> bool {
        self.get(key).is_some_and(|record| !record.is_empty())
    }

    /// Add `files` to what is kept for `key`. Only the last one is remembered
    /// as the chosen copy; keeping a second release does not drop the first,
    /// because each copy is downloadable and removable on its own.
    pub fn keep(&mut self, key: impl Into<String>, files: Vec<MediaRef>, at: i64) {
        let chosen = files.last().cloned();
        let record = self.downloads.entry(key.into()).or_default();
        for file in files {
            if !record.files.contains(&file) {
                record.files.push(file);
            }
        }
        if chosen.is_some() {
            record.chosen = chosen;
        }
        record.updated_at = at;
    }

    /// Forget a whole media's download.
    pub fn stop(&mut self, key: &str) -> Option<DownloadRecord> {
        self.downloads.remove(key)
    }

    /// Drop one file from a media's download, keeping the rest.
    pub fn stop_file(&mut self, key: &str, file: &MediaRef) -> bool {
        let Some(record) = self.downloads.get_mut(key) else {
            return false;
        };
        let before = record.files.len();
        record.files.retain(|candidate| candidate != file);
        if record.files.is_empty() {
            self.downloads.remove(key);
            return before > 0;
        }
        if record.chosen.as_ref() == Some(file) {
            record.chosen = record.files.first().cloned();
        }
        before != record.files.len()
    }

    /// Every media key whose download references this torrent.
    pub fn keys_for_info_hash<'a>(&'a self, info_hash: &'a str) -> impl Iterator<Item = &'a str> {
        self.downloads
            .iter()
            .filter(move |(_, record)| record.references(info_hash))
            .map(|(key, _)| key.as_str())
    }

    /// Every file reference wanted from this torrent, across all media.
    pub fn refs_for_info_hash<'a>(&'a self, info_hash: &'a str) -> impl Iterator<Item = &'a MediaRef> {
        self.downloads
            .values()
            .flat_map(|record| record.files.iter())
            .filter(move |file| file.info_hash.eq_ignore_ascii_case(info_hash))
    }

    /// Adopt old per-torrent download state. Used once, when the media store is
    /// empty but the library already marks a title as a download.
    pub fn adopt_legacy(&mut self, info_hash: &str, files: Vec<MediaRef>, at: i64) {
        if files.is_empty() {
            return;
        }
        self.downloads
            .entry(legacy_key(info_hash))
            .or_insert_with(|| DownloadRecord {
                files,
                chosen: None,
                updated_at: at,
            });
    }
}

/// Key for a film within its work.
pub fn movie_key(work: &Work) -> String {
    format!("{}#movie", work.key)
}

/// Key for one episode within its work.
pub fn episode_key(work: &Work, episode: &WorkEpisode) -> String {
    episode_key_parts(
        &work.key,
        episode.season,
        episode.episode,
        episode.air_date.as_deref(),
    )
}

/// The same key, without a [`Work`] in hand.
pub fn episode_key_parts(
    work_key: &str,
    season: Option<u32>,
    episode: Option<u32>,
    air_date: Option<&str>,
) -> String {
    format!("{work_key}#{}", episode_token(season, episode, air_date))
}

/// A fallback key for state migrated from the old torrent-level store, when the
/// media grouping is not known.
pub fn legacy_key(info_hash: &str) -> String {
    format!("legacy:{info_hash}")
}

fn episode_token(season: Option<u32>, episode: Option<u32>, air_date: Option<&str>) -> String {
    match (season, episode) {
        (Some(season), Some(episode)) => format!("s{season:02}e{episode:02}"),
        (None, Some(episode)) => format!("e{episode:02}"),
        (Some(season), None) => format!("s{season:02}"),
        (None, None) => match air_date {
            Some(date) => format!("date:{date}"),
            None => "unknown".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(pairs: &[(&str, &str)]) -> Vec<MediaRef> {
        pairs
            .iter()
            .map(|(hash, path)| MediaRef::new(*hash, *path))
            .collect()
    }

    #[test]
    fn keeping_a_second_copy_does_not_drop_the_first() {
        let mut store = MediaStore::default();
        store.keep("show#s01e01", refs(&[("aaa", "E01.mkv")]), 1);
        assert!(store.is_downloaded("show#s01e01"));

        // Keeping a second release adds it; each copy stays independently
        // removable, and the newest is remembered as the chosen one.
        store.keep("show#s01e01", refs(&[("bbb", "E01.mkv")]), 2);
        assert_eq!(store.get("show#s01e01").unwrap().files.len(), 2);
        assert_eq!(
            store.get("show#s01e01").unwrap().chosen,
            Some(MediaRef::new("bbb", "E01.mkv"))
        );
        assert_eq!(store.refs_for_info_hash("aaa").count(), 1);
        assert_eq!(store.refs_for_info_hash("bbb").count(), 1);

        // Removing one copy leaves the other.
        store.keep(
            "show#s01e01",
            vec![MediaRef::new("aaa", "E01.mkv"), MediaRef::new("bbb", "E01.mkv")],
            2,
        );
        store.stop_file("show#s01e01", &MediaRef::new("bbb", "E01.mkv"));
        assert_eq!(
            store.get("show#s01e01").unwrap().files,
            refs(&[("aaa", "E01.mkv")])
        );
    }

    #[test]
    fn stopping_a_file_keeps_the_rest_of_the_record() {
        let mut store = MediaStore::default();
        store.keep(
            "show#season1",
            refs(&[("aaa", "E01.mkv"), ("aaa", "E02.mkv")]),
            1,
        );
        let removed = store.stop_file("show#season1", &MediaRef::new("aaa", "E01.mkv"));
        assert!(removed);
        let record = store.get("show#season1").unwrap();
        assert_eq!(record.files, refs(&[("aaa", "E02.mkv")]));

        // Removing the last file drops the record.
        store.stop_file("show#season1", &MediaRef::new("aaa", "E02.mkv"));
        assert!(!store.is_downloaded("show#season1"));
    }

    #[test]
    fn keys_and_refs_are_grouped_by_source() {
        let mut store = MediaStore::default();
        store.keep("movie", refs(&[("aaa", "Film.mkv")]), 1);
        store.keep("ep", refs(&[("aaa", "E01.mkv"), ("bbb", "E01.mkv")]), 1);

        let mut from_aaa: Vec<&str> = store.keys_for_info_hash("aaa").collect();
        from_aaa.sort_unstable();
        assert_eq!(from_aaa, ["ep", "movie"]);
        assert_eq!(store.refs_for_info_hash("aaa").count(), 2);
        assert_eq!(store.refs_for_info_hash("bbb").count(), 1);
    }

    #[test]
    fn a_store_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("reel-media-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = MediaStore::path(&dir);

        let mut store = MediaStore::default();
        store.keep("show#s01e01", refs(&[("hash", "S01E01.mkv")]), 7);
        store.save(&path).unwrap();

        let loaded = MediaStore::load(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded.get("show#s01e01").unwrap().files,
            refs(&[("hash", "S01E01.mkv")])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_store_starts_empty() {
        let dir = std::env::temp_dir().join(format!("reel-media-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(MediaStore::path(&dir), b"not json").unwrap();
        assert!(MediaStore::load(&MediaStore::path(&dir)).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_state_is_adopted_once() {
        let mut store = MediaStore::default();
        store.adopt_legacy("hash", refs(&[("hash", "E01.mkv")]), 1);
        assert!(store.is_downloaded(&legacy_key("hash")));
        assert_eq!(store.refs_for_info_hash("hash").count(), 1);

        // Adopting again does not overwrite a user's later choice: the legacy
        // record keeps the file it was adopted with.
        store.keep("show#s01e01", refs(&[("hash", "E01.mkv")]), 2);
        store.adopt_legacy("hash", refs(&[("hash", "E02.mkv")]), 3);
        assert_eq!(
            store.get(&legacy_key("hash")).unwrap().files,
            refs(&[("hash", "E01.mkv")]),
            "adoption must not overwrite the adopted record"
        );
        assert_eq!(
            store.get("show#s01e01").unwrap().files,
            refs(&[("hash", "E01.mkv")])
        );
    }

    #[test]
    fn episode_tokens_are_stable_and_distinct() {
        assert_eq!(
            episode_key_parts("show", Some(1), Some(2), None),
            "show#s01e02"
        );
        assert_eq!(episode_key_parts("show", None, Some(5), None), "show#e05");
        assert_eq!(
            episode_key_parts("show", None, None, Some("2024-01-15")),
            "show#date:2024-01-15"
        );
        assert_ne!(
            episode_key_parts("show", Some(1), Some(2), None),
            episode_key_parts("show", Some(1), Some(3), None)
        );
    }

    #[test]
    fn watch_is_keyed_by_media_not_by_source() {
        use crate::model::WatchProgress;
        let mut store = MediaStore::default();
        let progress = WatchProgress {
            position: 300.0,
            duration: Some(1000.0),
            updated_at: 5,
            file_name: Some("E01.mkv".into()),
            title: None,
        };
        store.record_watch("show#s01e01", progress);

        // A different release of the same episode computes the same key, so it
        // reads the same position.
        assert_eq!(store.watch("show#s01e01").unwrap().position, 300.0);
        store.forget_watch("show#s01e01");
        assert!(store.watch("show#s01e01").is_none());
    }
}
