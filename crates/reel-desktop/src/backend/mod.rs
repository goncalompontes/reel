//! The seam between the UI and everything below it.
//!
//! The UI only ever talks to [`Backend`], which keeps three useful properties:
//!
//! * the whole interface can be driven in tests by [`FakeBackend`], with no
//!   engine, no network and no HTTP server;
//! * the engine stays off the UI thread for anything slow, because the async
//!   methods are spawned onto a runtime and report back through events;
//! * metadata enrichment happens in the background, so a library of two hundred
//!   titles does not stall the first frame.
//!
//! The real implementation mounts the engine, the streaming HTTP server and the
//! metadata provider in one process. The player is handed URLs on `127.0.0.1`,
//! so playback uses the same range-request path the CLI and the integration
//! tests exercise.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reel_catalog::{
    CatalogCache, CatalogEntry, CatalogSettings, ConversionMode, FileInput, LookupQuery, MediaRef,
    MediaStore, Metadata, MetadataProvider, Release, SearchAggregator, SearchResults, TmdbClient,
    WatchHistory, WatchProgress, Work, WorkMember, analyse, default_data_dir, episode_key_parts,
    provider_from_key,
};
use reel_core::model::{FileView, StatsView, StreamTarget, TorrentView};
use reel_core::title::clean_title;
use reel_core::{
    AddOptions, AddSource, Engine, EngineConfig, LibraryEntry, LibraryStore, StoredFile,
};

use crate::cast::CastService;

mod engine;
mod fake;

pub use engine::{CatalogOptions, EngineBackend};
pub use fake::FakeBackend;
// The engine's helpers are used by the tests below.
#[allow(unused_imports)]
pub(crate) use engine::*;
#[allow(unused_imports)]
pub(crate) use fake::*;

/// How many metadata lookups may be in flight at once. Providers rate-limit, and
/// a library can be large.
const MAX_CONCURRENT_LOOKUPS: usize = 2;

/// How many distinct works from one search to look up. A search may return many
/// releases for one work; they share one lookup, and a broad query must not
/// create an unbounded stream of TMDB requests.
const MAX_SEARCH_LOOKUPS: usize = 16;

/// Search metadata is nonessential UI enrichment; serialize requests so a large
/// result set does not burst at the provider.
const MAX_SEARCH_CONCURRENCY: usize = 1;
/// Minimum spacing between starting search metadata lookups.
const SEARCH_LOOKUP_DELAY: Duration = Duration::from_millis(400);

/// How many extra pages a search streams in after the first batch. A source
/// that paginates keeps filling the list until it runs out; this bounds a
/// source that never stops.
const MAX_SEARCH_ROUNDS: usize = 20;

/// Minimum gap between writing watch positions to disk.
const HISTORY_WRITE_INTERVAL: Duration = Duration::from_secs(3);

/// How long the settings page's cache figures are reused before the cache
/// directory is walked again. The walk is cheap but not free, and the page is
/// redrawn many times a second; the numbers are trivia, so a little staleness
/// is invisible and a disk walk per frame is not.
const CACHE_STATS_TTL: Duration = Duration::from_secs(2);

/// The cache figures the settings page shows, and when they were measured.
#[derive(Debug, Clone, Copy)]
struct CacheStats {
    measured_at: Instant,
    metadata_files: usize,
    bytes: u64,
}

/// How video will be shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerCapability {
    /// libmpv renders frames into the app window.
    Embedded,
    /// A separate player process; reel cannot draw on top of it.
    External { program: String },
    /// Neither worked.
    Unavailable { reason: String },
}

#[derive(Debug, Clone)]
pub struct BackendCapabilities {
    pub download_dir: String,
    pub client_name: String,
    pub player: PlayerCapability,
    pub mpv_api: Option<(u64, u64)>,
    /// Why embedded playback is unavailable, when it is.
    pub player_note: Option<String>,
}

/// How metadata enrichment is doing, for the settings page.
#[derive(Debug, Clone, Default)]
pub struct CatalogStatus {
    pub provider: String,
    pub configured: bool,
    /// Why there is no provider, when there is not.
    pub note: Option<String>,
    pub cached_metadata: usize,
    pub cache_bytes: u64,
    pub cache_dir: String,
    /// How many library entries have metadata.
    pub enriched: usize,
    /// How many library entries are still being looked up.
    pub pending: usize,
    /// Where the key in use came from, in words.
    pub key_source: String,
    /// False when the key comes from the environment and cannot be changed here.
    pub can_set_key: bool,
}

