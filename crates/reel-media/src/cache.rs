//! An LRU-ish on-disk cache for converted files.
//!
//! Conversion is expensive, and a cast can be restarted (or the same title cast
//! twice); keeping the output keyed by source identity avoids re-encoding.
//! Eviction is by age once the cache exceeds its byte limit.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::Result;
use crate::plan::MediaPlan;

/// Identifies the converted output for one (file, plan) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    /// Torrent info hash.
    pub info_hash: String,
    /// File index within the torrent.
    pub file_id: u64,
    /// Source length in bytes; changes when the file is replaced.
    pub length: u64,
    /// Short tag for the plan, e.g. `remux` or `h264-1080p`.
    pub tag: String,
}

impl CacheKey {
    /// Build a key directly.
    pub fn new(
        info_hash: impl Into<String>,
        file_id: u64,
        length: u64,
        tag: impl Into<String>,
    ) -> CacheKey {
        CacheKey {
            info_hash: info_hash.into(),
            file_id,
            length,
            tag: tag.into(),
        }
    }

    /// Build a key for a delivery plan.
    ///
    /// `audio_index` is the chosen audio stream, when the user picked one: a
    /// different track produces different bytes, so it must not reuse the
    /// default track's cached conversion.
    pub fn for_plan(
        info_hash: impl Into<String>,
        file_id: u64,
        length: u64,
        plan: &MediaPlan,
        audio_index: Option<u32>,
    ) -> CacheKey {
        let mut tag = match plan {
            MediaPlan::Remux => "remux".to_string(),
            MediaPlan::Transcode { max_height } => format!("h264-{max_height}p"),
            MediaPlan::Original => "original".to_string(),
            MediaPlan::Unavailable { .. } => "unavailable".to_string(),
        };
        if let Some(index) = audio_index {
            tag.push_str(&format!("-a{index}"));
        }
        CacheKey::new(info_hash, file_id, length, tag)
    }

    /// The cache file name for this key.
    pub fn file_name(&self) -> String {
        format!(
            "{}-{}-{}-{}.mp4",
            sanitize(&self.info_hash),
            self.file_id,
            self.length,
            sanitize(&self.tag)
        )
    }
}

/// The conversion cache directory.
#[derive(Debug, Clone)]
pub struct ConversionCache {
    root: PathBuf,
    limit_bytes: u64,
}

impl ConversionCache {
    /// A cache rooted at `root`, evicted down to `limit_bytes`.
    pub fn new(root: impl Into<PathBuf>, limit_bytes: u64) -> ConversionCache {
        ConversionCache {
            root: root.into(),
            limit_bytes,
        }
    }

    /// The cache directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The configured byte limit.
    pub fn limit_bytes(&self) -> u64 {
        self.limit_bytes
    }

    /// Where a key's output lives.
    pub fn path_for(&self, key: &CacheKey) -> PathBuf {
        self.root.join(key.file_name())
    }

    /// Whether the output for a key is already cached.
    pub fn contains(&self, key: &CacheKey) -> bool {
        self.path_for(key).is_file()
    }

    /// Total bytes currently cached.
    pub fn total_bytes(&self) -> Result<u64> {
        Ok(self.entries()?.iter().map(|entry| entry.size).sum())
    }

    /// Delete every cached file, returning the number of bytes freed.
    pub fn clear(&self) -> Result<u64> {
        let mut freed = 0;
        for entry in self.entries()? {
            if std::fs::remove_file(&entry.path).is_ok() {
                freed += entry.size;
            }
        }
        Ok(freed)
    }

    /// Remove the oldest files until the cache is within its limit, returning
    /// the number of bytes freed.
    pub fn evict_to_limit(&self) -> Result<u64> {
        let mut entries = self.entries()?;
        let mut total: u64 = entries.iter().map(|entry| entry.size).sum();
        if total <= self.limit_bytes {
            return Ok(0);
        }

        // Oldest first.
        entries.sort_by_key(|entry| entry.modified);
        let mut freed = 0;
        for entry in entries {
            if total <= self.limit_bytes {
                break;
            }
            if std::fs::remove_file(&entry.path).is_ok() {
                total -= entry.size;
                freed += entry.size;
            }
        }
        Ok(freed)
    }

    fn entries(&self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        let listing = match std::fs::read_dir(&self.root) {
            Ok(listing) => listing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(error) => return Err(error.into()),
        };
        for item in listing.flatten() {
            let path = item.path();
            let Ok(metadata) = item.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            out.push(Entry {
                path,
                size: metadata.len(),
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }
        Ok(out)
    }
}

struct Entry {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

/// Keep only characters that are safe in a file name.
fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::MediaPlan;
    use std::time::Duration;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("reel-media-cache-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, bytes: usize, age: Duration) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, vec![0u8; bytes]).unwrap();
        let when = SystemTime::now() - age;
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(when)
            .unwrap();
        path
    }

    #[test]
    fn key_names_are_stable_and_sanitized() {
        let key = CacheKey::new("abc/def", 3, 1000, "h264-1080p");
        assert_eq!(key.file_name(), "abc_def-3-1000-h264-1080p.mp4");

        let remux = CacheKey::for_plan("abc", 0, 1, &MediaPlan::Remux, None);
        assert_eq!(remux.file_name(), "abc-0-1-remux.mp4");
        let transcode =
            CacheKey::for_plan("abc", 0, 1, &MediaPlan::Transcode { max_height: 720 }, None);
        assert_eq!(transcode.file_name(), "abc-0-1-h264-720p.mp4");
        // A chosen audio stream gets its own cache entry.
        let chosen = CacheKey::for_plan("abc", 0, 1, &MediaPlan::Remux, Some(3));
        assert_eq!(chosen.file_name(), "abc-0-1-remux-a3.mp4");
    }

    #[test]
    fn contains_reflects_the_filesystem() {
        let dir = scratch("contains");
        let cache = ConversionCache::new(&dir, 1024);
        let key = CacheKey::for_plan("hash", 0, 10, &MediaPlan::Remux, None);
        assert!(!cache.contains(&key));
        std::fs::write(cache.path_for(&key), b"x").unwrap();
        assert!(cache.contains(&key));
    }

    #[test]
    fn a_missing_directory_is_an_empty_cache() {
        let cache = ConversionCache::new("/no/such/reel-cache-dir", 1024);
        assert_eq!(cache.total_bytes().unwrap(), 0);
        assert_eq!(cache.clear().unwrap(), 0);
    }

    #[test]
    fn evicts_the_oldest_files_first() {
        let dir = scratch("evict");
        write(&dir, "old.mp4", 600, Duration::from_secs(3600));
        write(&dir, "new.mp4", 600, Duration::from_secs(1));

        let cache = ConversionCache::new(&dir, 700);
        assert_eq!(cache.total_bytes().unwrap(), 1200);
        assert_eq!(cache.evict_to_limit().unwrap(), 600);
        assert_eq!(cache.total_bytes().unwrap(), 600);
        assert!(!dir.join("old.mp4").exists());
        assert!(dir.join("new.mp4").exists());
    }

    #[test]
    fn clear_removes_everything() {
        let dir = scratch("clear");
        write(&dir, "a.mp4", 100, Duration::from_secs(1));
        write(&dir, "b.mp4", 200, Duration::from_secs(1));
        let cache = ConversionCache::new(&dir, 10_000);
        assert_eq!(cache.clear().unwrap(), 300);
        assert_eq!(cache.total_bytes().unwrap(), 0);
    }
}
