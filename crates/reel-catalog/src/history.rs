//! Watch history, so the app can offer "continue watching" and resume playback.
//!
//! Keyed by torrent info hash rather than torrent id: ids are assigned per
//! session, so a restart would otherwise lose every saved position. The file is
//! plain JSON, small, and rewritten atomically.

use std::collections::BTreeMap;

/// Watch positions are stored per file: `<info hash>#<file id>`.
///
/// A series is one torrent with many episodes, and one position for the whole
/// torrent would resume episode 1 at episode 2's timestamp. Entries without a
/// `#` are the older format, still read so an existing history is not lost.
const FILE_SEPARATOR: char = '#';

fn file_key(info_hash: &str, file_id: usize) -> String {
    format!("{info_hash}{FILE_SEPARATOR}{file_id}")
}

fn key_of(key: &str) -> &str {
    match key.split_once(FILE_SEPARATOR) {
        Some((hash, _)) => hash,
        None => key,
    }
}

fn file_id_of(key: &str) -> Option<usize> {
    key.split_once(FILE_SEPARATOR)?.1.parse().ok()
}
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::model::{FINISHED_FRACTION, WatchProgress};

#[derive(Debug, Default, Serialize, Deserialize)]
struct HistoryFile {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    entries: BTreeMap<String, PersistedProgress>,
}

