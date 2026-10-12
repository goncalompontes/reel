//! The application: navigation, the catalog rows, the detail page, the add
//! form, settings, and the player screen.
//!
//! Rendering is pure egui — immediate mode, GPU-rendered, no webview. All state
//! comes from [`Backend`], so the same UI runs against a real engine or an
//! in-memory fake in tests.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use egui::{Color32, CornerRadius, Rect, Sense, Vec2};
use reel_catalog::{
    ArtworkKind, ArtworkRef, CatalogEntry, CatalogSettings, Metadata, Release, RowKind, SearchHit,
    Work, WorkEpisode, WorkMember, build_work_rows, build_works, episode_key, movie_key,
    separate_works,
};
use reel_core::fmt;

use crate::backend::{
    Backend, BackendEvent, LibraryItem, MediaDownload, PlayerCapability, SearchActionPhase,
    SourceInfo,
};
use crate::components::{
    caption, chip, empty_state, ghost_button, neutral_chip, page_title, row, section_header, stat,
};
use crate::icons;
use crate::player::{PlaybackInfo, PlaybackStats, PlayerController};
use crate::theme;
mod artwork;
mod chrome;
mod detail;
mod library;
mod player;
mod search;
mod settings;

pub(crate) use detail::*;
pub(crate) use search::*;
pub(crate) use settings::*;

/// A location in the content area.
///
/// The first entry of the navigation trail is always a *root* — a destination
/// on the rail. Anything else is a view pushed on top of it: `Work` today, with
/// collections, seasons and episodes to follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Home,
    Discover,
    Activity,
    Settings,
    /// A title in the library, by its lead torrent id.
    Work(usize),
    /// A title in the current search results, by work key.
    Result(String),
}

impl Screen {
    /// A rail destination, as opposed to a view pushed on top of one.
    fn is_root(&self) -> bool {
        !matches!(self, Screen::Work(_) | Screen::Result(_))
    }

    /// The fixed name of a rail destination, for breadcrumbs and the rail.
    fn root_label(&self) -> &'static str {
        match self {
            Screen::Home => "Home",
            Screen::Discover => "Discover",
            Screen::Activity => "Activity",
            Screen::Settings => "Settings",
            Screen::Work(_) | Screen::Result(_) => "",
        }
    }
}

/// The navigation trail: a root with views stacked on it.
///
/// The content area draws the last entry; breadcrumbs and Back walk the trail,
/// so a title opened from Discover returns to Discover rather than to Home.
struct Nav {
    trail: Vec<Screen>,
    /// Screens popped by Back, so Forward can restore them. Cleared whenever a
    /// new view is opened or the root changes.
    forward: Vec<Screen>,
}

impl Nav {
    fn new(root: Screen) -> Self {
        debug_assert!(root.is_root(), "a trail must start at a rail destination");
        Self {
            trail: vec![root],
            forward: Vec::new(),
        }
    }

    /// The screen currently shown.
    fn screen(&self) -> &Screen {
        self.trail.last().expect("the trail is never empty")
    }

    /// The rail destination the trail is rooted at.
    fn root(&self) -> &Screen {
        &self.trail[0]
    }

    fn trail(&self) -> &[Screen] {
        &self.trail
    }

    /// Go to a rail destination, replacing the trail.
    fn go(&mut self, root: Screen) {
        debug_assert!(root.is_root());
        self.trail.clear();
        self.trail.push(root);
        self.forward.clear();
    }

    /// Open a view on top of the current screen.
    fn open(&mut self, view: Screen) {
        if self.screen() != &view {
            self.trail.push(view);
            self.forward.clear();
        }
    }

    fn can_back(&self) -> bool {
        self.trail.len() > 1
    }

    fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    fn back(&mut self) -> bool {
        if self.can_back() {
            let view = self.trail.pop().expect("can_back implies a view");
            self.forward.push(view);
            true
        } else {
            false
        }
    }

    fn forward(&mut self) -> bool {
        if let Some(view) = self.forward.pop() {
            self.trail.push(view);
            true
        } else {
            false
        }
    }

    /// Drop views until the trail is `depth` long, for a breadcrumb click.
    fn truncate(&mut self, depth: usize) {
        self.trail.truncate(depth.max(1));
        self.forward.clear();
    }
}

/// A catalog row, resolved to torrent ids so rendering never rebuilds it.
#[derive(Debug, Clone)]
pub struct RowLayout {
    pub kind: RowKind,
    pub title: String,
    pub ids: Vec<usize>,
}

/// Everything the search page derives from its hits, cached between frames.
///
/// The derivation is not cheap — every release name is parsed, every hit's
/// metadata is cloned, and the hits are regrouped into works — so it is not
/// something to repeat on every frame of a list that can hold hundreds of rows.
struct SearchView {
    signature: SearchSignature,
    /// Hits after the min-seeders filter and the chosen sort.
    shown: Vec<SearchHit>,
    /// Parsed release info for each `shown` hit, by position.
    releases: Vec<Release>,
    /// One row per film or series, in display order. Discover lists these and
    /// the Result view renders one of them.
    works: Vec<Work>,
}

/// The inputs a [`SearchView`] was built from. When this changes the view is
/// rebuilt; otherwise the previous one is reused exactly as it was.
#[derive(Clone, PartialEq, Eq)]
struct SearchSignature {
    query: Option<String>,
    min_seeders: String,
    sort: SearchSort,
    /// Bumped whenever the hit list changes.
    hits: u64,
    /// Bumped whenever metadata for a hit arrives.
    metadata: u64,
}

/// How the Discover results are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchLayout {
    Grid,
    List,
}

impl SearchLayout {
    fn label(self) -> &'static str {
        match self {
            SearchLayout::Grid => "Grid",
            SearchLayout::List => "List",
        }
    }
}

