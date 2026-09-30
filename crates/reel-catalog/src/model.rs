//! Wire-level catalog types.
//!
//! These describe *what a title is*, independent of where the bytes come from.
//! A torrent in the library is matched against this metadata so the interface
//! can show a poster, a synopsis and a rating instead of a release name.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which image slot an artwork reference fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtworkKind {
    Poster,
    Backdrop,
}

/// The sizes a provider can render artwork at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtworkSize {
    /// Grid cards.
    Card,
    /// Detail page and hero banners.
    Hero,
}

/// A reference to one image, with its cached copy when we have it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtworkRef {
    /// Path on the provider's image host, e.g. `/abc123.jpg`.
    pub remote_path: String,
    /// Where the downloaded copy lives, once fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<PathBuf>,
}

impl ArtworkRef {
    pub fn new(remote_path: impl Into<String>) -> Self {
        Self {
            remote_path: remote_path.into(),
            local_path: None,
        }
    }

    /// True when a usable copy is on disk.
    pub fn is_cached(&self) -> bool {
        self.local_path.as_ref().is_some_and(|p| p.is_file())
    }

    /// The `file://` URI egui's image loader can open.
    pub fn local_uri(&self) -> Option<String> {
        let path = self.local_path.as_ref()?;
        if !path.is_file() {
            return None;
        }
        Some(format!("file://{}", path.display()))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artwork {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poster: Option<ArtworkRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backdrop: Option<ArtworkRef>,
}

impl Artwork {
    pub fn is_empty(&self) -> bool {
        self.poster.is_none() && self.backdrop.is_none()
    }

    pub fn get(&self, kind: ArtworkKind) -> Option<&ArtworkRef> {
        match kind {
            ArtworkKind::Poster => self.poster.as_ref(),
            ArtworkKind::Backdrop => self.backdrop.as_ref(),
        }
    }

    pub fn get_mut(&mut self, kind: ArtworkKind) -> Option<&mut ArtworkRef> {
        match kind {
            ArtworkKind::Poster => self.poster.as_mut(),
            ArtworkKind::Backdrop => self.backdrop.as_mut(),
        }
    }
}

/// Everything a catalog entry knows about a title.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    /// Provider that produced this, e.g. `tmdb`.
    pub source: String,
    /// Provider's own id, as a string so any provider fits.
    pub source_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagline: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_minutes: Option<u32>,
    /// Out of ten, as providers report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vote_count: Option<u32>,
    /// Provider's own popularity score, used only for ranking candidates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub popularity: Option<f32>,
    #[serde(default, skip_serializing_if = "Artwork::is_empty")]
    pub artwork: Artwork,
}

impl Metadata {
    /// `The Matrix (1999)`.
    pub fn display_title(&self) -> String {
        match self.year {
            Some(year) => format!("{} ({year})", self.title),
            None => self.title.clone(),
        }
    }

    /// `1h 52m`.
    pub fn runtime_label(&self) -> Option<String> {
        let minutes = self.runtime_minutes?;
        if minutes == 0 {
            return None;
        }
        let (h, m) = (minutes / 60, minutes % 60);
        Some(if h > 0 {
            format!("{h}h {m:02}m")
        } else {
            format!("{m}m")
        })
    }
}

/// A candidate returned by a metadata search, before details are fetched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub source: String,
    pub source_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vote_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub popularity: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poster_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backdrop_path: Option<String>,
}

/// Fraction of a title past which it counts as watched. Credits are long
/// enough that stopping at 92% is the norm.
pub const FINISHED_FRACTION: f64 = 0.92;

/// How far through a title the user got.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchProgress {
    /// Seconds watched.
    pub position: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Unix seconds, for ordering the "continue watching" row.
    pub updated_at: i64,
    /// Which file inside the torrent was being watched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// Torrent this progress belongs to, kept so the row survives a restart
    /// even before the engine has re-listed its torrents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl WatchProgress {
    /// Fraction watched, when the duration is known.
    pub fn fraction(&self) -> Option<f64> {
        let duration = self.duration.filter(|d| *d > 0.0)?;
        Some((self.position / duration).clamp(0.0, 1.0))
    }

    /// Watched far enough in to be worth resuming.
    pub fn is_resumable(&self) -> bool {
        self.position >= 30.0 && !self.is_finished()
    }

    /// Treated as finished past [`FINISHED_FRACTION`], so the credits do not
    /// keep a title in the continue-watching row forever.
    pub fn is_finished(&self) -> bool {
        self.fraction().is_some_and(|f| f > FINISHED_FRACTION)
    }
}