/// One configured place to look for torrents.
#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub name: String,
    pub configured: bool,
}

/// A torrent together with what the catalogue knows about it.
#[derive(Debug, Clone)]
pub struct LibraryItem {
    /// Engine state: files, progress, peers.
    pub torrent: TorrentView,
    /// Catalogue state: metadata, artwork, watch position.
    pub entry: CatalogEntry,
    /// The user asked to keep this title on disk. `false` means it is only ever
    /// streamed.
    pub downloading: bool,
    /// Which files the user asked to keep, so a per-episode/season/film row can
    /// show its own Download/Stop toggle.
    pub kept_files: Vec<usize>,
}

impl LibraryItem {
    /// Heading for the UI, preferring real metadata.
    pub fn heading(&self) -> String {
        self.entry.heading()
    }

    pub fn info_hash(&self) -> &str {
        &self.entry.info_hash
    }

    /// Position to resume from, if any.
    pub fn resume_position(&self) -> Option<f64> {
        self.entry.resume_position()
    }
}

/// Something the backend wants the UI to know about.
#[derive(Debug, Clone)]
pub enum BackendEvent {
    Info(String),
    Error(String),
    Added {
        id: usize,
        title: String,
    },
    /// Metadata arrived for a torrent, so the UI should refresh.
    Metadata {
        info_hash: String,
    },
    /// Metadata arrived for a search result, so the list should refresh.
    SearchMetadata,
    /// Wake the search metadata dispatcher after a miss or a pacing delay.
    SearchLookupReady,
    /// A title that had to be brought back into the session is now live and can
    /// be streamed. Payback may have been requested before that.
    Ready {
        info_hash: String,
    },
    /// A search result was added to play: its files have resolved, so the UI
    /// should open the player on this file.
    PlayReady {
        torrent_id: usize,
        file_id: usize,
    },
    /// Progress for an add/play initiated from a search result.
    SearchActionProgress {
        key: String,
        phase: SearchActionPhase,
    },
    /// A search-result add/play has either completed or failed.
    SearchActionFinished {
        key: String,
    },
    /// Watch positions changed.
    WatchUpdated,
    /// The result of checking an API key.
    ApiKeyChecked {
        ok: bool,
        message: String,
    },
    /// Results for a search. Carries the query so a stale answer for an older
    /// query can be recognised and dropped.
    SearchResults {
        query: String,
        results: SearchResults,
    },
    /// More results for a search already shown, streamed in as later pages
    /// arrive. Carries the query for the same stale-answer reason.
    SearchHitsAdded {
        query: String,
        hits: Vec<reel_catalog::SearchHit>,
    },
    /// No more results are coming for a search.
    SearchStreamEnded {
        query: String,
    },
    /// A search that could not run at all.
    SearchFailed {
        query: String,
        message: String,
    },
}

/// User-visible stages while adding or playing a search result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchActionPhase {
    ResolvingMagnet,
    Adding,
    PreparingPlayback,
}

impl SearchActionPhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::ResolvingMagnet => "Fetching magnet…",
            Self::Adding => "Adding…",
            Self::PreparingPlayback => "Starting playback…",
        }
    }
}

/// One media's download request: the logical media key plus the file(s) that
/// currently back it. The backend resolves this to a stable
/// [`reel_catalog::MediaRef`] and persists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDownload {
    /// e.g. `meta:tmdb:603#movie` or `meta:tmdb:1396#s01e02`.
    pub key: String,
    /// The live torrent holding the file right now.
    pub torrent_id: usize,
    /// File ids within that torrent.
    pub files: Vec<usize>,
}

/// The casting-relevant settings, snapshotted so a save can tell what changed.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CastConfig {
    enabled: bool,
    address: Option<String>,
    ffmpeg: Option<String>,
    mode: ConversionMode,
    max_height: u32,
}

impl CastConfig {
    pub(crate) fn from_settings(settings: &CatalogSettings) -> CastConfig {
        CastConfig {
            enabled: settings.allow_lan_streaming,
            address: settings.cast_lan_address.clone(),
            ffmpeg: settings.ffmpeg_path.clone(),
            mode: settings.conversion_mode,
            max_height: settings.conversion_max_height,
        }
    }