pub struct App {
    backend: Box<dyn Backend>,
    /// Where the content area is: a rail destination plus any pushed views.
    nav: Nav,
    /// Whether the player overlay is up. It covers the shell and returns to
    /// whatever the trail already shows when it closes.
    player_open: bool,
    /// Whether the global Add modal is up.
    add_open: bool,
    /// Set by the ⌘/Ctrl-K shortcut, consumed by the header's search field.
    focus_search: bool,
    /// Cached library, refreshed periodically while browsing.
    library: Vec<LibraryItem>,
    /// Torrents grouped into films and series. Rebuilt with the library.
    works: Vec<Work>,
    /// Row layout, resolved to work (lead torrent) ids.
    rows: Vec<RowLayout>,
    /// The remove dialog: which work, and which of its torrents are ticked.
    show_remove: Option<usize>,
    remove_selected: HashSet<usize>,
    remove_files: bool,
    last_refresh: Instant,
    toast: Option<Toast>,
    add_source: String,
    add_media_only: bool,
    filter: String,
    /// The search box, and what it last returned.
    search_query: String,
    search_for: Option<String>,
    search_hits: Vec<SearchHit>,
    /// Source names and failures worth showing under the results.
    search_note: Option<String>,
    searching: bool,
    /// Filters that narrow what the backends search for.
    search_year: String,
    search_season: String,
    search_episode: String,
    /// Hide results below this many seeders. Unknown seeder counts are kept.
    search_min_seeders: String,
    /// How the results are ordered for display.
    search_sort: SearchSort,
    /// Whether Discover shows a poster grid or a compact list.
    search_layout: SearchLayout,
    /// Whether the filter fields are expanded under the toolbar.
    filters_open: bool,
    /// True when the last search failed, so the view says so instead of
    /// claiming nothing matched.
    search_failed: bool,
    /// True while a source may still stream in more results.
    search_streaming: bool,
    /// The sources that failed for the current search, kept so the result note
    /// can be recomputed as more results arrive.
    search_failures: Vec<(String, String)>,
    /// Parsed release info per search hit title: film/series, season and
    /// episode, quality and languages. Computed once per title so the list does
    /// not re-parse on every frame.
    search_releases: HashMap<String, Release>,
    /// Per-title choices (season, kind, resolution) in the Result view.
    search_series_views: HashMap<String, SeriesView>,
    /// Search hits currently being added or prepared for playback.
    search_actions: HashMap<String, SearchActionPhase>,
    /// Derived search-page data, rebuilt only when its inputs change.
    search_view: Option<SearchView>,
    /// The search hit anchoring an open Result view. A work's key can change as
    /// metadata lands (a name group becoming a metadata group), so the Result
    /// view is found by a member hit that cannot.
    result_anchor: Option<String>,
    /// Bumped whenever the hit list changes, so the view knows to rebuild.
    search_revision: u64,
    /// Bumped whenever metadata for a hit arrives, for the same reason.
    search_metadata_revision: u64,
    /// Measured heights of the Discover work rows, so offscreen ones can be
    /// skipped.
    search_work_heights: Vec<f32>,
    /// The API key being typed on the settings page. Never filled in from
    /// storage: a credential is written, not displayed.
    api_key_input: String,
    /// Editable copy of the canonical settings while the Settings page is open.
    settings_draft: Option<CatalogSettings>,
    /// Which Settings pane is showing.
    settings_section: SettingsSection,
    player: PlayerController,
    /// Last time the mouse or a key moved, for hiding the chrome in fullscreen.
    player_last_activity: Instant,
    player_controls_hidden: bool,
    /// The bottom controls' screen rect last frame, so the OSD does not hide
    /// while the pointer rests on the buttons.
    player_controls_rect: Option<egui::Rect>,
    /// The player's top bar rect last frame, for the same reason.
    player_topbar_rect: Option<egui::Rect>,
    /// Whether the player's keyboard-shortcut overlay is up.
    player_help: bool,
    /// Torrent id the player was started from, to return to.
    player_origin: Option<usize>,
    /// A play request waiting for its temporary torrent to be brought back.
    pending_play: Option<(usize, usize)>,
    /// Frames and playback position seen last time progress was checked, so a
    /// wedge (frozen picture *and* frozen clock) can be told from a slow stream
    /// (which is still making progress) and from a normal scene that happens to
    /// hold on one frame.
    player_last_frames: u64,
    player_progress_position: f64,
    player_progress_at: Instant,
    /// Whether this play has already fallen back to a fresh re-add.
    stream_recovered: bool,
    /// The egui context for the current frame, so UI actions triggered deep in
    /// the widget tree can still start playback.
    pending_ctx: Option<egui::Context>,
    /// Last watch position handed to the backend, so we do not write on every
    /// frame.
    watch_last_recorded: f64,
    watch_recording_for: Option<String>,
}

struct Toast {
    message: String,
    error: bool,
    shown: Instant,
}

const REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const TOAST_LIFETIME: Duration = Duration::from_secs(6);
/// How many entries each catalog row shows.
const ROW_LIMIT: usize = 30;
/// Height assumed for a search card before it has been drawn once. The real
/// height is measured and remembered the first time the card is on screen.
const SEARCH_CARD_HEIGHT: f32 = 140.0;
/// Vertical gap between search cards.
const SEARCH_CARD_GAP: f32 = 8.0;
/// Width of the left (identity) column of a Work view.
const WORK_LEFT_COL: f32 = 300.0;
/// Record watch progress at most every this many seconds of playback.
const WATCH_RECORD_INTERVAL: f64 = 5.0;