fn default_version() -> u32 {
    1
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedProgress {
    position: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duration: Option<f64>,
    updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
}

impl From<PersistedProgress> for WatchProgress {
    fn from(p: PersistedProgress) -> Self {
        WatchProgress {
            position: p.position,
            duration: p.duration,
            updated_at: p.updated_at,
            file_name: p.file_name,
            title: p.title,
        }
    }
}

impl From<&WatchProgress> for PersistedProgress {
    fn from(p: &WatchProgress) -> Self {
        Self {
            position: p.position,
            duration: p.duration,
            updated_at: p.updated_at,
            file_name: p.file_name.clone(),
            title: p.title.clone(),
        }
    }
}

pub struct WatchHistory {
    path: PathBuf,
    entries: BTreeMap<String, WatchProgress>,
}

impl WatchHistory {
    /// Load from disk. A missing or unreadable file yields an empty history:
    /// losing watch positions must never stop the app from starting.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let entries = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<HistoryFile>(&bytes) {
                Ok(file) => file
                    .entries
                    .into_iter()
                    .map(|(key, value)| (key, value.into()))
                    .collect(),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "ignoring unreadable watch history");
                    BTreeMap::new()
                }
            },
            Err(_) => BTreeMap::new(),
        };
        Self { path, entries }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The torrent's most recent position, for "continue watching".
    pub fn get(&self, info_hash: &str) -> Option<&WatchProgress> {
        // Prefer a per-file entry: it is the newer format.
        self.entries
            .iter()
            .filter(|(key, _)| key_of(key) == info_hash)
            .map(|(_, progress)| progress)
            .max_by_key(|progress| progress.updated_at)
    }

    /// The position saved for one file of a torrent.
    pub fn get_file(&self, info_hash: &str, file_id: usize) -> Option<&WatchProgress> {
        self.entries
            .get(&file_key(info_hash, file_id))
            .or_else(|| if file_id == 0 { self.entries.get(info_hash) } else { None })
    }

    /// Every file of a torrent that has a saved position.
    pub fn per_file(&self, info_hash: &str) -> BTreeMap<usize, WatchProgress> {
        self.entries
            .iter()
            .filter(|(key, _)| key_of(key) == info_hash)
            .filter_map(|(key, progress)| file_id_of(key).map(|id| (id, progress.clone())))
            .collect()
    }

    /// Note that playback of `info_hash` reached `position`.
    ///
    /// Positions that are effectively zero are ignored: merely opening a film
    /// should not create a continue-watching entry.
    pub fn record(
        &mut self,
        info_hash: &str,
        file_name: Option<String>,
        title: Option<String>,
        position: f64,
        duration: Option<f64>,
    ) -> std::io::Result<()> {
        self.record_file(info_hash, 0, file_name, title, position, duration)
    }

    /// Record a position for one file, so a series resumes the episode that was
    /// actually being watched rather than applying one episode's timestamp to
    /// another.
    pub fn record_file(
        &mut self,
        info_hash: &str,
        file_id: usize,
        file_name: Option<String>,
        title: Option<String>,
        position: f64,
        duration: Option<f64>,
    ) -> std::io::Result<()> {
        if info_hash.is_empty() || !position.is_finite() || position < 1.0 {
            return Ok(());
        }

        let key = file_key(info_hash, file_id);
        let duration = duration.filter(|d| d.is_finite() && *d > 0.0);
        let previous = self.entries.get(&key).cloned();
        let progress = WatchProgress {
            position,
            duration,
            updated_at: now_unix(),
            file_name: file_name.or_else(|| previous.as_ref().and_then(|p| p.file_name.clone())),
            title: title.or_else(|| previous.as_ref().and_then(|p| p.title.clone())),
        };

        self.entries.insert(key, progress);
        // Keep the torrent-level record in step, for "continue watching" and
        // for readers of the older format.
        if file_id == 0 {
            self.entries.remove(info_hash);
        }
        self.save()
    }

    /// Mark a title as watched through to the end.
    pub fn mark_finished(
        &mut self,
        info_hash: &str,
        file_name: Option<String>,
        title: Option<String>,
    ) -> std::io::Result<()> {
        self.mark_file_finished(info_hash, 0, file_name, title)
    }

    pub fn mark_file_finished(
        &mut self,
        info_hash: &str,
        file_id: usize,
        file_name: Option<String>,
        title: Option<String>,
    ) -> std::io::Result<()> {
        let duration = self
            .get_file(info_hash, file_id)
            .and_then(|p| p.duration)
            .filter(|d| *d > 0.0);

        // Land clearly past the threshold rather than exactly on it, so the
        // entry cannot end up one ulp short of "finished".
        let position = duration
            .map(|d| (d * FINISHED_FRACTION).max(d - 1.0))
            .unwrap_or(FINISHED_FRACTION + 1.0);

        self.record_file(info_hash, file_id, file_name, title, position.max(1.0), duration)
    }

    pub fn forget(&mut self, info_hash: &str) -> std::io::Result<()> {
        let before = self.entries.len();
        self.entries.retain(|key, _| key_of(key) != info_hash);
        if self.entries.len() != before {
            return self.save();
        }
        Ok(())
    }

    /// Forget one file's position, leaving the rest of the series alone.
    pub fn forget_file(&mut self, info_hash: &str, file_id: usize) -> std::io::Result<()> {
        if self.entries.remove(&file_key(info_hash, file_id)).is_some() {
            return self.save();
        }
        Ok(())
    }

    /// Most recently watched first, one entry per torrent.
    ///
    /// A series stores a position per episode; "continue watching" wants the
    /// torrent, so the newest of its files stands for it.
    pub fn recent(&self, limit: usize, only_resumable: bool) -> Vec<(String, &WatchProgress)> {
        let mut newest: BTreeMap<&str, &WatchProgress> = BTreeMap::new();
        for (key, progress) in &self.entries {
            let hash = key_of(key);
            let keep = newest
                .get(hash)
                .map(|current| current.updated_at <= progress.updated_at)
                .unwrap_or(true);
            if keep {
                newest.insert(hash, progress);
            }
        }

        let mut items: Vec<(String, &WatchProgress)> = newest
            .into_iter()
            .filter(|(_, progress)| !only_resumable || progress.is_resumable())
            .map(|(hash, progress)| (hash.to_string(), progress))
            .collect();
        items.sort_by(|a, b| {
            b.1.updated_at
                .cmp(&a.1.updated_at)
                .then_with(|| a.0.cmp(&b.0))
        });
        items.truncate(limit);
        items
    }

    pub fn save(&self) -> std::io::Result<()> {
        let file = HistoryFile {
            version: default_version(),
            entries: self
                .entries
                .iter()
                .map(|(key, value)| (key.clone(), PersistedProgress::from(value)))
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file)?;

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let temp = self.path.with_extension("tmp");
        std::fs::write(&temp, bytes)?;
        std::fs::rename(&temp, &self.path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("reel-history-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("history.json")
    }

    #[test]
    fn each_episode_keeps_its_own_position() {
        let mut history = WatchHistory::load(scratch("per-file"));

        // One torrent, three episodes.
        history
            .record_file("show", 0, Some("e01.mkv".into()), None, 1200.0, Some(2800.0))
            .unwrap();
        history
            .record_file("show", 1, Some("e02.mkv".into()), None, 300.0, Some(2800.0))
            .unwrap();

        assert_eq!(history.get_file("show", 0).unwrap().position, 1200.0);
        assert_eq!(history.get_file("show", 1).unwrap().position, 300.0);
        assert_eq!(history.get_file("show", 2), None);

        // Finishing an episode must not disturb the other one.
        history.mark_file_finished("show", 0, None, None).unwrap();
        assert!(history.get_file("show", 0).unwrap().is_finished());
        assert_eq!(history.get_file("show", 1).unwrap().position, 300.0);

        // Continue watching is per torrent, and points at the newest episode.
        let recent = history.recent(10, true);
        assert_eq!(recent.len(), 1, "one row per torrent, not per episode");
        assert_eq!(recent[0].0, "show");
        assert_eq!(recent[0].1.position, 300.0);

        // And only the finished episode is forgotten by a per-file forget.
        history.forget_file("show", 0).unwrap();
        assert_eq!(history.get_file("show", 0), None);
        assert!(history.get_file("show", 1).is_some());
    }

    #[test]
    fn an_older_history_without_file_ids_is_still_read() {
        // A file written before positions were per-file.
        let path = scratch("legacy");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            br#"{"version":1,"entries":{"legacyhash":{"position":900.0,"duration":1000.0,"updated_at":5,"file_name":"old.mkv"}}}"#,
        )
        .unwrap();

        let history = WatchHistory::load(&path);
        assert_eq!(history.get("legacyhash").unwrap().position, 900.0);
        assert_eq!(history.get_file("legacyhash", 0).unwrap().position, 900.0);
        assert_eq!(history.recent(5, false).len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_missing_file_is_an_empty_history() {
        let history = WatchHistory::load(scratch("missing"));
        assert!(history.is_empty());
        assert_eq!(history.len(), 0);
    }

    #[test]
    fn positions_survive_a_reload() {
        let path = scratch("persist");

        let mut history = WatchHistory::load(&path);
        history
            .record("hash-a", Some("movie.mkv".into()), Some("The Matrix".into()), 300.0, Some(8160.0))
            .unwrap();
        history
            .record("hash-b", Some("other.mkv".into()), None, 42.0, Some(600.0))
            .unwrap();
        assert_eq!(history.len(), 2);

        let reloaded = WatchHistory::load(&path);
        let progress = reloaded.get("hash-a").expect("hash-a");
        assert_eq!(progress.position, 300.0);
        assert_eq!(progress.duration, Some(8160.0));
        assert_eq!(progress.file_name.as_deref(), Some("movie.mkv"));
        assert_eq!(progress.title.as_deref(), Some("The Matrix"));
        assert!(progress.is_resumable());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn opening_a_file_does_not_create_an_entry() {
        let mut history = WatchHistory::load(scratch("zero"));
        history.record("hash", None, None, 0.0, Some(100.0)).unwrap();
        history.record("hash", None, None, 0.4, Some(100.0)).unwrap();
        history
            .record("hash", None, None, f64::NAN, Some(100.0))
            .unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn continuing_keeps_the_original_file_name_when_not_given() {
        let mut history = WatchHistory::load(scratch("keep-name"));
        history
            .record("hash", Some("first.mkv".into()), Some("Title".into()), 60.0, Some(1000.0))
            .unwrap();
        history.record("hash", None, None, 120.0, None).unwrap();

        let progress = history.get("hash").unwrap();
        assert_eq!(progress.position, 120.0);
        assert_eq!(progress.file_name.as_deref(), Some("first.mkv"));
        assert_eq!(progress.title.as_deref(), Some("Title"));
    }

    #[test]
    fn recent_is_newest_first_and_can_filter_to_resumable() {
        let mut history = WatchHistory::load(scratch("recent"));

        // Two resumable, one finished, one barely started.
        history.record("old", None, None, 100.0, Some(1000.0)).unwrap();
        history.record("finished", None, None, 950.0, Some(1000.0)).unwrap();
        history.record("tiny", None, None, 5.0, Some(1000.0)).unwrap();
        history.record("newest", None, None, 200.0, Some(1000.0)).unwrap();

        // updated_at has one-second granularity, so set it explicitly.
        {
            let entries = &mut history.entries;
            for (hash, at) in [("old", 1_000), ("finished", 4_000), ("tiny", 5_000), ("newest", 3_000)] {
                entries
                    .get_mut(&file_key(hash, 0))
                    .unwrap_or_else(|| panic!("{hash} should be recorded"))
                    .updated_at = at;
            }
        }

        let all: Vec<String> = history.recent(10, false).into_iter().map(|(k, _)| k).collect();
        assert_eq!(all, ["tiny", "finished", "newest", "old"]);

        let resumable: Vec<String> = history.recent(10, true).into_iter().map(|(k, _)| k).collect();
        assert_eq!(resumable, ["newest", "old"], "finished and unstarted are excluded");

        assert_eq!(history.recent(1, true).len(), 1);
    }

    #[test]
    fn marking_finished_removes_it_from_continue_watching() {
        let mut history = WatchHistory::load(scratch("finished"));
        history.record("hash", None, None, 100.0, Some(1000.0)).unwrap();
        assert!(history.get("hash").unwrap().is_resumable());

        history.mark_finished("hash", None, None).unwrap();
        let progress = history.get("hash").unwrap();
        assert!(progress.is_finished());
        assert!(!progress.is_resumable());
        assert!(history.recent(10, true).is_empty());
    }

    #[test]
    fn forgetting_removes_it_from_disk() {
        let path = scratch("forget");
        let mut history = WatchHistory::load(&path);
        history.record("hash", None, None, 10.0, Some(100.0)).unwrap();
        history.forget("hash").unwrap();

        assert!(history.is_empty());
        assert!(WatchHistory::load(&path).is_empty());
        // Forgetting something unknown is fine.
        history.forget("nope").unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_corrupt_history_file_does_not_stop_startup() {
        let path = scratch("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json at all").unwrap();

        let history = WatchHistory::load(&path);
        assert!(history.is_empty());

        // And it can still be written back over.
        let mut history = history;
        history.record("hash", None, None, 30.0, Some(100.0)).unwrap();
        assert_eq!(WatchHistory::load(&path).len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
