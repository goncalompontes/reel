//! On-disk cache for metadata and artwork.
//!
//! Two things are cached: the metadata JSON (so a restart does not re-hit the
//! API for every title) and the downloaded images (so posters render without a
//! network round trip).
//!
//! Provider ids are used to build file names, so they are sanitised first. A
//! provider that returned `../../etc/passwd` as an id must not be able to write
//! outside the cache directory.

use std::path::{Path, PathBuf};

use crate::model::{ArtworkKind, ArtworkSize, Metadata};

pub struct CatalogCache {
    root: PathBuf,
}

impl CatalogCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn metadata_dir(&self) -> PathBuf {
        self.root.join("metadata")
    }

    fn artwork_dir(&self, source: &str) -> PathBuf {
        self.root.join("artwork").join(sanitize(source))
    }

    pub fn metadata_path(&self, source: &str, id: &str) -> PathBuf {
        self.metadata_dir()
            .join(format!("{}-{}.json", sanitize(source), sanitize(id)))
    }

    pub fn artwork_path(
        &self,
        source: &str,
        id: &str,
        kind: ArtworkKind,
        size: ArtworkSize,
    ) -> PathBuf {
        self.artwork_dir(source)
            .join(format!("{}-{}.jpg", sanitize(id), slot(kind, size)))
    }

    /// Read a cached metadata blob, if one was written before.
    pub fn read_metadata(&self, source: &str, id: &str) -> Option<Metadata> {
        let path = self.metadata_path(source, id);
        let data = std::fs::read(&path).ok()?;
        match serde_json::from_slice::<Metadata>(&data) {
            Ok(metadata) => Some(metadata),
            Err(e) => {
                // A corrupt entry should heal itself, not wedge the app.
                tracing::warn!(path = %path.display(), error = %e, "dropping corrupt cached metadata");
                let _ = std::fs::remove_file(&path);
                None
            }
        }
    }

    pub fn write_metadata(&self, metadata: &Metadata) -> std::io::Result<()> {
        let path = self.metadata_path(&metadata.source, &metadata.source_id);
        write_atomically(&path, &serde_json::to_vec(metadata)?)
    }

    /// Write metadata under an explicit key rather than the metadata's own id.
    ///
    /// Used for derived indexes, such as remembering which provider id a
    /// title + year resolved to, so a restart does not re-search the API.
    pub fn write_metadata_at(
        &self,
        source: &str,
        id: &str,
        metadata: &Metadata,
    ) -> std::io::Result<()> {
        let path = self.metadata_path(source, id);
        write_atomically(&path, &serde_json::to_vec(metadata)?)
    }

    /// Where the cached artwork for this slot lives, if it exists.
    pub fn cached_artwork(
        &self,
        source: &str,
        id: &str,
        kind: ArtworkKind,
        size: ArtworkSize,
    ) -> Option<PathBuf> {
        let path = self.artwork_path(source, id, kind, size);
        path.is_file().then_some(path)
    }

    /// Store downloaded artwork bytes and return the file they landed in.
    pub fn store_artwork(
        &self,
        source: &str,
        id: &str,
        kind: ArtworkKind,
        size: ArtworkSize,
        bytes: &[u8],
    ) -> std::io::Result<PathBuf> {
        let path = self.artwork_path(source, id, kind, size);
        write_atomically(&path, bytes)?;
        Ok(path)
    }

    /// Total size of everything cached, for the settings page.
    pub fn total_bytes(&self) -> u64 {
        fn walk(dir: &Path) -> u64 {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return 0;
            };
            entries
                .filter_map(Result::ok)
                .map(|entry| match entry.file_type() {
                    Ok(kind) if kind.is_dir() => walk(&entry.path()),
                    Ok(_) => entry.metadata().map(|m| m.len()).unwrap_or(0),
                    Err(_) => 0,
                })
                .sum()
        }
        walk(&self.root)
    }

    /// Number of cached metadata blobs, for the settings page.
    pub fn metadata_count(&self) -> usize {
        std::fs::read_dir(self.metadata_dir())
            .map(|entries| entries.filter_map(Result::ok).count())
            .unwrap_or(0)
    }

    pub fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

fn slot(kind: ArtworkKind, size: ArtworkSize) -> String {
    let kind = match kind {
        ArtworkKind::Poster => "poster",
        ArtworkKind::Backdrop => "backdrop",
    };
    let size = match size {
        ArtworkSize::Card => "card",
        ArtworkSize::Hero => "hero",
    };
    format!("{kind}-{size}")
}