impl App {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        let mut app = Self {
            backend,
            nav: Nav::new(Screen::Home),
            player_open: false,
            add_open: false,
            focus_search: false,
            library: Vec::new(),
            works: Vec::new(),
            rows: Vec::new(),
            show_remove: None,
            remove_selected: HashSet::new(),
            remove_files: false,
            last_refresh: Instant::now() - REFRESH_INTERVAL,
            toast: None,
            add_source: String::new(),
            add_media_only: true,
            filter: String::new(),
            search_query: String::new(),
            search_for: None,
            search_hits: Vec::new(),
            search_note: None,
            searching: false,
            search_year: String::new(),
            search_season: String::new(),
            search_episode: String::new(),
            search_min_seeders: String::new(),
            search_sort: SearchSort::Relevance,
            search_layout: SearchLayout::Grid,
            filters_open: false,
            search_failed: false,
            search_streaming: false,
            search_failures: Vec::new(),
            search_releases: HashMap::new(),
            search_series_views: HashMap::new(),
            search_actions: HashMap::new(),
            search_view: None,
            result_anchor: None,
            search_revision: 0,
            search_metadata_revision: 0,
            search_work_heights: Vec::new(),
            api_key_input: String::new(),
            settings_draft: None,
            settings_section: SettingsSection::General,
            player: PlayerController::new(),
            player_last_activity: Instant::now(),
            player_controls_hidden: false,
            player_controls_rect: None,
            player_topbar_rect: None,
            player_help: false,
            player_origin: None,
            pending_play: None,
            player_last_frames: 0,
            player_progress_position: 0.0,
            player_progress_at: Instant::now(),
            stream_recovered: false,
            pending_ctx: None,
            watch_last_recorded: 0.0,
            watch_recording_for: None,
        };
        // Saved settings are canonical: apply playback preferences immediately.
        let settings = app.backend.settings();
        app.player.set_volume(settings.default_volume);
        app.player
            .set_preferences(crate::player::PlaybackPreferences {
                subtitles_enabled: settings.subtitles_enabled,
                subtitle_language: settings.subtitle_language.clone(),
                volume: settings.default_volume,
            });
        app.refresh();
        app
    }

    /// The screen currently shown. Exposed for tests.
    pub fn screen(&self) -> &Screen {
        self.nav.screen()
    }

    /// The rail destination the current trail is rooted at. Exposed for tests.
    pub fn root(&self) -> &Screen {
        self.nav.root()
    }

    /// The navigation trail, deepest last. Exposed for tests.
    pub fn nav_trail(&self) -> &[Screen] {
        self.nav.trail()
    }

    /// Whether the player overlay is showing. Exposed for tests.
    pub fn is_player(&self) -> bool {
        self.player_open
    }

    /// Whether the Add modal is showing. Exposed for tests.
    pub fn is_add_open(&self) -> bool {
        self.add_open
    }

    /// Open the global Add modal. Exposed for tests and the debug tool.
    pub fn open_add(&mut self) {
        self.add_open = true;
    }

    /// Pushed views: `Work` stands on top of the root it was opened from, so
    /// "back" returns there rather than to a hardcoded Home.
    pub fn open_work(&mut self, work_id: usize) {
        self.nav.open(Screen::Work(work_id));
    }

    /// Open a search result's title view, by its work key.
    pub fn open_result(&mut self, key: impl Into<String>) {
        self.nav.open(Screen::Result(key.into()));
    }

    /// Walk one step back. Returns whether there was anywhere to go.
    pub fn back(&mut self) -> bool {
        self.nav.back()
    }

    /// Go to a rail destination.
    pub fn go_to(&mut self, root: Screen) {
        debug_assert!(root.is_root(), "go_to takes a rail destination");
        self.nav.go(root);
    }

    /// Number of entries in the cached library. Exposed for tests.
    pub fn library_len(&self) -> usize {
        self.library.len()
    }

    /// The settings the backend currently holds. Exposed for tests.
    pub fn settings(&self) -> CatalogSettings {
        self.backend.settings()
    }

    /// Frames the player has decoded and uploaded. Exposed for tests.
    pub fn frames_uploaded(&self) -> u64 {
        self.player.frames_uploaded()
    }

    /// The player's current error, if any. Exposed for tests.
    pub fn player_error(&self) -> Option<String> {
        self.player.error().map(str::to_string)
    }

    /// The player's transport state. Exposed for tests.
    pub fn player_state(&self) -> reel_player::PlayerState {
        self.player.state()
    }

    /// Row headings currently laid out. Exposed for tests.
    pub fn row_titles(&self) -> Vec<String> {
        self.rows.iter().map(|row| row.title.clone()).collect()
    }

    /// Go to a location. A rail destination replaces the trail; a view is
    /// pushed on top of wherever you are. Kept for tests and the debug tool.
    pub fn navigate(&mut self, screen: Screen) {
        if screen.is_root() {
            self.nav.go(screen);
        } else {
            self.nav.open(screen);
        }
    }

    pub fn set_add_source(&mut self, source: impl Into<String>) {
        self.add_source = source.into();
    }

    pub fn submit_add(&mut self) {
        let source = self.add_source.trim().to_string();
        if source.is_empty() {
            self.warn("Paste a magnet link, a .torrent URL, or an info hash first");
            return;
        }
        self.backend.add(&source, self.add_media_only);
        self.set_toast("Adding torrent\u{2026}", false);
        self.add_source.clear();
    }

    /// Numbers of results currently shown. Exposed for tests.
    pub fn search_result_count(&self) -> usize {
        self.search_hits.len()
    }

    pub fn set_search_query(&mut self, query: impl Into<String>) {
        self.search_query = query.into();
    }

    pub fn submit_search(&mut self) {
        let text = self.search_query.trim().to_string();
        if text.is_empty() {
            self.warn("Type something to search for");
            return;
        }

        let mut query = reel_catalog::SearchQuery::new(text);
        query.year = self.search_year.trim().parse().ok();
        query.season = self.search_season.trim().parse().ok();
        query.episode = self.search_episode.trim().parse().ok();

        self.searching = true;
        self.search_note = None;
        self.search_failed = false;
        self.search_for = Some(query.describe());
        self.backend.search(&query);
    }

    pub fn item(&self, id: usize) -> Option<LibraryItem> {
        self.library
            .iter()
            .find(|item| item.torrent.id == id)
            .cloned()
    }

    /// The work whose lead torrent is `id`. Screens are keyed by that id.
    pub fn work(&self, id: usize) -> Option<Work> {
        self.works
            .iter()
            .find(|work| work.lead_torrent_id() == id)
            .cloned()
    }

    /// Every work currently in the library. Exposed for tests.
    pub fn works(&self) -> &[Work] {
        &self.works
    }

    /// The work a torrent belongs to, by any of its member ids.
    pub fn work_id_for_torrent(&self, torrent_id: usize) -> Option<usize> {
        self.works
            .iter()
            .find(|work| {
                work.members
                    .iter()
                    .any(|member| member.torrent_id() == torrent_id)
            })
            .map(Work::lead_torrent_id)
    }

    /// Engine state for the work's best member.
    fn lead_item(&self, work: &Work) -> Option<LibraryItem> {
        self.item(work.lead_torrent_id())
    }

    /// The episode Play should start: the newest resumable one, else the first.
    ///
    /// The copy used is the one the user chose for that episode (falling back to
    /// the best), so "remember the chosen copy" holds when Play starts a series
    /// and not only when the copy chooser was used.
    pub fn preferred_episode(&self, work: &Work) -> Option<(usize, usize)> {
        let episodes = work.episodes();
        for episode in &episodes {
            let Some(variant) = self.chosen_variant(work, episode) else {
                continue;
            };
            // Exact, not the torrent-level fallback: a series must never treat
            // episode 1's position as episode 2's.
            let resumable = self
                .item(variant.torrent_id)
                .and_then(|item| item.entry.watch_by_file.get(&variant.file_id).cloned())
                .is_some_and(|progress| progress.is_resumable());
            if resumable {
                return Some((variant.torrent_id, variant.file_id));
            }
        }
        episodes
            .first()
            .and_then(|episode| self.chosen_variant(work, episode))
            .map(|variant| (variant.torrent_id, variant.file_id))
    }

    /// Play a whole work: the best version of a film, or its next episode.
    pub fn play_work(&mut self, ctx: &egui::Context, work_id: usize) {
        let Some(work) = self.work(work_id) else {
            self.warn("That title is no longer in the library");
            return;
        };
        if work.is_series() {
            match self.preferred_episode(&work) {
                Some((torrent_id, file_id)) => self.play_file(ctx, torrent_id, file_id),
                None => self.warn("This series has no playable episode yet"),
            }
        } else {
            match work.versions().first() {
                Some(version) => self.play_file(ctx, version.torrent_id, version.file_id),
                None => self.warn("This title has no playable file yet"),
            }
        }
    }

    /// The file "Start over" and "Mark watched" act on: the best copy of a
    /// film, or the first episode of a series.
    fn work_target(&self, work: &Work) -> Option<(usize, usize)> {
        if work.is_series() {
            work.episodes()
                .first()
                .and_then(|episode| episode.preferred())
                .map(|variant| (variant.torrent_id, variant.file_id))
        } else {
            work.versions()
                .first()
                .map(|version| (version.torrent_id, version.file_id))
        }
    }

    /// Forget the saved position and play the work from the beginning.
    fn restart_work(&mut self, work_id: usize) {
        let Some(work) = self.work(work_id) else {
            return;
        };
        let Some((torrent_id, file_id)) = self.work_target(&work) else {
            return;
        };
        if let Some(item) = self.item(torrent_id) {
            self.backend.forget_watch(&item.entry.info_hash, file_id);
        }
        self.refresh();
        if let Some(ctx) = self.pending_ctx.clone() {
            self.play_file(&ctx, torrent_id, file_id);
        }
    }

    /// Mark the work's target file finished.
    fn mark_work_watched(&mut self, work: &Work) {
        if let Some((torrent_id, file_id)) = self.work_target(work) {
            if let Some(item) = self.item(torrent_id) {
                self.backend.mark_finished(
                    &item.entry.info_hash,
                    file_id,
                    Some(work.title.clone()),
                );
            }
        }
        self.refresh();
    }

    /// Prepare to play a file, then open the player once the backend says it is
    /// ready.
    ///
    /// The backend may have to re-add the torrent (a stream that was released,
    /// or a switch between streaming and downloading), and a re-add gets a new
    /// engine id. Opening the player immediately would hand mpv the *old*
    /// stream URL, which then 404s and never plays. So the play is deferred
    /// until [`crate::backend::BackendEvent::Ready`], and the URL is read from
    /// the refreshed library at that point.
    pub fn play_file(&mut self, _ctx: &egui::Context, torrent_id: usize, file_id: usize) {
        let Some(item) = self.item(torrent_id) else {
            self.warn("That torrent is no longer in the library");
            return;
        };
        let Some(file) = item.torrent.files.iter().find(|f| f.id == file_id) else {
            self.warn("That file is no longer in the torrent");
            return;
        };

        // Sidecar subtitles ride along with the video, both as files to fetch
        // and as URLs for mpv to load.
        let companions = companion_subtitles(&item.torrent.files, file);
        let mut selection = vec![file_id];
        selection.extend(companions.iter().copied());
        selection.sort_unstable();
        selection.dedup();

        self.stream_recovered = false;
        self.start_streaming(torrent_id, &selection);
        self.pending_play = Some((torrent_id, file_id));
        self.set_toast("Preparing to stream\u{2026}", false);
    }

    /// Open the player for a file that the backend has confirmed is ready.
    fn open_player(&mut self, torrent_id: usize, file_id: usize) {
        let Some(item) = self.item(torrent_id) else {
            self.warn("That torrent is no longer in the library");
            return;
        };
        let Some(file) = item.torrent.files.iter().find(|f| f.id == file_id) else {
            self.warn("That file is no longer in the torrent");
            return;
        };

        // Resume the *file* that was actually being watched, not the torrent as
        // a whole: a series is one torrent with many episodes. The torrent-level
        // record is only a fallback for a single-file title.
        let start_at = resume_start_at(&item, file_id);

        let companions = companion_subtitles(&item.torrent.files, file);
        let subtitles: Vec<(String, String)> = companions
            .iter()
            .filter_map(|id| item.torrent.files.iter().find(|f| f.id == *id))
            .map(|sub| {
                let url =
                    sub.stream.url.clone().unwrap_or_else(|| {
                        format!("{}{}", self.backend.base_url(), sub.stream.path)
                    });
                (url, sub.name.clone())
            })
            .collect();

        let info = PlaybackInfo {
            torrent_id,
            file_id,
            info_hash: item.entry.info_hash.clone(),
            title: item.heading(),
            file_name: file.name.clone(),
            fallback_duration: None,
            start_at,
            subtitles,
        };

        let url = file
            .stream
            .url
            .clone()
            .unwrap_or_else(|| format!("{}{}", self.backend.base_url(), file.stream.path));

        let others = item
            .torrent
            .files
            .iter()
            .filter(|f| f.included && f.id != file_id && !companions.contains(&f.id))
            .count();
        if others > 0 {
            self.set_toast(
                format!(
                    "Streaming {} ({} other file{} not fetched)",
                    reel_core::title::truncate(&file.name, 40),
                    others,
                    if others == 1 { "" } else { "s" }
                ),
                false,
            );
        }

        let capability = self.backend.capabilities().player.clone();
        self.player_origin = Some(torrent_id);
        self.player_last_frames = 0;
        self.player_progress_position = 0.0;
        self.player_progress_at = Instant::now();
        self.watch_last_recorded = start_at.unwrap_or(0.0);
        self.watch_recording_for = Some(item.entry.info_hash.clone());

        tracing::info!(%url, start_at = ?start_at, live = file.included, "opening the player");
        let ctx = self.pending_ctx.clone().unwrap_or_default();
        match self.player.open(&ctx, &capability, &url, info) {
            Ok(()) => {
                self.player_open = true;
                self.toast = None;
            }
            Err(e) => {
                self.toast = Some(Toast {
                    message: e,
                    error: true,
                    shown: Instant::now(),
                });
            }
        }
    }

    /// Mark the cached library stale, so the next frame re-reads the engine.
    ///
    /// Engine changes (which files are selected, progress) are applied on the
    /// runtime, so reading straight back can race them; re-reading on the next
    /// frame is both simpler and correct.
    fn invalidate(&mut self) {
        self.last_refresh = Instant::now() - REFRESH_INTERVAL;
    }

    fn refresh(&mut self) {
        self.library = self.backend.library();
        self.drive_search_enrich();

        // Two torrents of the same film or show become one work, so the library
        // shows versions of a title rather than unrelated cards.
        let members: Vec<WorkMember> = self
            .library
            .iter()
            .map(|item| WorkMember::new(item.entry.clone(), item.torrent.primary_file_id))
            .collect();
        let merge = self.backend.settings().merge_works;
        self.works = if merge {
            build_works(&members)
        } else {
            separate_works(&members)
        };

        self.rows = build_work_rows(&self.works, ROW_LIMIT)
            .into_iter()
            .map(|row| RowLayout {
                kind: row.kind,
                title: row.title,
                ids: row.works.iter().map(Work::lead_torrent_id).collect(),
            })
            .collect();

        self.last_refresh = Instant::now();
    }

    /// Keep filling in metadata for search results as lookup slots free up.
    ///
    /// Split out of [`Self::refresh`] so a search-metadata event can advance the
    /// pipeline without rebuilding the library — the settings page, which shows
    /// none of it, would otherwise do that work on every arrival.
    fn drive_search_enrich(&mut self) {
        if !self.searching && !self.search_hits.is_empty() {
            self.backend.enrich_search(&self.search_hits);
        }
    }

    /// Note that the search hits changed, so the derived view is rebuilt.
    fn bump_search_revision(&mut self) {
        self.search_revision = self.search_revision.wrapping_add(1);
    }

    /// The current inputs to the derived search view.
    fn search_signature(&self) -> SearchSignature {
        SearchSignature {
            query: self.search_for.clone(),
            min_seeders: self.search_min_seeders.clone(),
            sort: self.search_sort,
            hits: self.search_revision,
            metadata: self.search_metadata_revision,
        }
    }

    /// Derive the view the search page draws from the raw hits.
    fn build_search_view(&mut self, signature: SearchSignature) -> SearchView {
        let min_seeders: u32 = self.search_min_seeders.trim().parse().unwrap_or(0);
        let mut shown = self.search_hits.clone();
        if min_seeders > 0 {
            shown.retain(|hit| hit.seeders.is_none_or(|seeders| seeders >= min_seeders));
        }
        self.search_sort.apply(&mut shown);

        // Parse each result once, and remember it by title so a filter change
        // does not re-parse everything.
        let releases: Vec<Release> = shown
            .iter()
            .map(|hit| match self.search_releases.get(&hit.title) {
                Some(release) => release.clone(),
                None => {
                    let release = reel_catalog::release::analyse_name(&hit.title);
                    self.search_releases
                        .insert(hit.title.clone(), release.clone());
                    release
                }
            })
            .collect();

        // Metadata matched so far, read once per rebuild rather than per frame.
        let metas: Vec<Option<Metadata>> = shown
            .iter()
            .map(|hit| self.backend.search_metadata(hit))
            .collect();

        let works = self.build_search_works(&shown, &releases, &metas);

        SearchView {
            signature,
            shown,
            releases,
            works,
        }
    }

    /// Group the shown hits into one work per film or series, best match first.
    fn build_search_works(
        &self,
        shown: &[SearchHit],
        releases: &[Release],
        metas: &[Option<Metadata>],
    ) -> Vec<Work> {
        // One member per hit; `torrent_id` is its index, so a member maps back
        // to its hit. Reusing `build_works` keeps the notion of "same title"
        // identical to the library.
        let members: Vec<WorkMember> = shown
            .iter()
            .enumerate()
            .map(|(index, hit)| {
                let entry = CatalogEntry {
                    torrent_id: index,
                    info_hash: format!("search-{index}"),
                    display_title: hit.title.clone(),
                    year: hit.year,
                    metadata: metas[index].clone(),
                    watch: None,
                    release: releases[index].clone(),
                    watch_by_file: Default::default(),
                };
                WorkMember::new(entry, releases[index].episodes.first().map(|e| e.file_id))
            })
            .collect();

        let mut works = build_works(&members);
        // Order by how well the work's title matches the query, then by the
        // order the backends returned — so an exact hit stays on top even when a
        // source ranks a fuzzy match (an episode title, say) above it.
        let query = self.search_for.clone().unwrap_or_default();
        works.sort_by_key(|work| {
            let first = work
                .members
                .iter()
                .map(WorkMember::torrent_id)
                .min()
                .unwrap_or(usize::MAX);
            (std::cmp::Reverse(work_score(work, &query)), first)
        });
        works
    }

    fn maybe_refresh(&mut self) {
        // The player needs fresh engine stats for its overlay, and the library
        // needs new metadata as it arrives, so both refresh on one cadence.
        if self.screen() != &Screen::Settings && self.last_refresh.elapsed() >= REFRESH_INTERVAL {
            self.refresh();
        }
    }

    /// Whether something is moving without input, so the UI has to keep
    /// repainting to show it.
    fn active_transfers(&self) -> bool {
        self.library.iter().any(|item| {
            item.torrent.stats.download_bps > 0 || (item.downloading && !item.torrent.finished)
        })
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        for event in self.backend.take_events() {
            match event {
                BackendEvent::Info(message) => {
                    self.toast = Some(Toast {
                        message,
                        error: false,
                        shown: Instant::now(),
                    });
                }
                BackendEvent::Error(message) => {
                    self.toast = Some(Toast {
                        message,
                        error: true,
                        shown: Instant::now(),
                    });
                }
                BackendEvent::Added { id, title } => {
                    self.toast = Some(Toast {
                        message: format!("Added {title}"),
                        error: false,
                        shown: Instant::now(),
                    });
                    self.refresh();
                    // A new torrent may be another version of something already
                    // held, so open the work it joined rather than the torrent.
                    let work_id = self.work_id_for_torrent(id).unwrap_or(id);
                    self.open_work(work_id);
                }
                BackendEvent::Metadata { .. } | BackendEvent::WatchUpdated => {
                    // Only these change the library, and the settings page does
                    // not show it, so it is left alone there.
                    if self.screen() != &Screen::Settings {
                        self.refresh();
                    }
                }
                BackendEvent::SearchMetadata => {
                    // A rating or poster arrived for a result: the derived view
                    // has to be rebuilt to pick it up. This does not touch the
                    // library, so only the search pipeline is advanced.
                    self.search_metadata_revision = self.search_metadata_revision.wrapping_add(1);
                    self.drive_search_enrich();
                }
                BackendEvent::SearchLookupReady => {
                    // A lookup slot freed up (or a miss was recorded): let the
                    // next one start without rebuilding anything.
                    self.drive_search_enrich();
                }
                BackendEvent::Ready { info_hash } => {
                    self.refresh();
                    if let Some((torrent_id, file_id)) = self.pending_play {
                        let matches = self
                            .item(torrent_id)
                            .is_some_and(|item| item.entry.info_hash == info_hash);
                        if matches {
                            self.pending_play = None;
                            self.open_player(torrent_id, file_id);
                        }
                    }
                }
                BackendEvent::PlayReady {
                    torrent_id,
                    file_id,
                } => {
                    // The files have resolved, so this is a normal play from
                    // here: start the stream and let `Ready` open the player.
                    tracing::debug!(
                        torrent_id,
                        file_id,
                        "opening the player for a search result"
                    );
                    self.refresh();
                    self.play_file(ctx, torrent_id, file_id);
                }
                BackendEvent::SearchActionProgress { key, phase } => {
                    self.search_actions.insert(key, phase);
                }
                BackendEvent::SearchActionFinished { key } => {
                    self.search_actions.remove(&key);
                }
                BackendEvent::ApiKeyChecked { ok, message } => {
                    self.set_toast(message, !ok);
                }
                BackendEvent::SearchResults { query, results } => {
                    // A late answer for an older query must not replace what the
                    // user is looking at now.
                    if self.search_for.as_deref() == Some(query.as_str()) {
                        self.searching = false;
                        // "Nothing matched" and "every source failed" are
                        // different things and must read differently.
                        self.search_failed =
                            results.hits.is_empty() && !results.failures.is_empty();
                        self.search_failures = results.failures.clone();
                        self.search_note = describe_search(&results);
                        self.search_hits = results.hits;
                        self.search_releases.clear();
                        self.search_series_views.clear();
                        // A new result set means the measured row heights are
                        // for the wrong rows.
                        self.search_work_heights.clear();
                        self.bump_search_revision();
                        // A source that paginates keeps sending more.
                        self.search_streaming = true;
                        self.backend.enrich_search(&self.search_hits);
                    }
                }
                BackendEvent::SearchHitsAdded { query, hits } => {
                    if self.search_for.as_deref() == Some(query.as_str()) {
                        let mut known: HashSet<String> =
                            self.search_hits.iter().map(SearchHit::key).collect();
                        let fresh: Vec<SearchHit> = hits
                            .into_iter()
                            .filter(|hit| known.insert(hit.key()))
                            .collect();
                        if !fresh.is_empty() {
                            self.backend.enrich_search(&fresh);
                            self.search_hits.extend(fresh);
                            self.search_note = describe_search_summary(
                                self.search_hits.len(),
                                &self.search_failures,
                            );
                            self.bump_search_revision();
                        }
                    }
                }
                BackendEvent::SearchStreamEnded { query } => {
                    if self.search_for.as_deref() == Some(query.as_str()) {
                        self.search_streaming = false;
                    }
                }
                BackendEvent::SearchFailed { query, message } => {
                    if self.search_for.as_deref() == Some(query.as_str()) {
                        self.searching = false;
                        self.search_streaming = false;
                        self.search_hits.clear();
                        self.search_releases.clear();
                        self.search_series_views.clear();
                        self.search_failed = true;
                        self.search_note = Some(message);
                        self.search_work_heights.clear();
                        self.bump_search_revision();
                    }
                }
            }
            ctx.request_repaint();
        }
    }

    fn set_toast(&mut self, message: impl Into<String>, error: bool) {
        self.toast = Some(Toast {
            message: message.into(),
            error,
            shown: Instant::now(),
        });
    }

    fn start_search_action(&mut self, hit: SearchHit, play: bool) {
        let phase = if hit.magnet.is_none() && hit.torrent_url.is_none() {
            SearchActionPhase::ResolvingMagnet
        } else {
            SearchActionPhase::Adding
        };
        let key = hit.key();
        let title = reel_core::title::truncate(&hit.title, 40);
        self.search_actions.insert(key, phase);
        if play {
            self.backend.play_search_hit(hit, self.add_media_only);
        } else {
            self.backend.add_search_hit(hit, self.add_media_only);
        }
        self.set_toast(format!("{} {title}", phase.label()), false);
        if let Some(ctx) = &self.pending_ctx {
            ctx.request_repaint();
        }
    }

    fn warn(&mut self, message: impl Into<String>) {
        self.set_toast(message, true);
    }

    fn selected_work(&self) -> Option<Work> {
        match self.screen() {
            Screen::Work(id) => self.work(*id),
            _ => None,
        }
    }

    /// Stats for the player overlay, taken from the engine rather than mpv.
    fn playback_stats(&self, torrent_id: usize) -> PlaybackStats {
        let Some(item) = self.item(torrent_id) else {
            return PlaybackStats::default();
        };
        PlaybackStats {
            download_bps: item.torrent.stats.download_bps,
            peers: item.torrent.stats.peers.live,
            file_percent: item.torrent.stats.percent,
        }
    }

    /// Change which files a torrent is set to fetch, without starting it.
    ///
    /// A checkbox is a plan, not a command: ticking one on a paused pack must
    /// not quietly resume the whole torrent. Play and Download are what start
    /// fetching; this only edits the selection.
    pub fn select_files(&mut self, torrent_id: usize, files: &[usize]) {
        if files.is_empty() {
            self.warn("Choose at least one file to fetch");
            return;
        }
        self.backend.set_only_files(torrent_id, files);
        self.invalidate();
    }

    /// The one place the app starts fetching a selection.
    pub fn start_streaming(&mut self, torrent_id: usize, files: &[usize]) {
        if files.is_empty() {
            self.warn("Choose at least one file to fetch");
            return;
        }
        self.backend.start_files(torrent_id, files);
        self.invalidate();
    }

    /// Start fetching selections from several torrents at once.
    ///
    /// A season can live in one torrent or be spread across several; each
    /// torrent is given only the files that belong to it, added to whatever it
    /// is already fetching so a download is never silently cancelled.
    pub fn start_streaming_many(&mut self, selections: &[(usize, Vec<usize>)]) {
        for (torrent_id, files) in selections {
            if files.is_empty() {
                continue;
            }
            let mut next = files.clone();
            if let Some(item) = self.item(*torrent_id) {
                for file in item.torrent.files.iter().filter(|file| file.included) {
                    if !next.contains(&file.id) {
                        next.push(file.id);
                    }
                }
            }
            next.sort_unstable();
            next.dedup();
            self.backend.start_files(*torrent_id, &next);
        }
        self.invalidate();
    }

    /// The episodes of a season as media-keyed downloads, using the chosen
    /// copy of each episode (or the best one).
    fn season_downloads(&self, work: &Work, season: Option<u32>) -> Vec<MediaDownload> {
        let mut items = Vec::new();
        for group in work.seasons() {
            if group.number != season {
                continue;
            }
            for episode in &group.episodes {
                let Some(variant) = self.chosen_variant(work, episode) else {
                    continue;
                };
                items.push(MediaDownload {
                    key: episode_key(work, episode),
                    torrent_id: variant.torrent_id,
                    files: vec![variant.file_id],
                });
            }
        }
        items
    }

    /// The copy of an episode the user picked, else the best one.
    fn chosen_variant<'a>(
        &'a self,
        work: &'a Work,
        episode: &'a WorkEpisode,
    ) -> Option<&'a reel_catalog::EpisodeVariant> {
        let key = episode_key(work, episode);
        if let Some((torrent_id, file_id)) = self.backend.chosen_source(&key) {
            if let Some(variant) = episode
                .variants
                .iter()
                .find(|variant| variant.torrent_id == torrent_id && variant.file_id == file_id)
            {
                return Some(variant);
            }
        }
        episode.preferred()
    }

    /// Persist how far playback has got, but not on every frame.
    fn record_watch_progress(&mut self) {
        let Some(info_hash) = self.watch_recording_for.clone() else {
            return;
        };
        let info = self.player.current().cloned();
        let state = self.player.state();
        if !state.loaded || state.position < 1.0 {
            return;
        }
        if (state.position - self.watch_last_recorded).abs() < WATCH_RECORD_INTERVAL {
            return;
        }
        self.watch_last_recorded = state.position;
        self.backend.record_watch(
            &info_hash,
            info.as_ref().map(|i| i.file_id).unwrap_or(0),
            info.as_ref().map(|i| i.file_name.clone()),
            info.as_ref().map(|i| i.title.clone()),
            state.position,
            state.duration,
        );
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pending_ctx = Some(ctx.clone());
        self.maybe_refresh();
        self.drain_events(&ctx);

        // Progress, transfer rates and peer counts come from the engine and
        // change with no input, so keep repainting while anything is active.
        // Metadata lookups and a search still streaming count too: without that,
        // a rating or a later page that arrives after the user stops moving the
        // mouse would not appear until they moved it.
        if self.active_transfers()
            || self.backend.pending_lookups() > 0
            || self.search_streaming
            || !self.search_actions.is_empty()
        {
            ctx.request_repaint_after(REFRESH_INTERVAL);
        }

        // The player is an overlay: it covers the shell and returns to whatever
        // the trail already shows when it closes.
        if self.player_open {
            self.player_screen(ui, &ctx);
            self.draw_toast(ui, &ctx);
            return;
        }

        self.handle_shortcuts(&ctx);

        // The rail collapses to icons on a narrow window.
        let rail_width = if ui.available_width() < 1040.0 {
            60.0
        } else {
            200.0
        };
        egui::Panel::left("rail")
            .exact_size(rail_width)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                egui::Frame::NONE
                    .fill(theme::SURFACE)
                    .inner_margin(egui::Margin::symmetric(10, 12)),
            )
            .show(ui, |ui| self.rail(ui, rail_width));

        // The header only earns its place when there is somewhere to go back to
        // (a pushed Work/Result view). Root views carry their own titles and
        // controls, so no bar is drawn over them.
        if self.nav.can_back() {
            egui::Panel::top("header")
                .frame(
                    egui::Frame::NONE
                        .fill(theme::BG)
                        .inner_margin(egui::Margin::symmetric(18, 10)),
                )
                .show(ui, |ui| self.header(ui));
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::symmetric(20, 6)),
            )
            .show(ui, |ui| match self.screen().clone() {
                Screen::Home => self.library_screen(ui),
                Screen::Work(_) => self.detail_screen(ui),
                Screen::Result(key) => self.search_result_screen(ui, &key),
                Screen::Discover => self.search_screen(ui),
                Screen::Activity => self.activity_screen(ui),
                Screen::Settings => self.settings_screen(ui),
            });

        self.draw_add_modal(&ctx);
        self.draw_toast(ui, &ctx);
        self.draw_remove_dialog(ui, &ctx);
    }

    fn on_exit(&mut self) {
        self.player.close();
    }
}