    /// Whether moving to `other` needs the media server to be rebuilt (rather
    /// than just updating the conversion policy in place).
    fn needs_rebind(&self, other: &CastConfig) -> bool {
        self.enabled != other.enabled || self.address != other.address || self.ffmpeg != other.ffmpeg
    }
}

pub trait Backend {
    fn capabilities(&self) -> &BackendCapabilities;
    fn base_url(&self) -> &str;

    /// The whole library, torrents joined to catalogue metadata.
    fn library(&self) -> Vec<LibraryItem>;
    fn item(&self, id: usize) -> Option<LibraryItem>;

    fn add(&self, source: &str, media_only: bool);
    fn set_paused(&self, id: usize, paused: bool);
    fn remove(&self, id: usize, delete_files: bool);
    /// Choose exactly which files of a torrent are fetched.
    ///
    /// This is what makes "watch episode 3 of a season pack" fetch only episode
    /// 3 rather than the whole season. Files left out keep whatever they had and
    /// stop being requested.
    fn set_only_files(&self, id: usize, files: &[usize]);

    /// Fetch exactly these files *and* make sure the torrent is running.
    ///
    /// This is what pressing Play uses: the copy is temporary, so it is fetched
    /// with the streaming storage and thrown away when playback stops. A
    /// multi-file torrent is added paused, so narrowing the selection alone
    /// would leave the stream request waiting forever; the torrent is unpaused
    /// too.
    fn start_files(&self, id: usize, files: &[usize]);

    /// Keep this media, backed by these files. The record is keyed by the
    /// media (`work.key#s01e02`, `work.key#movie`), not the torrent, and
    /// replaces any previous copy — so switching the chosen release moves the
    /// download rather than duplicating it.
    fn download_media(&self, items: Vec<MediaDownload>);

    /// Stop keeping these files. When a media's last file goes, the media is no
    /// longer a download; its source deletes the files and goes back to
    /// streaming.
    fn stop_media_download(&self, items: Vec<MediaDownload>);

    /// Remember which source the user chose for a media, so the copy chooser
    /// survives a restart.
    fn choose_source(&self, key: String, torrent_id: usize, file_id: usize);

    /// The user's chosen source for a media, as `(torrent id, file id)`.
    fn chosen_source(&self, key: &str) -> Option<(usize, usize)>;

    /// Playback has stopped. A title that is not being downloaded is released
    /// so its temporary storage goes away; a download is left alone.
    fn stop_streaming(&self, id: usize);

    /// Re-add a stream from scratch and start these files. Used when a cached
    /// resume stalls: a fresh add is the path that is known to play.
    fn restart_stream(&self, id: usize, files: &[usize]);

    /// Whether the title is currently in the session, so it can stream now.
    /// A title that is not live has to be brought back first, which reports
    /// [`BackendEvent::Ready`] when it is done.
    fn is_live(&self, id: usize) -> bool;

    /// Report the player's position so the backend can keep streaming from
    /// running away: once too much is buffered ahead of playback it pauses the
    /// fetch, and resumes when playback catches up.
    fn note_playback(&self, id: usize, file_id: usize, position: f64, duration: Option<f64>);

    /// Remember how far through one file of a torrent playback got.
    ///
    /// Per file, not per torrent: a series is one torrent with many episodes,
    /// and one position for all of them would resume episode one at episode
    /// two's timestamp.
    fn record_watch(
        &self,
        info_hash: &str,
        file_id: usize,
        file_name: Option<String>,
        title: Option<String>,
        position: f64,
        duration: Option<f64>,
    );
    fn mark_finished(&self, info_hash: &str, file_id: usize, title: Option<String>);
    fn forget_watch(&self, info_hash: &str, file_id: usize);

    /// Start a search. Results arrive as [`BackendEvent::SearchResults`].
    ///
    /// The whole query — text plus any year/season/episode filters — is passed,
    /// so backends that understand a filter can use it.
    fn search(&self, query: &reel_catalog::SearchQuery);
    /// Add a search result. A hit with no magnet yet (an index site that hides
    /// it behind the torrent's page) is resolved first, then added.
    fn add_search_hit(&self, hit: reel_catalog::SearchHit, media_only: bool);

