//! Wire-level catalog types.
//!
//! These describe *what a title is*, independent of where the bytes come from.
//! A torrent in the library is matched against this metadata so the interface
//! can show a poster, a synopsis and a rating instead of a release name.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::release::{MediaKind, Release, ReleaseAttributes};

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

/// One season of a series, as a metadata provider lists it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SeasonSummary {
    pub number: u32,
    /// When the season started, `YYYY-MM-DD`. The anchor for date lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub air_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_count: Option<u32>,
}

/// One episode of a series, as a metadata provider describes it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EpisodeInfo {
    pub season: u32,
    pub number: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    /// Episode still, used as the thumbnail in an episode list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub still: Option<ArtworkRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_minutes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub air_date: Option<String>,
}

impl EpisodeInfo {
    /// `S01E03 The One With The Title`, or just `S01E03`.
    pub fn label(&self) -> String {
        let code = format!("S{:02}E{:02}", self.season, self.number);
        match self.name.as_deref().filter(|n| !n.is_empty()) {
            Some(name) => format!("{code}  {name}"),
            None => code,
        }
    }

    pub fn still_uri(&self) -> Option<String> {
        self.still.as_ref().and_then(ArtworkRef::local_uri)
    }
}

/// Everything a catalog entry knows about a title.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    /// Film or series. Absent in caches written before this existed, hence the
    /// default rather than a required field.
    #[serde(default)]
    pub kind: MediaKind,
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

    /// Number of seasons a series has, when it is a series.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season_count: Option<u32>,
    /// The provider's season list. Used to work out which season a date belongs
    /// to, for a show numbered by air date.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seasons: Vec<SeasonSummary>,
    /// Episodes of the season that was looked up, when it is a series.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub episodes: Vec<EpisodeInfo>,
}

impl Metadata {
    /// `The Matrix (1999)`.
    pub fn display_title(&self) -> String {
        match self.year {
            Some(year) => format!("{} ({year})", self.title),
            None => self.title.clone(),
        }
    }

    pub fn is_series(&self) -> bool {
        self.kind == MediaKind::Series
    }

    /// Look up one episode of the fetched season.
    pub fn episode(&self, season: u32, number: u32) -> Option<&EpisodeInfo> {
        self.episodes
            .iter()
            .find(|e| e.season == season && e.number == number)
    }

    /// The season a date falls in: the latest one that started on or before it.
    ///
    /// Daily shows are numbered by air date, and a date is the only handle we
    /// have on which season it belongs to. Seasons are ordered by start date, so
    /// the last one that has already begun is the candidate.
    pub fn season_for_date(&self, date: &str) -> Option<u32> {
        self.seasons
            .iter()
            .filter_map(|season| season.air_date.as_deref().map(|air| (season.number, air)))
            .filter(|(_, air)| *air <= date)
            .max_by_key(|(number, _)| *number)
            .map(|(number, _)| number)
            .or_else(|| self.seasons.first().map(|season| season.number))
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
    /// Most recent watch position for the torrent as a whole, which is what
    /// "continue watching" needs.
    pub watch: Option<WatchProgress>,
    /// What the torrent's own names say it is: film or series, which season,
    /// which episodes, and the release's technical attributes.
    pub release: Release,
    /// Watch positions per file, so an episode list can show which ones were
    /// watched and offer to resume the right one.
    pub watch_by_file: BTreeMap<usize, WatchProgress>,
}

impl CatalogEntry {
    /// Film, series, or not enough to say.
    pub fn kind(&self) -> MediaKind {
        match self.metadata.as_ref().map(|m| m.kind) {
            Some(MediaKind::Unknown) | None => self.release.kind,
            Some(kind) => kind,
        }
    }

    pub fn is_series(&self) -> bool {
        self.kind() == MediaKind::Series
    }

    /// Watch position for one file, falling back to the torrent's own record
    /// so a position saved before per-file tracking is not lost.
    pub fn watch_for_file(&self, file_id: usize) -> Option<&WatchProgress> {
        self.watch_by_file.get(&file_id).or(self.watch.as_ref())
    }

    /// Technical attributes to show as extra information.
    pub fn attributes(&self) -> &ReleaseAttributes {
        &self.release.attributes
    }

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
        
            release: Release::default(),
            watch_by_file: Default::default(),
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
    fn episodes_are_addressable_by_number() {
        let metadata = Metadata {
            kind: MediaKind::Series,
            title: "Some Show".into(),
            episodes: vec![
                EpisodeInfo {
                    season: 1,
                    number: 1,
                    name: Some("Pilot".into()),
                    ..Default::default()
                },
                EpisodeInfo {
                    season: 1,
                    number: 3,
                    name: None,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        assert!(metadata.is_series());
        assert_eq!(metadata.episode(1, 1).unwrap().label(), "S01E01  Pilot");
        // An un-named episode still gets a usable label.
        assert_eq!(metadata.episode(1, 3).unwrap().label(), "S01E03");
        assert_eq!(metadata.episode(1, 2).map(|e| e.number), None);
        assert_eq!(metadata.episode(2, 1).map(|e| e.number), None);
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