/// The egui context is needed by UI actions that can also be triggered by
/// keyboard shortcuts deep inside the widget tree. The app stores it each frame
/// so those paths can start playback.
impl App {
    pub fn set_context(&mut self, ctx: egui::Context) {
        self.pending_ctx = Some(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::sample_library;

    fn app() -> App {
        App::new(Box::new(crate::backend::FakeBackend::new(sample_library())))
    }

    #[test]
    fn a_slow_or_buffering_stream_is_not_a_wedge() {
        use reel_player::PlayerState;

        // Loading: no file yet, core idle.
        let loading = PlayerState {
            loaded: false,
            idle: true,
            ..Default::default()
        };
        assert!(stream_looks_busy(&loading));
        assert!(!playback_is_wedged(true, false, 0, 0, 0.0, 0.0, false));

        // Rebuffering mid-play: paused-for-cache.
        let buffering = PlayerState {
            loaded: true,
            paused_for_cache: true,
            buffering_percent: Some(20.0),
            ..Default::default()
        };
        assert!(stream_looks_busy(&buffering));

        // Cache draining but core busy: still buffering.
        let draining = PlayerState {
            loaded: true,
            buffering_percent: Some(50.0),
            ..Default::default()
        };
        assert!(stream_looks_busy(&draining));

        // Playing normally: not busy, but the clock is moving.
        let playing = PlayerState {
            loaded: true,
            position: 12.0,
            ..Default::default()
        };
        assert!(!stream_looks_busy(&playing));
        assert!(!playback_is_wedged(
            false, false, 100, 100, 12.0, 10.0, false
        ));

        // A still scene holds the picture but the clock still moves.
        assert!(!playback_is_wedged(
            false, false, 100, 100, 12.0, 10.0, false
        ));

        // A slow torrent still delivering bytes is never a wedge.
        assert!(!playback_is_wedged(
            false, true, 100, 100, 10.0, 10.0, false
        ));

        // A seek still buffering is not a wedge.
        assert!(!playback_is_wedged(
            false, false, 100, 100, 10.0, 10.0, true
        ));

        // Nothing moving, nothing buffering, no bytes: a wedge.
        assert!(playback_is_wedged(
            false, false, 100, 100, 10.0, 10.0, false
        ));
    }

    #[test]
    fn starts_on_the_library_with_rows() {
        let app = app();
        assert_eq!(app.screen(), &Screen::Home);
        assert_eq!(app.library_len(), 3);

        let rows = app.row_titles();
        assert_eq!(
            rows,
            [
                "Continue watching",
                "Recently added",
                "Not started",
                "Watched"
            ],
            "every row should have something in the sample library"
        );
    }

    #[test]
    fn navigation_changes_screens() {
        let mut app = app();
        app.go_to(Screen::Settings);
        assert_eq!(app.screen(), &Screen::Settings);
        app.go_to(Screen::Discover);
        assert_eq!(app.screen(), &Screen::Discover);

        // A pushed view leaves a trail back to the root it came from.
        app.open_work(1);
        assert_eq!(app.screen(), &Screen::Work(1));
        assert_eq!(app.root(), &Screen::Discover);
        assert!(app.back());
        assert_eq!(app.screen(), &Screen::Discover);
        assert!(!app.back(), "the root is the end of the trail");
    }

    #[test]
    fn a_work_opened_from_home_returns_home() {
        let mut app = app();
        app.open_work(1);
        assert_eq!(app.nav_trail(), &[Screen::Home, Screen::Work(1)]);
        assert!(app.back());
        assert_eq!(app.screen(), &Screen::Home);
    }

    #[test]
    fn opening_a_result_pushes_it_over_discover() {
        let mut app = app();
        app.go_to(Screen::Discover);
        app.search_for = Some("the bear".into());
        let mut hit = SearchHit::new("The Bear S02E01 1080p", "ext.to");
        hit.seeders = Some(40);
        app.search_hits = vec![hit];

        let signature = app.search_signature();
        let view = app.build_search_view(signature);
        let key = view.works[0].key.clone();
        app.search_view = Some(view);

        app.open_result(key.clone());
        assert_eq!(app.screen(), &Screen::Result(key.clone()));
        assert_eq!(app.root(), &Screen::Discover);
        assert_eq!(app.nav_trail(), &[Screen::Discover, Screen::Result(key)]);
        assert!(app.back());
        assert_eq!(app.screen(), &Screen::Discover);
    }

    #[test]
    fn an_open_result_picks_up_late_hits() {
        let mut app = app();
        app.search_for = Some("the bear".into());
        let mut first = SearchHit::new("The Bear S02E01 1080p", "ext.to");
        first.seeders = Some(40);
        app.search_hits = vec![first];

        let signature = app.search_signature();
        let view = app.build_search_view(signature);
        let key = view.works[0].key.clone();
        let anchor = view.shown[view.works[0].members[0].torrent_id()].key();
        app.search_view = Some(view);
        app.result_anchor = Some(anchor.clone());

        // A later page adds another release of the same title while the Result
        // view is open (Discover is not drawn, so nothing else refreshes it).
        let mut second = SearchHit::new("The Bear S02E02 1080p", "ext.to");
        second.seeders = Some(30);
        app.search_hits.push(second);
        app.bump_search_revision();

        let signature = app.search_signature();
        let view = app.build_search_view(signature);
        assert_eq!(view.works.len(), 1, "both releases group into one title");
        assert_eq!(view.works[0].members.len(), 2, "the late hit joined it");
        assert!(
            view.works.iter().any(|work| work.key == key)
                || view
                    .works
                    .iter()
                    .any(|work| work_contains_hit(work, &view.shown, &anchor)),
            "the open result is still found after the rebuild"
        );
    }

    #[test]
    fn adding_requires_a_source() {
        let mut app = app();

        app.submit_add();
        assert!(app.toast.as_ref().is_some_and(|t| t.error));

        app.set_add_source("magnet:?xt=urn:btih:abc");
        app.submit_add();
        assert!(app.toast.as_ref().is_some_and(|t| !t.error));
        assert_eq!(app.add_source, "", "the field should be cleared on submit");
    }

    #[test]
    fn search_action_shows_immediate_resolving_feedback_and_clears_on_completion() {
        let mut app = app();
        let mut hit = SearchHit::new("A page-only result", "ext.to");
        hit.detail_url = Some("https://example.test/torrent/1".to_string());
        let key = hit.key();

        app.start_search_action(hit, false);

        assert_eq!(
            app.search_actions.get(&key),
            Some(&SearchActionPhase::ResolvingMagnet)
        );
        assert!(
            app.toast
                .as_ref()
                .unwrap()
                .message
                .contains("Fetching magnet")
        );

        app.drain_events(&egui::Context::default());
        assert!(!app.search_actions.contains_key(&key));
    }

    #[test]
    fn the_derived_search_view_filters_and_groups() {
        let mut app = app();
        app.search_for = Some("the bear".into());
        let mut first = SearchHit::new("The Bear S02E01 1080p", "ext.to");
        first.seeders = Some(40);
        let mut second = SearchHit::new("The Bear S02E02 1080p", "ext.to");
        second.seeders = Some(4);
        let mut other = SearchHit::new("Unrelated Film 1999 2160p", "ext.to");
        other.seeders = Some(80);
        app.search_hits = vec![first, second, other];
        app.search_min_seeders = "10".into();

        let signature = app.search_signature();
        let view = app.build_search_view(signature);
        assert_eq!(
            view.shown.len(),
            2,
            "the low-seeder release is filtered out"
        );
        assert_eq!(view.shown[0].title, "The Bear S02E01 1080p");
        assert_eq!(view.releases.len(), view.shown.len());
        assert_eq!(
            view.works.len(),
            2,
            "the two remaining titles are different works"
        );
    }

    #[test]
    fn pause_and_remove_go_through_the_backend() {
        let mut app = app();
        app.backend.set_paused(1, true);
        app.refresh();
        assert!(app.item(1).expect("torrent 1").torrent.stats.is_paused());

        app.backend.remove(1, false);
        app.refresh();
        assert!(app.item(1).is_none());
        assert_eq!(app.library_len(), 2);
    }

    #[test]
    fn metadata_and_resume_positions_reach_the_ui() {
        let app = app();

        // Torrent 1 is partly watched and has metadata.
        let matrix = app.item(1).expect("torrent 1");
        assert_eq!(matrix.heading(), "The Matrix (1999)");
        assert!(matrix.entry.metadata.is_some());
        assert!(matrix.resume_position().is_some());
        assert!(matrix.entry.poster().is_some());

        // Torrent 3 was watched to the end, so it offers no resume.
        let sintel = app.item(3).expect("torrent 3");
        assert!(sintel.entry.watch.as_ref().unwrap().is_finished());
        assert!(sintel.resume_position().is_none());

        // Torrent 2 has never been opened.
        let bunny = app.item(2).expect("torrent 2");
        assert!(bunny.entry.watch.is_none());
        assert!(bunny.resume_position().is_none());
    }

    #[test]
    fn filtered_library_matches_metadata_titles() {
        let mut app = app();
        app.filter = "matrix".to_string();
        // The filter is applied while rendering; assert the data it selects.
        let matches: Vec<String> = app
            .library
            .iter()
            .filter(|item| item.entry.title().to_lowercase().contains("matrix"))
            .map(|item| item.entry.title())
            .collect();
        assert_eq!(matches, ["The Matrix"]);
    }

    fn app_with_season_pack() -> App {
        let mut items = sample_library();
        items.push(crate::testing::sample_season_pack());
        App::new(Box::new(crate::backend::FakeBackend::new(items)))
    }

    fn fetching(app: &App, torrent_id: usize) -> Vec<usize> {
        app.item(torrent_id)
            .expect("torrent")
            .torrent
            .files
            .iter()
            .filter(|file| file.included)
            .map(|file| file.id)
            .collect()
    }

    #[test]
    fn watching_one_episode_stops_fetching_the_rest_of_the_season() {
        let mut app = app_with_season_pack();

        // Adding with media_only selects every playable file, so a season pack
        // starts out wanting all three.
        assert_eq!(fetching(&app, 9), vec![0, 1, 2]);

        // Sitting down to watch episode 2 should fetch episode 2.
        let ctx = egui::Context::default();
        app.play_file(&ctx, 9, 1);
        // The engine applies the change on its own thread, and the app re-reads
        // its cache on the next frame; this is that frame.
        app.refresh();
        assert_eq!(fetching(&app, 9), vec![1], "only the watched episode");

        // And the choice is reversible.
        app.backend.set_only_files(9, &[0, 1, 2]);
        app.refresh();
        assert_eq!(fetching(&app, 9), vec![0, 1, 2]);
    }

    #[test]
    fn selecting_no_files_is_refused() {
        let mut app = app_with_season_pack();
        app.backend.set_only_files(9, &[]);
        app.refresh();
        assert_eq!(
            fetching(&app, 9),
            vec![0, 1, 2],
            "an empty selection would leave nothing to fetch"
        );
    }

    #[test]
    fn playing_a_paused_pack_resumes_it_and_fetches_only_that_episode() {
        let mut app = app_with_season_pack();
        // This is exactly what adding a season pack produces.
        app.backend.set_paused(9, true);
        app.refresh();
        assert!(app.item(9).expect("pack").torrent.stats.is_paused());

        let ctx = egui::Context::default();
        app.play_file(&ctx, 9, 1);
        app.refresh();

        let item = app.item(9).expect("pack");
        assert!(
            !item.torrent.stats.is_paused(),
            "pressing play has to resume the torrent or the stream never starts"
        );
        assert_eq!(fetching(&app, 9), vec![1], "only the episode being watched");
    }

    #[test]
    fn downloading_one_episode_keeps_only_that_episode() {
        let mut app = app_with_season_pack();

        app.backend.download_media(vec![MediaDownload {
            key: "test#episode".into(),
            torrent_id: 9,
            files: vec![1],
        }]);
        app.refresh();
        let item = app.item(9).expect("the pack");
        assert!(item.downloading);
        assert_eq!(item.kept_files, vec![1], "only the chosen episode is kept");
        let included: Vec<usize> = item
            .torrent
            .files
            .iter()
            .filter(|file| file.included)
            .map(|file| file.id)
            .collect();
        assert_eq!(included, vec![1], "and only it is fetched");

        // Stopping that episode clears the title completely.
        app.backend.stop_media_download(vec![MediaDownload {
            key: "test#episode".into(),
            torrent_id: 9,
            files: vec![1],
        }]);
        app.refresh();
        let item = app.item(9).expect("the pack");
        assert!(!item.downloading);
        assert!(item.kept_files.is_empty());
    }

    #[test]
    fn a_fully_downloaded_file_reads_as_downloaded() {
        let mut item = crate::testing::sample_season_pack();
        item.kept_files = vec![0];

        item.torrent.files[0].progress_bytes = item.torrent.files[0].length;
        let state = download_state(&item, 0);
        assert!(state.kept);
        assert!(state.done, "a fully fetched file is downloaded, not 99%");
        assert!((state.fraction - 1.0).abs() < 0.001);

        item.torrent.files[0].progress_bytes = item.torrent.files[0].length / 4;
        let state = download_state(&item, 0);
        assert!(state.kept && !state.done);
        assert!(state.fraction < 0.3);
    }

    #[test]
    fn resume_positions_are_per_file_not_per_torrent() {
        let mut app = app_with_season_pack();
        let hash = app.item(9).expect("the pack").entry.info_hash.clone();
        // Watch episode 1 (file 0) for a while.
        app.backend
            .record_watch(&hash, 0, None, None, 123.0, Some(1000.0));
        app.refresh();

        let item = app.item(9).expect("the pack");
        assert_eq!(resume_start_at(&item, 0), Some(123.0));
        assert_eq!(
            resume_start_at(&item, 1),
            None,
            "episode 2 must not inherit episode 1's position"
        );
        assert_eq!(resume_start_at(&item, 2), None);
    }

    #[test]
    fn a_film_may_borrow_its_torrent_position() {
        let app = app();
        let matrix = app.item(1).expect("the matrix");
        assert!(!matrix.entry.is_series());
        assert_eq!(
            resume_start_at(&matrix, 0),
            matrix.resume_position(),
            "a film's own record is the only thing its position can mean"
        );
    }

    #[test]
    fn sidecar_subtitles_are_matched_to_their_video() {
        let item = sample_library()
            .into_iter()
            .find(|item| item.torrent.id == 1)
            .expect("the matrix");
        let video = item
            .torrent
            .files
            .iter()
            .find(|f| f.id == 0)
            .expect("video");
        let subs = companion_subtitles(&item.torrent.files, video);
        assert_eq!(subs, vec![2], "The.Matrix.en.srt belongs to the film");
    }

    fn duplicate_film() -> App {
        let mut items = sample_library();
        let mut copy = items
            .iter()
            .find(|item| item.torrent.id == 1)
            .cloned()
            .expect("the matrix");
        copy.torrent.id = 4;
        copy.entry.torrent_id = 4;
        copy.entry.info_hash = "ffffffffffffffffffffffffffffffffffffffff".into();
        copy.torrent.info_hash = copy.entry.info_hash.clone();
        copy.torrent.name = Some("The.Matrix.1999.2160p.BluRay.x265".into());
        copy.entry.release.title = "The Matrix".into();
        copy.entry.release.attributes.resolution = Some("2160p".into());
        // No metadata, so this copy has to be folded into the enriched one.
        copy.entry.metadata = None;
        items.push(copy);
        App::new(Box::new(crate::backend::FakeBackend::new(items)))
    }

    #[test]
    fn copies_of_a_film_become_one_work_ordered_by_quality() {
        let app = duplicate_film();
        assert_eq!(app.works().len(), 3, "the two Matrix copies are one work");

        let work = app
            .works()
            .iter()
            .find(|work| work.title == "The Matrix")
            .expect("the matrix work");
        assert_eq!(work.torrent_count(), 2);
        let versions = work.versions();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].attributes.resolution.as_deref(), Some("2160p"));
        assert_eq!(
            work.lead_torrent_id(),
            4,
            "the best copy is the work's handle"
        );
    }