    /// Add a search result and start streaming it: resolve the magnet if
    /// needed, add the torrent, and open the player on its best file once the
    /// files resolve. Emits [`BackendEvent::PlayReady`] when it is time.
    fn play_search_hit(&self, _hit: reel_catalog::SearchHit, _media_only: bool) {}

    /// Start metadata lookups for search results, so the list can show a rating
    /// and a year next to each. Best-effort and bounded; the results are read
    /// back with [`Backend::search_metadata`].
    fn enrich_search(&self, _hits: &[reel_catalog::SearchHit]) {}

    /// Metadata matched to a search result, once [`Backend::enrich_search`] has
    /// had a chance to run.
    fn search_metadata(&self, _hit: &reel_catalog::SearchHit) -> Option<Metadata> {
        None
    }

    /// How many metadata lookups are still running. The UI keeps repainting
    /// while any are, so results appear as they arrive without a mouse move.
    fn pending_lookups(&self) -> usize {
        0
    }

    /// The configured search sources, for the settings page.
    fn search_sources(&self) -> Vec<SourceInfo>;

    fn catalog_status(&self) -> CatalogStatus;
    /// The persisted settings, the canonical configuration.
    fn settings(&self) -> CatalogSettings;
    /// Persist settings and apply what can be applied immediately. A changed
    /// download directory takes effect on the next start.
    fn save_settings(&self, settings: CatalogSettings);
    /// Store a TMDB key and use it immediately. `None` clears it.
    fn set_api_key(&self, key: Option<String>);
    /// Check a key against the provider and report back as an event.
    fn test_api_key(&self, key: String);
    /// Clear cached metadata and artwork, then enrich again.
    fn clear_catalog_cache(&self);
    /// Forget what we know and look everything up again.
    fn refresh_metadata(&self);

    fn take_events(&self) -> Vec<BackendEvent>;

    /// Why casting is unavailable, or `None` when the LAN media server is
    /// running and renderers can be cast to.
    fn cast_unavailable(&self) -> Option<String> {
        Some("casting is not available".to_string())
    }

    /// Renderers found on the local network.
    fn cast_devices(&self) -> Vec<crate::cast::DeviceInfo> {
        Vec::new()
    }

    /// Whether a renderer search is in progress.
    fn cast_discovering(&self) -> bool {
        false
    }

    /// The active cast, if any.
    fn cast_status(&self) -> Option<crate::cast::CastStatus> {
        None
    }

    /// Start (or repeat) a renderer search.
    fn refresh_cast_devices(&self) {}

    /// Find a renderer at a specific address (IP, `ip:port`, or a URL) and add
    /// it. The escape hatch for networks that block SSDP multicast.
    fn add_cast_device(&self, _address: String) {}

    /// Cast a torrent file to the renderer with the given UDN.
    fn start_cast(&self, _udn: String, _torrent_id: usize, _file_id: usize, _title: String) {}

    /// Control the active cast.
    fn cast_action(&self, _action: crate::cast::CastAction) {}

    /// Delete the files converted for casting.
    fn clear_cast_cache(&self) {}

    /// Dismiss the cast view and session without going through the renderer.
    fn dismiss_cast(&self) {}
}

#[cfg(test)]
mod tests {
    use crate::testing::sample_library;

    use super::*;

    #[test]
    fn cast_config_rebinds_only_for_listener_changes() {
        let base = CatalogSettings::default();
        let original = CastConfig::from_settings(&base);

        // Conversion policy changes update in place.
        let mut policy = base.clone();
        policy.conversion_max_height = 720;
        policy.conversion_mode = ConversionMode::Remux;
        assert!(!original.needs_rebind(&CastConfig::from_settings(&policy)));

        // The LAN toggle and the advertised address need a rebuild.
        let mut enabled = base.clone();
        enabled.allow_lan_streaming = true;
        assert!(original.needs_rebind(&CastConfig::from_settings(&enabled)));

        let mut addressed = base.clone();
        addressed.cast_lan_address = Some("192.168.1.5".to_string());
        assert!(original.needs_rebind(&CastConfig::from_settings(&addressed)));
    }