/// Reduce an arbitrary provider id to a safe single path component.
///
/// Anything that is not alphanumeric, `-` or `_` becomes `_`, which collapses
/// `..`, `/` and NUL into harmless characters.
pub fn sanitize(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    if cleaned.is_empty() || cleaned.chars().all(|c| c == '_') {
        "_".to_string()
    } else {
        cleaned
    }
}

/// Write to a temporary file and rename, so a crash mid-write cannot leave a
/// half-written file that later parses as corrupt.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Artwork;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("reel-cache-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn metadata(id: &str) -> Metadata {
        Metadata {
            source: "tmdb".into(),
            source_id: id.into(),
            title: "The Matrix".into(),
            year: Some(1999),
            artwork: Artwork::default(),
            ..Default::default()
        }
    }

    #[test]
    fn sanitising_cannot_escape_the_cache_directory() {
        // These are the shapes that would otherwise write outside the root.
        for hostile in [
            "../../etc/passwd",
            "..",
            "/absolute/path",
            "a/b/c",
            "nul\0byte",
            "....//..//",
        ] {
            let safe = sanitize(hostile);
            assert!(
                !safe.contains('/') && !safe.contains('\\') && !safe.contains(".."),
                "{hostile:?} produced {safe:?}"
            );
        }

        let cache = CatalogCache::new("/tmp/root");
        let path = cache.metadata_path("../../evil", "../../etc/passwd");
        assert!(
            path.starts_with("/tmp/root/metadata"),
            "escaped the cache root: {}",
            path.display()
        );
        // Exactly one file directly inside the metadata directory.
        assert_eq!(path.parent().unwrap(), std::path::Path::new("/tmp/root/metadata"));
        assert_eq!(path.file_name().unwrap().to_string_lossy().matches('/').count(), 0);
    }

    #[test]
    fn metadata_round_trips() {
        let dir = scratch("roundtrip");
        let cache = CatalogCache::new(&dir);

        assert!(cache.read_metadata("tmdb", "603").is_none());
        cache.write_metadata(&metadata("603")).unwrap();

        let read = cache.read_metadata("tmdb", "603").expect("cached");
        assert_eq!(read.title, "The Matrix");
        assert_eq!(read.year, Some(1999));
        assert_eq!(cache.metadata_count(), 1);

        cache.clear().unwrap();
        assert!(cache.read_metadata("tmdb", "603").is_none());
        // Clearing twice must not error.
        cache.clear().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_metadata_is_dropped_not_returned() {
        let dir = scratch("corrupt");
        let cache = CatalogCache::new(&dir);
        let path = cache.metadata_path("tmdb", "1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ this is not json").unwrap();

        assert!(cache.read_metadata("tmdb", "1").is_none());
        assert!(!path.exists(), "the corrupt file should have been removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn artwork_is_stored_and_found_per_slot() {
        let dir = scratch("artwork");
        let cache = CatalogCache::new(&dir);

        assert!(cache
            .cached_artwork("tmdb", "603", ArtworkKind::Poster, ArtworkSize::Card)
            .is_none());

        let stored = cache
            .store_artwork("tmdb", "603", ArtworkKind::Poster, ArtworkSize::Card, b"jpegbytes")
            .unwrap();
        assert_eq!(std::fs::read(&stored).unwrap(), b"jpegbytes");

        // The same id at another size is a different file.
        assert!(cache
            .cached_artwork("tmdb", "603", ArtworkKind::Poster, ArtworkSize::Hero)
            .is_none());
        assert!(cache
            .cached_artwork("tmdb", "603", ArtworkKind::Backdrop, ArtworkSize::Hero)
            .is_none());
        assert!(cache
            .cached_artwork("tmdb", "603", ArtworkKind::Poster, ArtworkSize::Card)
            .is_some());

        assert!(cache.total_bytes() >= b"jpegbytes".len() as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_temporary_files_are_left_behind() {
        let dir = scratch("atomic");
        let cache = CatalogCache::new(&dir);
        cache.write_metadata(&metadata("1")).unwrap();
        cache
            .store_artwork("tmdb", "1", ArtworkKind::Poster, ArtworkSize::Card, b"x")
            .unwrap();

        let temps: Vec<_> = walk_names(&dir)
            .into_iter()
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(temps.is_empty(), "left temporary files: {temps:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn walk_names(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.filter_map(Result::ok) {
            out.push(entry.file_name().to_string_lossy().into_owned());
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                out.extend(walk_names(&entry.path()));
            }
        }
        out
    }
}