    #[test]
    fn duplicate_episodes_across_torrents_get_two_variants() {
        let mut items = sample_library();
        items.push(crate::testing::sample_season_pack());

        let mut copy = crate::testing::sample_season_pack();
        copy.torrent.id = 11;
        copy.entry.torrent_id = 11;
        copy.entry.info_hash = "1111111111111111111111111111111111111111".into();
        copy.torrent.info_hash = copy.entry.info_hash.clone();
        copy.entry.release.attributes.resolution = Some("2160p".into());
        items.push(copy);

        let app = App::new(Box::new(crate::backend::FakeBackend::new(items)));
        let work = app
            .works()
            .iter()
            .find(|work| work.title == "Some Show")
            .expect("the show");
        assert_eq!(work.torrent_count(), 2);

        let episodes = work.episodes();
        assert_eq!(episodes.len(), 3, "the same three episodes, not six");
        assert!(episodes.iter().all(|episode| episode.variants.len() == 2));
        // The 2160p copy is preferred on every row.
        assert_eq!(episodes[0].preferred().unwrap().torrent_id, 11);
    }

    #[test]
    fn the_remove_dialog_starts_with_every_source_or_one() {
        let mut app = duplicate_film();
        let work_id = app
            .works()
            .iter()
            .find(|work| work.title == "The Matrix")
            .unwrap()
            .lead_torrent_id();

        app.open_remove_dialog(work_id, None);
        assert_eq!(
            app.remove_selected.len(),
            2,
            "all sources are ticked by default"
        );

        app.open_remove_dialog(work_id, Some(1));
        assert_eq!(app.remove_selected.len(), 1);
        assert!(app.remove_selected.contains(&1));
    }