    fn entry_without_name() -> LibraryEntry {
        let item = sample_library().into_iter().next().expect("a sample");
        LibraryEntry {
            id: 0,
            info_hash: item.torrent.info_hash.clone(),
            source: String::new(),
            name: None,
            files: Vec::new(),
            primary_file_id: None,
            selected_files: Vec::new(),
            downloading: false,
            paused: false,
            added_at: 0,
        }
    }

    #[test]
    fn a_stream_path_names_the_torrent_and_the_file() {
        let path = stream_path(0, 3, "Show S01E04.mkv");
        assert_eq!(path, "/stream/0/3/Show%20S01E04.mkv");
        // Always `stream / <torrent> / <file> / <name>`: a missing file id
        // makes the router treat the file name as the id and answer 400.
        assert_eq!(path.trim_start_matches('/').split('/').count(), 4);
    }

    #[test]
    fn an_unresolved_magnet_is_not_called_by_its_hash() {
        let entry = entry_without_name();
        let (title, year) = display_title_for_entry(&entry);
        assert_eq!(title, "Resolving magnet\u{2026}");
        assert_eq!(year, None);
        assert!(
            !title.contains(&entry.info_hash),
            "the info hash should never be shown as a title"
        );
    }

    #[test]
    fn an_empty_name_is_treated_as_no_name() {
        let mut entry = entry_without_name();
        entry.name = Some("   ".to_string());
        assert_eq!(
            display_title_for_entry(&entry).0,
            "Resolving magnet\u{2026}"
        );
    }

    #[test]
    fn a_real_name_is_cleaned_as_usual() {
        let mut entry = entry_without_name();
        entry.name = Some("The.Matrix.1999.1080p.BluRay.x264-GROUP".to_string());
        let (title, year) = display_title_for_entry(&entry);
        assert_eq!(title, "The Matrix");
        assert_eq!(year, Some(1999));
    }

    #[test]
    fn search_metadata_deduplicates_release_variants_of_a_series() {
        let mut first =
            reel_catalog::SearchHit::new("The.Bear.S03E01.1080p.WEB-DL.H264-GROUP", "ext.to");
        first.year = Some(2022);
        let mut second =
            reel_catalog::SearchHit::new("The.Bear.S03E02.720p.WEBRip.x265-OTHER", "ext.to");
        second.year = Some(2022);

        assert_eq!(search_metadata_key(&first), search_metadata_key(&second));

        let query = search_lookup_query(&first);
        assert_eq!(query.title, "The Bear");
        assert_eq!(query.year, Some(2022));
        assert_eq!(query.kind, reel_catalog::MediaKind::Series);
        assert!(query.seasons.is_empty());
        assert!(query.episodes.is_empty());
        assert!(query.air_dates.is_empty());
    }

    #[test]
    fn search_metadata_keeps_distinct_work_identities_separate() {
        let bear = reel_catalog::SearchHit::new("The.Bear.S03E01.1080p", "ext.to");
        let different_year = reel_catalog::SearchHit {
            year: Some(2022),
            ..bear.clone()
        };
        let movie = reel_catalog::SearchHit::new("The.Bear.2022.1080p.BluRay", "ext.to");

        assert_ne!(
            search_metadata_key(&bear),
            search_metadata_key(&different_year)
        );
        assert_ne!(search_metadata_key(&bear), search_metadata_key(&movie));
    }

    #[test]
    fn search_metadata_lookup_budget_is_unique_and_bounded() {
        let mut budget = HashSet::new();
        for index in 0..MAX_SEARCH_LOOKUPS {
            assert!(claim_search_lookup_budget(
                &mut budget,
                &format!("work-{index}")
            ));
        }
        assert_eq!(budget.len(), MAX_SEARCH_LOOKUPS);
        assert!(!claim_search_lookup_budget(&mut budget, "one-more-work"));
        assert!(!claim_search_lookup_budget(&mut budget, "work-0"));
    }

    #[test]
    fn search_metadata_lookups_are_spaced_by_the_configured_delay() {
        let start = Instant::now();
        assert_eq!(
            search_lookup_delay_remaining(start, start),
            Some(SEARCH_LOOKUP_DELAY)
        );
        assert_eq!(
            search_lookup_delay_remaining(start, start + Duration::from_millis(250)),
            Some(Duration::from_millis(150))
        );
        assert_eq!(
            search_lookup_delay_remaining(start, start + SEARCH_LOOKUP_DELAY),
            None
        );
    }
}