/// A library entry, enriched when metadata could be found.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogEntry {
    /// Session-local torrent id.
    pub torrent_id: usize,
    /// Stable across restarts, and the key for watch history.
    pub info_hash: String,
    /// Title cleaned from the release name, always available.
    pub display_title: String,
    pub year: Option<u16>,
    /// Metadata, when a provider matched this title.
    pub metadata: Option<Metadata>,
    pub watch: Option<WatchProgress>,
}

impl CatalogEntry {
    /// What to show as the heading: metadata wins, the cleaned name is the
    /// fallback.
    pub fn title(&self) -> String {
        match self.metadata.as_ref() {
            Some(metadata) => metadata.title.clone(),
            None => self.display_title.clone(),
        }
    }

    /// Year from metadata if present, else the one scraped from the name.
    pub fn year(&self) -> Option<u16> {
        self.metadata
            .as_ref()
            .and_then(|m| m.year)
            .or(self.year)
    }

    /// The heading with its year.
    pub fn heading(&self) -> String {
        match self.year() {
            Some(year) => format!("{} ({year})", self.title()),
            None => self.title(),
        }
    }

    pub fn poster(&self) -> Option<&ArtworkRef> {
        self.metadata.as_ref()?.artwork.poster.as_ref()
    }

    pub fn backdrop(&self) -> Option<&ArtworkRef> {
        self.metadata.as_ref()?.artwork.backdrop.as_ref()
    }

    /// Whether the user can pick up where they left off.
    pub fn resume_position(&self) -> Option<f64> {
        self.watch.as_ref().filter(|w| w.is_resumable()).map(|w| w.position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(position: f64, duration: Option<f64>) -> WatchProgress {
        WatchProgress {
            position,
            duration,
            updated_at: 0,
            file_name: None,
            title: None,
        }
    }

    #[test]
    fn runtime_is_formatted_for_people() {
        let mut metadata = Metadata {
            runtime_minutes: Some(112),
            ..Default::default()
        };
        assert_eq!(metadata.runtime_label().as_deref(), Some("1h 52m"));
        metadata.runtime_minutes = Some(48);
        assert_eq!(metadata.runtime_label().as_deref(), Some("48m"));
        metadata.runtime_minutes = None;
        assert_eq!(metadata.runtime_label(), None);
    }

    #[test]
    fn resumable_needs_real_progress_but_not_the_credits() {
        assert!(!progress(0.0, Some(1000.0)).is_resumable(), "not started");
        assert!(!progress(20.0, Some(1000.0)).is_resumable(), "too early");
        assert!(progress(300.0, Some(1000.0)).is_resumable());
        assert!(!progress(960.0, Some(1000.0)).is_resumable(), "credits");
        assert!(progress(980.0, Some(1000.0)).is_finished());
        // No duration known: resumable as long as it is past the threshold.
        assert!(progress(120.0, None).is_resumable());
    }

    #[test]
    fn entry_prefers_metadata_over_the_release_name() {
        let entry = CatalogEntry {
            torrent_id: 1,
            info_hash: "abc".into(),
            display_title: "The Matrix".into(),
            year: Some(1999),
            metadata: Some(Metadata {
                title: "The Matrix".into(),
                year: Some(1999),
                ..Default::default()
            }),
            watch: None,
        };
        assert_eq!(entry.heading(), "The Matrix (1999)");
        assert_eq!(entry.resume_position(), None);

        // Without metadata the scraped year still shows.
        let bare = CatalogEntry {
            metadata: None,
            ..entry
        };
        assert_eq!(bare.heading(), "The Matrix (1999)");
    }

    #[test]
    fn local_uri_only_when_the_file_exists() {
        let mut artwork = ArtworkRef::new("/poster.jpg");
        assert_eq!(artwork.local_uri(), None);
        artwork.local_path = Some(PathBuf::from("/definitely/not/here.jpg"));
        assert_eq!(artwork.local_uri(), None);
        assert!(!artwork.is_cached());
    }
}