    #[test]
    fn episode_codes_are_read_from_names() {
        assert_eq!(
            episode_token("Show.S01E02.1080p.mkv").as_deref(),
            Some("s01e02")
        );
        assert_eq!(episode_token("Show.S1E2.srt").as_deref(), Some("s1e2"));
        assert_eq!(episode_token("The.Matrix.1999.mkv"), None);
    }

    #[test]
    fn marking_watched_removes_it_from_continue_watching() {
        let mut app = app();
        app.backend
            .mark_finished("a3f1c0ffee1234567890abcdef1234567890abcd", 0, None);
        app.refresh();

        let rows = app.row_titles();
        assert!(
            !rows.contains(&"Continue watching".to_string()),
            "nothing should be resumable any more: {rows:?}"
        );
        assert!(rows.contains(&"Watched".to_string()));
    }
}

#[cfg(test)]
mod cover_tests {
    use super::*;

    #[test]
    fn a_matching_aspect_is_not_cropped() {
        let uv = cover_uv(16.0 / 9.0, 16.0 / 9.0);
        assert_eq!(
            uv,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
        );
    }

    #[test]
    fn an_image_wider_than_the_hole_is_cropped_left_and_right() {
        // 16:9 source into a square hole: scaling to fill the height overflows
        // the width, so the sides go. Always centred, so both insets match.
        let source: f32 = 16.0 / 9.0;
        let uv = cover_uv(source, 1.0);
        assert!((uv.width() - (1.0 / source)).abs() < 0.001, "{uv:?}");
        assert!((uv.height() - 1.0).abs() < 0.001, "{uv:?}");
        assert!((uv.min.x - (1.0 - uv.max.x)).abs() < 0.001, "{uv:?}");
    }

    #[test]
    fn an_image_narrower_than_the_hole_is_cropped_top_and_bottom() {
        // 16:9 source into a 5:1 banner: the hole is much wider, so filling it
        // overflows vertically and the middle band is what survives.
        let source: f32 = 16.0 / 9.0;
        let uv = cover_uv(source, 5.0);
        assert!((uv.width() - 1.0).abs() < 0.001, "{uv:?}");
        assert!((uv.height() - (source / 5.0)).abs() < 0.001, "{uv:?}");
        assert!((uv.min.y - (1.0 - uv.max.y)).abs() < 0.001, "{uv:?}");
    }

    #[test]
    fn posters_in_poster_shaped_cards_are_untouched() {
        // The card is built to 2:3, so a real poster needs no crop at all.
        let uv = cover_uv(2.0 / 3.0, 168.0 / 252.0);
        assert!((uv.width() - 1.0).abs() < 0.001, "{uv:?}");
        assert!((uv.height() - 1.0).abs() < 0.001, "{uv:?}");
    }

    #[test]
    fn degenerate_aspects_fall_back_to_the_whole_image() {
        let full = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        for (source, target) in [
            (0.0, 1.0),
            (1.0, 0.0),
            (f32::NAN, 1.0),
            (1.0, f32::INFINITY),
        ] {
            assert_eq!(cover_uv(source, target), full);
        }
    }
}
