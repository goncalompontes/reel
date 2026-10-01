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
    ArtworkKind, ArtworkRef, CatalogSettings, RowKind, SearchHit, Work, WorkEpisode, WorkMember,
    build_work_rows, build_works, separate_works,
};
use reel_core::fmt;

use crate::backend::{Backend, BackendEvent, LibraryItem, PlayerCapability};
use crate::player::{PlaybackInfo, PlaybackStats, PlayerController};
use crate::theme;

/// Which page is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Library,
    Detail(usize),
    Player,
    Add,
    Search,
    Settings,
}

/// A catalog row, resolved to torrent ids so rendering never rebuilds it.
#[derive(Debug, Clone)]
pub struct RowLayout {
    pub kind: RowKind,
    pub title: String,
    pub ids: Vec<usize>,
}

pub struct App {
    backend: Box<dyn Backend>,
    screen: Screen,
    /// Cached library, refreshed periodically while browsing.
    library: Vec<LibraryItem>,
    /// Torrents grouped into films and series. Rebuilt with the library.
    works: Vec<Work>,
    /// Row layout, resolved to work (lead torrent) ids.
    rows: Vec<RowLayout>,
    /// Which copy of an episode the user picked, keyed by work and episode.
    episode_choice: HashMap<String, (usize, usize)>,
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
    /// The API key being typed on the settings page. Never filled in from
    /// storage: a credential is written, not displayed.
    api_key_input: String,
    /// Editable copy of the canonical settings while the Settings page is open.
    settings_draft: Option<CatalogSettings>,
    player: PlayerController,
    /// Last time the mouse or a key moved, for hiding the chrome in fullscreen.
    player_last_activity: Instant,
    player_controls_hidden: bool,
    /// Torrent id the player was started from, to return to.
    player_origin: Option<usize>,
    /// A play request waiting for its temporary torrent to be brought back.
    pending_play: Option<(usize, usize)>,
    /// Frames seen last frame and when they last advanced, to detect a stall
    /// (a frozen picture, not only "no frame at all").
    player_last_frames: u64,
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
/// Record watch progress at most every this many seconds of playback.
const WATCH_RECORD_INTERVAL: f64 = 5.0;

impl App {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        let mut app = Self {
            backend,
            screen: Screen::Library,
            library: Vec::new(),
            works: Vec::new(),
            rows: Vec::new(),
            episode_choice: HashMap::new(),
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
            api_key_input: String::new(),
            settings_draft: None,
            player: PlayerController::new(),
            player_last_activity: Instant::now(),
            player_controls_hidden: false,
            player_origin: None,
            pending_play: None,
            player_last_frames: 0,
            player_progress_at: Instant::now(),
            stream_recovered: false,
            pending_ctx: None,
            watch_last_recorded: 0.0,
            watch_recording_for: None,
        };
        // Saved settings are canonical: apply playback preferences immediately.
        let settings = app.backend.settings();
        app.player.set_volume(settings.default_volume);
        app.player.set_preferences(crate::player::PlaybackPreferences {
            subtitles_enabled: settings.subtitles_enabled,
            subtitle_language: settings.subtitle_language.clone(),
            volume: settings.default_volume,
        });
        app.refresh();
        app
    }

    /// The screen currently shown. Exposed for tests.
    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// Number of entries in the cached library. Exposed for tests.
    pub fn library_len(&self) -> usize {
        self.library.len()
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

    pub fn navigate(&mut self, screen: Screen) {
        self.screen = screen;
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
        let query = self.search_query.trim().to_string();
        if query.is_empty() {
            self.warn("Type something to search for");
            return;
        }
        self.searching = true;
        self.search_note = None;
        self.search_for = Some(query.clone());
        self.backend.search(&query);
    }

    pub fn item(&self, id: usize) -> Option<LibraryItem> {
        self.library.iter().find(|item| item.torrent.id == id).cloned()
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
    pub fn preferred_episode(&self, work: &Work) -> Option<(usize, usize)> {
        let episodes = work.episodes();
        for episode in &episodes {
            for variant in &episode.variants {
                let resumable = self
                    .item(variant.torrent_id)
                    .and_then(|item| item.entry.watch_for_file(variant.file_id).cloned())
                    .is_some_and(|progress| progress.is_resumable());
                if resumable {
                    return Some((variant.torrent_id, variant.file_id));
                }
            }
        }
        episodes
            .first()
            .and_then(WorkEpisode::preferred)
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
        let per_file = item
            .entry
            .watch_by_file
            .get(&file_id)
            .filter(|progress| progress.is_resumable())
            .map(|progress| progress.position);
        let start_at = per_file.or_else(|| {
            let playable = item.torrent.files.iter().filter(|f| f.included).count();
            (playable <= 1).then(|| item.resume_position()).flatten()
        });

        let companions = companion_subtitles(&item.torrent.files, file);
        let subtitles: Vec<(String, String)> = companions
            .iter()
            .filter_map(|id| item.torrent.files.iter().find(|f| f.id == *id))
            .map(|sub| {
                let url = sub
                    .stream
                    .url
                    .clone()
                    .unwrap_or_else(|| format!("{}{}", self.backend.base_url(), sub.stream.path));
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
        self.player_progress_at = Instant::now();
        self.watch_last_recorded = start_at.unwrap_or(0.0);
        self.watch_recording_for = Some(item.entry.info_hash.clone());

        tracing::info!(%url, start_at = ?start_at, live = file.included, "opening the player");
        let ctx = self
            .pending_ctx
            .clone()
            .unwrap_or_else(egui::Context::default);
        match self.player.open(&ctx, &capability, &url, info) {
            Ok(()) => {
                self.screen = Screen::Player;
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

    fn maybe_refresh(&mut self) {
        // The player needs fresh engine stats for its overlay, and the library
        // needs new metadata as it arrives, so both refresh on one cadence.
        if self.screen != Screen::Settings && self.last_refresh.elapsed() >= REFRESH_INTERVAL {
            self.refresh();
        }
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
                    self.screen = Screen::Detail(work_id);
                }
                BackendEvent::Metadata { .. } | BackendEvent::WatchUpdated => {
                    self.refresh();
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
                BackendEvent::ApiKeyChecked { ok, message } => {
                    self.set_toast(message, !ok);
                }
                BackendEvent::SearchResults { query, results } => {
                    // A late answer for an older query must not replace what the
                    // user is looking at now.
                    if self.search_for.as_deref() == Some(query.as_str()) {
                        self.searching = false;
                        self.search_note = describe_search(&results);
                        self.search_hits = results.hits;
                    }
                }
                BackendEvent::SearchFailed { query, message } => {
                    if self.search_for.as_deref() == Some(query.as_str()) {
                        self.searching = false;
                        self.search_hits.clear();
                        self.search_note = Some(message);
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

    fn warn(&mut self, message: impl Into<String>) {
        self.set_toast(message, true);
    }

    fn selected_work(&self) -> Option<Work> {
        match self.screen {
            Screen::Detail(id) => self.work(id),
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

    /// The episodes of a season as `(torrent, files)` batches, using the chosen
    /// copy of each episode (or the best one).
    fn season_selection(&self, work: &Work, season: Option<u32>) -> Vec<(usize, Vec<usize>)> {
        let mut batches: Vec<(usize, Vec<usize>)> = Vec::new();
        for group in work.seasons() {
            if group.number != season {
                continue;
            }
            for episode in &group.episodes {
                let variant = self.chosen_variant(work, episode);
                let Some(variant) = variant else { continue };
                match batches.iter_mut().find(|(id, _)| *id == variant.torrent_id) {
                    Some((_, files)) => files.push(variant.file_id),
                    None => batches.push((variant.torrent_id, vec![variant.file_id])),
                }
            }
        }
        batches
    }

    /// The copy of an episode the user picked, else the best one.
    fn chosen_variant<'a>(
        &'a self,
        work: &'a Work,
        episode: &'a WorkEpisode,
    ) -> Option<&'a reel_catalog::EpisodeVariant> {
        let key = episode_choice_key(work, episode);
        if let Some((torrent_id, file_id)) = self.episode_choice.get(&key) {
            if let Some(variant) = episode
                .variants
                .iter()
                .find(|variant| variant.torrent_id == *torrent_id && variant.file_id == *file_id)
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

        // Players get a bare canvas: the video and its controls only.
        if self.screen == Screen::Player {
            self.player_screen(ui, &ctx);
            self.draw_toast(ui, &ctx);
            return;
        }

        egui::Panel::top("top-bar")
            .frame(
                egui::Frame::NONE
                    .fill(theme::SURFACE)
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(ui, |ui| self.top_bar(ui));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::symmetric(20, 14)),
            )
            .show(ui, |ui| match self.screen.clone() {
                Screen::Library => self.library_screen(ui),
                Screen::Detail(_) => self.detail_screen(ui),
                Screen::Add => self.add_screen(ui),
                Screen::Search => self.search_screen(ui),
                Screen::Settings => self.settings_screen(ui),
                Screen::Player => unreachable!("handled above"),
            });

        self.draw_toast(ui, &ctx);
        self.draw_remove_dialog(ui, &ctx);
    }

    fn on_exit(&mut self) {
        self.player.close();
    }
}

// ------------------------------------------------------------------ chrome

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("reel")
                    .size(22.0)
                    .strong()
                    .color(theme::ACCENT),
            );
            ui.add_space(6.0);

            let tab = |ui: &mut egui::Ui, current: &Screen, target: Screen, label: &str| {
                if ui.selectable_label(current == &target, label).clicked() {
                    Some(target)
                } else {
                    None
                }
            };

            let mut next = None;
            next = next.or(tab(ui, &self.screen, Screen::Library, "  Library  "));
            next = next.or(tab(ui, &self.screen, Screen::Search, "  Search  "));
            next = next.or(tab(ui, &self.screen, Screen::Add, "  Add  "));
            next = next.or(tab(ui, &self.screen, Screen::Settings, "  Settings  "));
            if let Some(target) = next {
                self.screen = target;
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.screen == Screen::Library {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text("filter library\u{2026}")
                            .desired_width(200.0),
                    );
                }

                let (down, peers): (u64, u32) = self.library.iter().fold((0, 0), |(d, p), item| {
                    (
                        d + item.torrent.stats.download_bps,
                        p + item.torrent.stats.peers.live,
                    )
                });
                if down > 0 {
                    ui.label(
                        egui::RichText::new(format!("{} \u{2b07}", fmt::human_rate(down)))
                            .color(theme::TEXT_DIM),
                    );
                }
                ui.label(egui::RichText::new(format!("{peers} peers")).color(theme::TEXT_DIM));
            });
        });
    }

    fn draw_toast(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let (message, color) = {
            let Some(toast) = self.toast.as_ref() else {
                return;
            };
            if toast.shown.elapsed() > TOAST_LIFETIME {
                self.toast = None;
                return;
            }
            let color = if toast.error { theme::DANGER } else { theme::OK };
            (toast.message.clone(), color)
        };

        // Keep animating so the toast expires even without user input.
        ctx.request_repaint_after(Duration::from_millis(200));

        let mut dismiss = false;
        egui::Panel::bottom("toast")
            .frame(
                egui::Frame::NONE
                    .fill(theme::SURFACE_RAISED)
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .stroke(egui::Stroke::new(1.0, color))
                    .corner_radius(CornerRadius::same(8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(color, "\u{2022}");
                    ui.label(message);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("dismiss").clicked() {
                            dismiss = true;
                        }
                    });
                });
            });

        if dismiss {
            self.toast = None;
        }
    }

    /// Open the remove dialog for a work. `only` pre-ticks a single source;
    /// otherwise every source is ticked, so removing a merged title removes all
    /// of it unless the user says otherwise.
    pub fn open_remove_dialog(&mut self, work_id: usize, only: Option<usize>) {
        let Some(work) = self.work(work_id) else {
            return;
        };
        self.remove_selected = match only {
            Some(torrent_id) => HashSet::from([torrent_id]),
            None => work.members.iter().map(|member| member.torrent_id()).collect(),
        };
        self.remove_files = false;
        self.show_remove = Some(work_id);
    }

    fn draw_remove_dialog(&mut self, _ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(work_id) = self.show_remove else {
            return;
        };
        // Navigating away dismisses the dialog; it belongs to one detail page.
        if self.screen != Screen::Detail(work_id) {
            self.show_remove = None;
            return;
        }
        let Some(work) = self.work(work_id) else {
            self.show_remove = None;
            return;
        };

        let mut close = false;
        let mut confirmed = false;
        let mut paused_change: Option<(usize, bool)> = None;

        egui::Window::new("Remove torrents")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(format!("Which copies of {}?", work.heading())).strong(),
                );
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "Untick a source to keep it. Removing forgets the torrent and any saved \
                         position; deleting also removes downloaded files from disk.",
                    )
                    .color(theme::TEXT_DIM)
                    .size(11.5),
                );
                ui.add_space(8.0);

                for member in &work.members {
                    let torrent_id = member.torrent_id();
                    let mut selected = self.remove_selected.contains(&torrent_id);
                    let label = format!(
                        "{}  \u{2022}  {}",
                        reel_core::title::truncate(&member.entry.heading(), 48),
                        member.attributes().quality_label()
                    );
                    if ui.checkbox(&mut selected, label).changed() {
                        if selected {
                            self.remove_selected.insert(torrent_id);
                        } else {
                            self.remove_selected.remove(&torrent_id);
                        }
                    }
                }

                ui.add_space(6.0);
                ui.checkbox(&mut self.remove_files, "Also delete downloaded files");

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    let count = self.remove_selected.len();
                    let remove_label = if count == 0 {
                        "Remove selected".to_string()
                    } else {
                        format!("Remove {count} torrent{}", if count == 1 { "" } else { "s" })
                    };
                    if ui
                        .add_enabled(
                            count > 0,
                            egui::Button::new(
                                egui::RichText::new(remove_label).color(theme::DANGER),
                            ),
                        )
                        .clicked()
                    {
                        confirmed = true;
                    }
                });

                // Pausing a source is the other thing one wants from here.
                for member in &work.members {
                    let torrent_id = member.torrent_id();
                    if let Some(item) = self.item(torrent_id) {
                        let paused = item.torrent.stats.is_paused();
                        let label = if paused {
                            "Resume download"
                        } else {
                            "Pause download"
                        };
                        if ui
                            .small_button(format!("{label} \u{2014} {}", item.heading()))
                            .clicked()
                        {
                            paused_change = Some((torrent_id, !paused));
                        }
                    }
                }
            });

        if let Some((torrent_id, paused)) = paused_change {
            self.backend.set_paused(torrent_id, paused);
            self.invalidate();
        }

        if confirmed {
            let ids: Vec<usize> = self.remove_selected.iter().copied().collect();
            let count = ids.len();
            for torrent_id in ids {
                self.backend.remove(torrent_id, self.remove_files);
            }
            self.set_toast(
                format!("Removed {count} torrent{}", if count == 1 { "" } else { "s" }),
                false,
            );
            self.refresh();
            // The work may have shrunk or vanished entirely.
            if self.work(work_id).is_none() {
                self.screen = Screen::Library;
            }
            close = true;
        }
        if close {
            self.show_remove = None;
            self.remove_selected.clear();
        }
    }
}

// ---------------------------------------------------------------- artwork

impl App {
    /// Paint a poster or backdrop, falling back to generated artwork.
    ///
    /// The fallback is deliberate, not a placeholder: without an API key there
    /// are no real posters, and a stable colour per title keeps the grid
    /// readable and obviously intentional.
    fn paint_art(
        ui: &egui::Ui,
        rect: Rect,
        reference: Option<&ArtworkRef>,
        seed: &str,
        kind: ArtworkKind,
        radius: CornerRadius,
    ) {
        if let Some(uri) = reference.and_then(|artwork| artwork.local_uri()) {
            ui.painter().rect_filled(rect, radius, theme::SURFACE_RAISED);
            // Crop rather than squash. Providers publish posters at 2:3 and
            // backdrops at 16:9, and a hero banner is far wider than either, so
            // filling the rect directly would visibly distort the image.
            let uv = cover_uv(source_aspect(kind), rect.width() / rect.height().max(1.0));
            egui::Image::new(uri)
                .uv(uv)
                .corner_radius(radius)
                .paint_at(ui, rect);
            return;
        }

        let base = match kind {
            ArtworkKind::Poster => theme::poster_color(seed),
            ArtworkKind::Backdrop => theme::poster_shade(seed),
        };
        let painter = ui.painter();
        painter.rect_filled(rect, radius, base);
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            reel_core::title::initials(&seed),
            egui::FontId::proportional(rect.height() * 0.22),
            Color32::from_white_alpha(38),
        );
    }

    /// A poster tile: artwork, title, versions and a resume bar.
    fn card(&mut self, ui: &mut egui::Ui, work: &Work, width: f32, height: f32) -> CardHit {
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
        let painter = ui.painter().clone();

        Self::paint_art(
            ui,
            rect,
            work.poster(),
            &work.title,
            ArtworkKind::Poster,
            theme::CARD_RADIUS,
        );

        // A scrim behind the text so it stays readable over bright artwork.
        let mut scrim = rect;
        scrim.set_top(rect.bottom() - 62.0);
        painter.rect_filled(scrim, theme::CARD_RADIUS, Color32::from_black_alpha(190));

        ui.put(
            Rect::from_min_size(
                egui::pos2(rect.left() + 8.0, rect.bottom() - 56.0),
                Vec2::new(rect.width() - 16.0, 20.0),
            ),
            egui::Label::new(
                egui::RichText::new(reel_core::title::truncate(&work.title, 22))
                    .size(13.5)
                    .color(Color32::WHITE),
            )
            .selectable(false),
        );

        // Second line: a resume hint, or what the title actually contains.
        let subtitle = if let Some(position) = work.resume_position() {
            format!("\u{25b6} resume from {}", fmt::human_duration(position))
        } else if work.is_series() {
            let episodes = work.episodes();
            let choices = episodes
                .iter()
                .filter(|episode| episode.variants.len() > 1)
                .count();
            if choices > 0 {
                format!("{} episodes \u{00b7} {choices} with choices", episodes.len())
            } else {
                plural(episodes.len(), "episode")
            }
        } else {
            match work.versions().first() {
                Some(version) => {
                    let count = work.versions().len();
                    if count > 1 {
                        format!("{} \u{00b7} {count} versions", version.attributes.quality_label())
                    } else {
                        version.attributes.quality_label()
                    }
                }
                None => work
                    .year
                    .map(|year| year.to_string())
                    .unwrap_or_else(|| reel_core::title::truncate(&work.key, 10)),
            }
        };
        ui.put(
            Rect::from_min_size(
                egui::pos2(rect.left() + 8.0, rect.bottom() - 36.0),
                Vec2::new(rect.width() - 16.0, 16.0),
            ),
            egui::Label::new(
                egui::RichText::new(subtitle)
                    .size(10.5)
                    .color(if work.resume_position().is_some() {
                        theme::ACCENT
                    } else {
                        theme::TEXT_DIM
                    }),
            )
            .selectable(false),
        );

        // Download progress along the bottom edge, from the best member.
        let lead = self.lead_item(work);
        if let Some(item) = lead.as_ref() {
            if item.torrent.stats.percent < 99.5 {
                let bar = Rect::from_min_size(
                    egui::pos2(rect.left(), rect.bottom() - 4.0),
                    Vec2::new(rect.width(), 4.0),
                );
                let mut filled = bar;
                filled.set_right(
                    bar.left() + bar.width() * (item.torrent.stats.percent as f32 / 100.0),
                );
                painter.rect_filled(bar, CornerRadius::ZERO, Color32::from_black_alpha(150));
                painter.rect_filled(filled, CornerRadius::ZERO, theme::OK);
            }

            // A count badge when the library holds more than one copy.
            if work.torrent_count() > 1 {
                painter.circle_filled(
                    egui::pos2(rect.right() - 14.0, rect.top() + 14.0),
                    8.0,
                    Color32::from_black_alpha(180),
                );
                painter.text(
                    egui::pos2(rect.right() - 14.0, rect.top() + 14.0),
                    egui::Align2::CENTER_CENTER,
                    work.torrent_count().to_string(),
                    egui::FontId::proportional(11.0),
                    Color32::WHITE,
                );
            }

            let dot_x = if work.torrent_count() > 1 {
                rect.right() - 30.0
            } else {
                rect.right() - 14.0
            };
            painter.circle_filled(
                egui::pos2(dot_x, rect.top() + 14.0),
                5.0,
                theme::state_color(&item.torrent.stats.state),
            );
        }

        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            painter.rect_stroke(
                rect,
                theme::CARD_RADIUS,
                egui::Stroke::new(1.5, theme::ACCENT),
                egui::StrokeKind::Inside,
            );
        }

        CardHit {
            clicked: response.clicked(),
            double_clicked: response.double_clicked(),
        }
    }
}

struct CardHit {
    clicked: bool,
    double_clicked: bool,
}

// ----------------------------------------------------------------- library

impl App {
    fn library_screen(&mut self, ui: &mut egui::Ui) {
        let filter = self.filter.trim().to_lowercase();

        if self.works.is_empty() {
            self.empty_library(ui);
            return;
        }

        // When filtering, show one flat grid: rows would only fragment the
        // matches.
        if !filter.is_empty() {
            let matches: Vec<Work> = self
                .works
                .iter()
                .filter(|work| {
                    format!("{} {}", work.title, work.heading())
                        .to_lowercase()
                        .contains(&filter)
                })
                .cloned()
                .collect();

            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "{} match{} for \"{}\"",
                        matches.len(),
                        if matches.len() == 1 { "" } else { "es" },
                        self.filter.trim()
                    ))
                    .size(16.0)
                    .strong(),
                );
                ui.add_space(10.0);
                if matches.is_empty() {
                    ui.label(egui::RichText::new("Nothing here matches.").color(theme::TEXT_DIM));
                } else {
                    self.card_grid(ui, &matches);
                }
            });
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            if let Some(featured) = self.works.first().cloned() {
                self.hero(ui, &featured);
                ui.add_space(16.0);
            }

            for row in self.rows.clone() {
                let works: Vec<Work> = row.ids.iter().filter_map(|id| self.work(*id)).collect();
                if works.is_empty() {
                    continue;
                }

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&row.title).size(16.0).strong());
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(plural(works.len(), "title"))
                            .size(11.0)
                            .color(theme::TEXT_DIM),
                    );
                });
                ui.add_space(6.0);
                self.row_strip(ui, &works, row.kind);
                ui.add_space(20.0);
            }
        });
    }

    /// A horizontally scrolling strip of posters.
    fn row_strip(&mut self, ui: &mut egui::Ui, works: &[Work], kind: RowKind) {
        // Resume cards are taller so the "resume from" line has room.
        let (width, height) = match kind {
            RowKind::ContinueWatching => (240.0, 135.0),
            _ => (168.0, 252.0),
        };

        let mut open: Option<usize> = None;
        let mut play: Option<usize> = None;

        egui::ScrollArea::horizontal()
            .id_salt(format!("row-{kind:?}"))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for work in works {
                        let hit = self.card(ui, work, width, height);
                        let id = work.lead_torrent_id();
                        if hit.double_clicked {
                            play = Some(id);
                        } else if hit.clicked {
                            open = Some(id);
                        }
                        ui.add_space(10.0);
                    }
                });
            });

        if let Some(id) = play {
            self.play_with_ctx(id);
        } else if let Some(id) = open {
            self.screen = Screen::Detail(id);
        }
    }

    /// A wrapped grid, used for filtered results and small libraries.
    fn card_grid(&mut self, ui: &mut egui::Ui, works: &[Work]) {
        const CARD_W: f32 = 168.0;
        // 2:3, the shape of a real poster, so artwork is not stretched.
        const CARD_H: f32 = 252.0;
        const GAP: f32 = 12.0;

        let available = ui.available_width();
        let per_row = (((available + GAP) / (CARD_W + GAP)).floor() as usize).max(1);
        let mut open: Option<usize> = None;
        let mut play: Option<usize> = None;

        for chunk in works.chunks(per_row) {
            ui.horizontal(|ui| {
                for work in chunk {
                    let hit = self.card(ui, work, CARD_W, CARD_H);
                    let id = work.lead_torrent_id();
                    if hit.double_clicked {
                        play = Some(id);
                    } else if hit.clicked {
                        open = Some(id);
                    }
                    ui.add_space(GAP);
                }
            });
            ui.add_space(GAP);
        }

        if let Some(id) = play {
            self.play_with_ctx(id);
        } else if let Some(id) = open {
            self.screen = Screen::Detail(id);
        }
    }

    fn empty_library(&mut self, ui: &mut egui::Ui) {
        ui.add_space(60.0);
        ui.vertical_centered(|ui| {
            ui.label(egui::RichText::new("Your library is empty").size(24.0).strong());
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(
                    "Add a magnet link or a .torrent URL and reel will start fetching the \
                     video, streaming it while it downloads.",
                )
                .color(theme::TEXT_DIM),
            );
            ui.add_space(20.0);
            if ui.button("  Add a torrent  ").clicked() {
                self.screen = Screen::Add;
            }
        });
    }

    /// The banner at the top of the library: newest entry, blown up.
    fn hero(&mut self, ui: &mut egui::Ui, work: &Work) {
        let height = 240.0;
        let width = ui.available_width();
        let (rect, hero_response) =
            ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
        let painter = ui.painter().clone();

        Self::paint_art(
            ui,
            rect,
            work.backdrop(),
            &work.title,
            ArtworkKind::Backdrop,
            theme::PANEL_RADIUS,
        );

        // Darken the lower half so the text reads over any artwork.
        let mut scrim = rect;
        scrim.set_top(rect.bottom() - 120.0);
        painter.rect_filled(scrim, theme::PANEL_RADIUS, Color32::from_black_alpha(180));

        ui.put(
            Rect::from_min_size(
                egui::pos2(rect.left() + 24.0, rect.top() + 22.0),
                Vec2::new(160.0, 18.0),
            ),
            egui::Label::new(
                egui::RichText::new("LATEST")
                    .size(11.0)
                    .strong()
                    .color(theme::ACCENT),
            )
            .halign(egui::Align::LEFT)
            .selectable(false),
        );

        ui.put(
            Rect::from_min_size(
                egui::pos2(rect.left() + 24.0, rect.bottom() - 84.0),
                Vec2::new(rect.width() - 300.0, 40.0),
            ),
            egui::Label::new(
                egui::RichText::new(reel_core::title::truncate(&work.heading(), 52))
                    .size(30.0)
                    .color(Color32::WHITE),
            )
            .halign(egui::Align::LEFT)
            .selectable(false),
        );

        // Metadata line: genres and rating when we have them, otherwise the
        // transfer state of the best copy.
        let lead = self.lead_item(work);
        let meta = match work.metadata.as_ref() {
            Some(metadata) => {
                let mut parts = Vec::new();
                if let Some(year) = metadata.year {
                    parts.push(year.to_string());
                }
                if let Some(runtime) = metadata.runtime_label() {
                    parts.push(runtime);
                }
                if let Some(rating) = metadata.rating {
                    parts.push(format!("\u{2605} {rating:.1}/10"));
                }
                if !metadata.genres.is_empty() {
                    parts.push(metadata.genres.join(" \u{2022} "));
                }
                if work.torrent_count() > 1 {
                    parts.push(format!("{} copies", work.torrent_count()));
                }
                parts.join("   \u{2022}   ")
            }
            None => match lead.as_ref() {
                Some(item) => format!(
                    "{}   \u{2022}   {} peers   \u{2022}   {:.0}% downloaded",
                    item.torrent.stats.state,
                    item.torrent.stats.peers.live,
                    item.torrent.stats.percent
                ),
                None => String::new(),
            },
        };
        ui.put(
            Rect::from_min_size(
                egui::pos2(rect.left() + 24.0, rect.bottom() - 44.0),
                Vec2::new(rect.width() - 300.0, 18.0),
            ),
            egui::Label::new(egui::RichText::new(meta).size(12.0).color(theme::TEXT_DIM))
                .halign(egui::Align::LEFT)
                .selectable(false),
        );

        let label = match work.resume_position() {
            Some(position) => format!("\u{25b6}  Resume {}", fmt::human_duration(position)),
            None => "\u{25b6}  Play".to_string(),
        };
        let play = ui
            .put(
                Rect::from_min_size(
                    egui::pos2(rect.right() - 224.0, rect.bottom() - 74.0),
                    Vec2::new(190.0, 38.0),
                ),
                egui::Button::new(egui::RichText::new(label).size(15.0)),
            )
            .clicked();

        let id = work.lead_torrent_id();
        if play {
            self.play_with_ctx(id);
        } else if hero_response.clicked() {
            self.screen = Screen::Detail(id);
        }
    }

    /// Play a work, using the context saved at the start of the frame.
    fn play_with_ctx(&mut self, work_id: usize) {
        let Some(ctx) = self.pending_ctx.clone() else {
            self.warn("Playback is not ready yet");
            return;
        };
        self.play_work(&ctx, work_id);
    }
}

// ------------------------------------------------------------------ detail

impl App {
    fn detail_screen(&mut self, ui: &mut egui::Ui) {
        let Some(work) = self.selected_work() else {
            ui.label("That title is no longer in the library.");
            if ui.button("Back to library").clicked() {
                self.screen = Screen::Library;
            }
            return;
        };

        let mut action: Option<DetailAction> = None;
        let work_id = work.lead_torrent_id();
        let lead = self.lead_item(&work);

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("\u{2b05}  Library").clicked() {
                    action = Some(DetailAction::Navigate(Screen::Library));
                }
                let copies = work.torrent_count();
                ui.label(
                    egui::RichText::new(if copies > 1 {
                        format!("{copies} copies")
                    } else {
                        "1 copy".to_string()
                    })
                    .color(theme::TEXT_DIM),
                );
            });

            ui.add_space(10.0);

            // Backdrop header, with the poster overlapping it.
            let header_height = 200.0;
            let (header, _) = ui.allocate_exact_size(
                Vec2::new(ui.available_width(), header_height),
                Sense::hover(),
            );
            let painter = ui.painter().clone();
            Self::paint_art(
                ui,
                header,
                work.backdrop(),
                &work.title,
                ArtworkKind::Backdrop,
                theme::PANEL_RADIUS,
            );
            let mut scrim = header;
            scrim.set_left(header.left() + header.width() * 0.4);
            painter.rect_filled(scrim, theme::PANEL_RADIUS, Color32::from_black_alpha(150));

            ui.add_space(-header_height + 24.0);
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                let poster_size = Vec2::new(160.0, 230.0);
                let (poster, _) = ui.allocate_exact_size(poster_size, Sense::hover());
                Self::paint_art(
                    ui,
                    poster,
                    work.poster(),
                    &work.title,
                    ArtworkKind::Poster,
                    theme::CARD_RADIUS,
                );

                ui.add_space(20.0);
                ui.vertical(|ui| {
                    ui.add_space(40.0);
                    ui.label(
                        egui::RichText::new(work.heading())
                            .size(28.0)
                            .strong()
                            .color(Color32::WHITE),
                    );

                    if let Some(metadata) = work.metadata.as_ref() {
                        if let Some(tagline) = metadata.tagline.as_deref() {
                            ui.label(
                                egui::RichText::new(tagline)
                                    .size(13.0)
                                    .italics()
                                    .color(theme::TEXT_DIM),
                            );
                        }

                        let mut facts = Vec::new();
                        if let Some(rating) = metadata.rating {
                            facts.push(format!("\u{2605} {rating:.1}/10"));
                        }
                        if let Some(votes) = metadata.vote_count {
                            facts.push(format!("{votes} votes"));
                        }
                        if let Some(runtime) = metadata.runtime_label() {
                            facts.push(runtime);
                        }
                        if work.is_series() {
                            facts.push(plural(work.episodes().len(), "episode"));
                        } else {
                            facts.push(plural(work.versions().len(), "version"));
                        }
                        if !facts.is_empty() {
                            ui.label(
                                egui::RichText::new(facts.join("   \u{2022}   "))
                                    .size(12.0)
                                    .color(theme::TEXT_DIM),
                            );
                        }

                        if !metadata.genres.is_empty() {
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new(metadata.genres.join("  \u{2022}  "))
                                    .size(12.0)
                                    .color(theme::ACCENT),
                            );
                        }
                    } else if let Some(item) = lead.as_ref() {
                        ui.label(
                            egui::RichText::new(format!(
                                "{}  \u{2022}  {}  \u{2022}  {}",
                                item.torrent.stats.state,
                                fmt::human_bytes(item.torrent.stats.total_bytes),
                                reel_core::title::truncate(item.info_hash(), 16)
                            ))
                            .color(theme::TEXT_DIM),
                        );
                    }
                });
            });

            ui.add_space(16.0);

            let resumable = work.resume_position();
            let has_playable = if work.is_series() {
                !work.episodes().is_empty()
            } else {
                !work.versions().is_empty()
            };

            ui.horizontal(|ui| {
                let label = match resumable {
                    Some(position) => format!("\u{25b6}  Resume {}", fmt::human_duration(position)),
                    None => "\u{25b6}  Play".to_string(),
                };
                if ui
                    .add_enabled(
                        has_playable,
                        egui::Button::new(egui::RichText::new(label).size(15.0)),
                    )
                    .clicked()
                {
                    action = Some(DetailAction::PlayWork);
                }

                if resumable.is_some() && ui.button("Start over").clicked() {
                    action = Some(DetailAction::PlayWorkFromStart);
                }

                if ui.button("Mark watched").clicked() {
                    action = Some(DetailAction::MarkWatched);
                }

                if ui.button("\u{1f5d1}  Remove").clicked() {
                    action = Some(DetailAction::OpenRemove);
                }

                if let Some(item) = lead.as_ref() {
                    if let Some(file) = item.torrent.primary_file() {
                        let url = file
                            .stream
                            .url
                            .clone()
                            .unwrap_or_else(|| file.stream.path.clone());
                        if ui
                            .button("\u{1f4cb}  Copy stream URL")
                            .on_hover_text(url.clone())
                            .clicked()
                        {
                            ui.ctx().copy_text(url);
                        }
                    }
                }
            });

            ui.add_space(14.0);

            if let Some(overview) = work.metadata.as_ref().and_then(|m| m.overview.as_deref()) {
                ui.label(egui::RichText::new(overview).size(13.5).color(theme::TEXT));
                ui.add_space(12.0);
            }

            if let Some(item) = lead.as_ref() {
                // No title-level progress bar: a title can hold several
                // downloads at once, so progress lives on each row. (There is no
                // "up" stat either — reel never seeds.)
                ui.horizontal(|ui| {
                    stat(ui, "downloaded", &fmt::human_bytes(item.torrent.stats.progress_bytes));
                    stat(ui, "down", &fmt::human_rate(item.torrent.stats.download_bps));
                    stat(ui, "peers", &item.torrent.stats.peers.live.to_string());
                    stat(ui, "eta", &fmt::human_eta(item.torrent.stats.eta_seconds));
                });

                if let Some(error) = item.torrent.stats.error.as_deref() {
                    ui.add_space(6.0);
                    ui.colored_label(theme::DANGER, error);
                }
            }

            ui.add_space(16.0);
            ui.separator();
            ui.add_space(10.0);

            // A series is episodes; a film is a list of copies.
            if work.is_series() {
                self.work_episode_list(ui, &work, &mut action);
            } else {
                self.work_version_list(ui, &work, &mut action);
            }

            self.work_sources(ui, &work, &mut action);
            self.storage_line(ui, &work);
        });

        match action {
            Some(DetailAction::Navigate(screen)) => self.screen = screen,
            Some(DetailAction::OpenRemove) => self.open_remove_dialog(work_id, None),
            Some(DetailAction::RemoveSource(torrent_id)) => {
                self.open_remove_dialog(work_id, Some(torrent_id))
            }
            Some(DetailAction::SetPaused(torrent_id, paused)) => {
                self.backend.set_paused(torrent_id, paused)
            }
            Some(DetailAction::PlayWork) => {
                if let Some(ctx) = self.pending_ctx.clone() {
                    self.play_work(&ctx, work_id);
                }
            }
            Some(DetailAction::PlayWorkFromStart) => self.restart_work(work_id),
            Some(DetailAction::MarkWatched) => self.mark_work_watched(&work),
            Some(DetailAction::Play(torrent_id, file_id)) => {
                if let Some(ctx) = self.pending_ctx.clone() {
                    self.play_file(&ctx, torrent_id, file_id);
                }
            }
            Some(DetailAction::Download(torrent_id, file_id)) => {
                self.backend.download_files(torrent_id, &[file_id]);
                self.invalidate();
            }
            Some(DetailAction::CancelDownload(torrent_id, file_id)) => {
                self.backend.stop_download_files(torrent_id, &[file_id]);
                self.invalidate();
            }
            Some(DetailAction::DownloadEpisodes(selections)) => {
                for (torrent_id, files) in &selections {
                    self.backend.download_files(*torrent_id, files);
                }
                self.invalidate();
            }
            None => {}
        }
    }

    /// The film view: every copy of the film, best first.
    fn work_version_list(
        &mut self,
        ui: &mut egui::Ui,
        work: &Work,
        action: &mut Option<DetailAction>,
    ) {
        let versions = work.versions();
        ui.label(
            egui::RichText::new(format!("Versions ({})", versions.len()))
                .size(16.0)
                .strong(),
        );
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(
                "Play a copy, or download one to keep it. The best copy is first; each can be \
                 downloaded and removed on its own.",
            )
            .color(theme::TEXT_DIM)
            .size(11.0),
        );
        ui.add_space(6.0);
        self.pause_notice(ui, work);

        for (index, version) in versions.iter().enumerate() {
            // A copy is "kept" because the user asked for that copy, not because
            // the title as a whole is a download.
            let state = self
                .item(version.torrent_id)
                .map(|item| download_state(&item, version.file_id))
                .unwrap_or(DownloadState {
                    kept: false,
                    done: false,
                    fraction: 0.0,
                });

            egui::Frame::NONE
                .fill(theme::SURFACE)
                .inner_margin(egui::Margin::symmetric(12, 10))
                .corner_radius(theme::CARD_RADIUS)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            let label = if index == 0 {
                                format!("{}  \u{00b7}  best", version.attributes.quality_label())
                            } else {
                                version.attributes.quality_label()
                            };
                            ui.label(egui::RichText::new(label).size(13.5).strong());
                            ui.label(
                                egui::RichText::new(format!(
                                    "{}  \u{00b7}  {}",
                                    version.title,
                                    fmt::human_bytes(version.size)
                                ))
                                .size(11.0)
                                .color(theme::TEXT_DIM),
                            );
                            // The progress belongs to this download, not the
                            // title as a whole.
                            if state.kept && !state.done {
                                ui.add_space(3.0);
                                ui.add(
                                    egui::ProgressBar::new(state.fraction)
                                        .desired_width(240.0)
                                        .fill(theme::ACCENT)
                                        .text(format!("{:.0}%", state.fraction * 100.0)),
                                );
                            }
                        });

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Play").clicked() {
                                *action =
                                    Some(DetailAction::Play(version.torrent_id, version.file_id));
                            }
                            download_buttons(
                                ui,
                                version.torrent_id,
                                version.file_id,
                                state,
                                action,
                            );
                            if ui
                                .small_button("Remove")
                                .on_hover_text("Remove this source")
                                .clicked()
                            {
                                *action = Some(DetailAction::RemoveSource(version.torrent_id));
                            }
                        });
                    });
                });
            ui.add_space(4.0);
        }

        self.extras_list(ui, work);
    }

    /// Explain the paused-on-add state: the engine holds a fetch plan but is
    /// not fetching, so nothing should look like it is downloading.
    fn pause_notice(&self, ui: &mut egui::Ui, work: &Work) {
        let downloading = work
            .members
            .iter()
            .any(|member| self.item(member.torrent_id()).is_some_and(|item| item.downloading));
        let paused = self
            .lead_item(work)
            .is_some_and(|item| item.torrent.stats.is_paused());
        if paused && !downloading {
            ui.label(
                egui::RichText::new(
                    "Paused \u{2014} nothing is being fetched yet. Press Play to stream a copy, \
                     or Download one to keep it.",
                )
                .size(11.5)
                .color(theme::WARN),
            );
        }
    }

    /// Bonus material, shared by the film and series views.
    fn extras_list(&self, ui: &mut egui::Ui, work: &Work) {
        let extras = work.extras();
        if extras.is_empty() {
            return;
        }
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Extras").size(13.0).color(theme::TEXT_DIM));
        for extra in extras {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(reel_core::title::truncate(&extra.path, 60))
                        .size(12.0)
                        .color(theme::TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(fmt::human_bytes(extra.size))
                            .size(11.0)
                            .color(theme::TEXT_DIM),
                    );
                });
            });
        }
    }

    /// The series view: episodes merged across every source.
    fn work_episode_list(
        &mut self,
        ui: &mut egui::Ui,
        work: &Work,
        action: &mut Option<DetailAction>,
    ) {
        let episodes = work.episodes();
        let seasons = work.seasons();

        let mut heading = work.title.clone();
        if seasons.len() == 1 {
            if let Some(number) = seasons[0].number {
                heading = format!("{} \u{2014} Season {number}", work.title);
            }
        }
        heading.push_str(&format!("  \u{2022}  {}", plural(episodes.len(), "episode")));
        if seasons.len() > 1 {
            heading.push_str(&format!("  \u{2022}  {} seasons", seasons.len()));
        }
        ui.label(egui::RichText::new(heading).size(16.0).strong());

        let duplicates = episodes
            .iter()
            .filter(|episode| episode.variants.len() > 1)
            .count();
        if duplicates > 0 {
            ui.label(
                egui::RichText::new(format!(
                    "{duplicates} episode{} held in more than one copy \u{2014} pick a copy on \
                     the row.",
                    if duplicates == 1 { "" } else { "s" }
                ))
                .size(11.0)
                .color(theme::ACCENT),
            );
        }
        if let Some(overview) = work.metadata.as_ref().and_then(|m| m.overview.as_deref()) {
            ui.label(
                egui::RichText::new(reel_core::title::truncate(overview, 160))
                    .size(12.0)
                    .color(theme::TEXT_DIM),
            );
        }
        self.pause_notice(ui, work);
        ui.add_space(8.0);

        if ui
            .small_button("Download all episodes")
            .on_hover_text("Fetch the best copy of every episode")
            .clicked()
        {
            let mut batches: Vec<(usize, Vec<usize>)> = Vec::new();
            for episode in &episodes {
                if let Some(variant) = self.chosen_variant(work, episode) {
                    match batches.iter_mut().find(|(id, _)| *id == variant.torrent_id) {
                        Some((_, files)) => files.push(variant.file_id),
                        None => batches.push((variant.torrent_id, vec![variant.file_id])),
                    }
                }
            }
            *action = Some(DetailAction::DownloadEpisodes(batches));
        }
        ui.add_space(8.0);

        for season in &seasons {
            if seasons.len() > 1 {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let label = match season.number {
                        Some(number) => format!("Season {number}"),
                        None => "Unnumbered".to_string(),
                    };
                    ui.label(
                        egui::RichText::new(label)
                            .size(13.0)
                            .strong()
                            .color(theme::ACCENT),
                    );
                    if ui
                        .small_button("Download season")
                        .on_hover_text("Fetch the best copy of every episode in this season")
                        .clicked()
                    {
                        *action = Some(DetailAction::DownloadEpisodes(
                            self.season_selection(work, season.number),
                        ));
                    }
                });
            }
            for episode in &season.episodes {
                self.episode_row(ui, work, episode, action);
            }
        }

        self.extras_list(ui, work);
    }

    /// One episode row, with a copy chooser when the library holds several.
    fn episode_row(
        &mut self,
        ui: &mut egui::Ui,
        work: &Work,
        episode: &WorkEpisode,
        action: &mut Option<DetailAction>,
    ) {
        let chosen = self.chosen_variant(work, episode).cloned();
        let info = work.episode_info(episode).cloned();

        let watched = chosen.as_ref().and_then(|variant| {
            self.item(variant.torrent_id)
                .and_then(|item| item.entry.watch_for_file(variant.file_id).cloned())
        });
        // Kept is per episode, not per title: downloading one episode of a
        // pack keeps that episode and nothing else.
        let state = chosen
            .as_ref()
            .and_then(|variant| {
                self.item(variant.torrent_id)
                    .map(|item| download_state(&item, variant.file_id))
            })
            .unwrap_or(DownloadState {
                kept: false,
                done: false,
                fraction: 0.0,
            });

        let key = episode_choice_key(work, episode);
        let code = episode.code();
        let name = info
            .as_ref()
            .and_then(|info| info.name.clone())
            .unwrap_or_else(|| "(no title found)".to_string());
        let size = chosen.as_ref().map(|variant| variant.size).unwrap_or(0);
        let quality = chosen
            .as_ref()
            .map(|variant| variant.attributes.quality_label())
            .unwrap_or_else(|| "quality unknown".to_string());
        let variants = episode.variants.clone();
        let chosen_pair = chosen.as_ref().map(|variant| (variant.torrent_id, variant.file_id));

        egui::Frame::NONE
            .fill(theme::SURFACE)
            .inner_margin(egui::Margin::symmetric(10, 8))
            .corner_radius(theme::CARD_RADIUS)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (thumb, _) = ui.allocate_exact_size(Vec2::new(96.0, 54.0), Sense::hover());
                    match info.as_ref().and_then(|info| info.still_uri()) {
                        Some(uri) => {
                            egui::Image::new(uri)
                                .corner_radius(CornerRadius::same(4))
                                .paint_at(ui, thumb);
                        }
                        None => {
                            ui.painter().rect_filled(
                                thumb,
                                CornerRadius::same(4),
                                theme::SURFACE_RAISED,
                            );
                            ui.painter().text(
                                thumb.center(),
                                egui::Align2::CENTER_CENTER,
                                episode
                                    .episode
                                    .map(|n| format!("E{n:02}"))
                                    .unwrap_or_else(|| "?".to_string()),
                                egui::FontId::proportional(14.0),
                                theme::TEXT_DIM,
                            );
                        }
                    }

                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{code}  {name}"))
                                .size(13.5)
                                .strong(),
                        );
                        let mut facts = Vec::new();
                        if let Some(runtime) = info.as_ref().and_then(|info| info.runtime_minutes) {
                            facts.push(format!("{runtime}m"));
                        }
                        if let Some(air) = info.as_ref().and_then(|info| info.air_date.as_deref()) {
                            facts.push(air.to_string());
                        }
                        facts.push(fmt::human_bytes(size));
                        facts.push(quality.clone());
                        if variants.len() > 1 {
                            facts.push(format!("{} copies", variants.len()));
                        }
                        if let Some(progress) = watched.as_ref() {
                            if progress.is_finished() {
                                facts.push("watched".to_string());
                            } else {
                                facts.push(format!(
                                    "resume {}",
                                    fmt::human_duration(progress.position)
                                ));
                            }
                        }
                        ui.label(
                            egui::RichText::new(facts.join("   \u{2022}   "))
                                .size(11.0)
                                .color(theme::TEXT_DIM),
                        );
                        if state.kept && !state.done {
                            ui.add_space(3.0);
                            ui.add(
                                egui::ProgressBar::new(state.fraction)
                                    .desired_width(240.0)
                                    .fill(theme::ACCENT)
                                    .text(format!("{:.0}%", state.fraction * 100.0)),
                            );
                        }
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some((torrent_id, file_id)) = chosen_pair {
                            if ui.small_button("Play").clicked() {
                                *action = Some(DetailAction::Play(torrent_id, file_id));
                            }
                            download_buttons(ui, torrent_id, file_id, state, action);
                        }

                        if variants.len() > 1 {
                            ui.menu_button(format!("copy: {quality}"), |ui| {
                                for variant in &variants {
                                    let selected = chosen_pair
                                        == Some((variant.torrent_id, variant.file_id));
                                    let label = format!(
                                        "{}  \u{00b7}  {}",
                                        variant.attributes.quality_label(),
                                        fmt::human_bytes(variant.size)
                                    );
                                    if ui.selectable_label(selected, label).clicked() {
                                        self.episode_choice.insert(
                                            key.clone(),
                                            (variant.torrent_id, variant.file_id),
                                        );
                                        ui.close();
                                    }
                                }
                            });
                        }
                    });
                });
            });
        ui.add_space(4.0);
    }

    /// Every source backing the work, with per-source controls.
    fn work_sources(
        &mut self,
        ui: &mut egui::Ui,
        work: &Work,
        action: &mut Option<DetailAction>,
    ) {
        ui.add_space(16.0);
        ui.separator();
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new(format!("Sources ({})", work.torrent_count()))
                .size(16.0)
                .strong(),
        );
        ui.add_space(4.0);
        for member in &work.members {
            let torrent_id = member.torrent_id();
            let Some(item) = self.item(torrent_id) else {
                continue;
            };
            egui::Frame::NONE
                .fill(theme::SURFACE)
                .inner_margin(egui::Margin::symmetric(12, 8))
                .corner_radius(theme::CARD_RADIUS)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new(reel_core::title::truncate(&item.heading(), 56))
                                    .size(12.5),
                            );
                            ui.label(
                                egui::RichText::new(format!(
                                    "{}  \u{00b7}  {}  \u{00b7}  {:.0}%",
                                    member.attributes().quality_label(),
                                    fmt::human_bytes(item.torrent.stats.total_bytes),
                                    item.torrent.stats.percent
                                ))
                                .size(10.5)
                                .color(theme::TEXT_DIM),
                            );
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Remove").clicked() {
                                *action = Some(DetailAction::RemoveSource(torrent_id));
                            }
                            let paused = item.torrent.stats.is_paused();
                            let label = if paused { "Resume" } else { "Pause" };
                            if ui
                                .add_enabled(
                                    !item.torrent.finished,
                                    egui::Button::new(label).small(),
                                )
                                .clicked()
                            {
                                *action = Some(DetailAction::SetPaused(torrent_id, !paused));
                            }
                        });
                    });
                });
            ui.add_space(4.0);
        }
    }

    /// What is stored where, shown under either view.
    fn storage_line(&self, ui: &mut egui::Ui, work: &Work) {
        ui.add_space(6.0);
        let Some(item) = self.lead_item(work) else {
            return;
        };
        let mut facts = vec![item.torrent.output_folder.clone()];
        if let Some(metadata) = work.metadata.as_ref() {
            facts.push(format!("matched on {}", metadata.source));
        }
        let attributes = work.lead().attributes().summary();
        if !attributes.is_empty() {
            facts.push(attributes.join(" "));
        }
        ui.label(
            egui::RichText::new(facts.join("   \u{2022}   "))
                .color(theme::TEXT_DIM)
                .size(11.0),
        );
    }

}

enum DetailAction {
    Navigate(Screen),
    OpenRemove,
    RemoveSource(usize),
    SetPaused(usize, bool),
    MarkWatched,
    Play(usize, usize),
    PlayWork,
    PlayWorkFromStart,
    Download(usize, usize),
    CancelDownload(usize, usize),
    DownloadEpisodes(Vec<(usize, Vec<usize>)>),
}

/// The stable key an episode's chosen copy is remembered under, for a session.
fn episode_choice_key(work: &Work, episode: &WorkEpisode) -> String {
    format!(
        "{}|{:?}|{:?}|{}",
        work.key,
        episode.season,
        episode.episode,
        episode.air_date.as_deref().unwrap_or("")
    )
}

/// Subtitle files that belong to a video file.
///
/// A sidecar is matched either by episode code (`Show.S01E02.en.srt` for
/// `Show.S01E02.mkv`, even in a separate `Subs/` folder) or, when the names
/// carry no code, by sitting beside the video and sharing its stem
/// (`Movie.en.srt` for `Movie.mkv`).
pub fn companion_subtitles(
    files: &[reel_core::model::FileView],
    video: &reel_core::model::FileView,
) -> Vec<usize> {
    let video_token = episode_token(&video.name);
    let video_stem = file_stem(&video.name);
    let video_dir = dir_of(&video.path);

    files
        .iter()
        .filter(|file| file.is_subtitle)
        .filter(|file| match (episode_token(&file.name), video_token.as_deref()) {
            // The episode code is the strongest signal: it keeps S01E03's
            // subtitles away from S01E02 even in a shared folder.
            (Some(sub), Some(token)) => sub == token,
            _ => {
                dir_of(&file.path) == video_dir
                    && shares_title_prefix(&file_stem(&file.name), &video_stem)
            }
        })
        .map(|file| file.id)
        .collect()
}

/// The `SxxEyy` episode code in a name, lowercased, if there is one.
fn episode_token(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b's' {
            i += 1;
            continue;
        }
        let season_start = i + 1;
        let mut j = season_start;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j == season_start || j >= bytes.len() || bytes[j] != b'e' {
            i += 1;
            continue;
        }
        let episode_start = j + 1;
        let mut k = episode_start;
        while k < bytes.len() && bytes[k].is_ascii_digit() {
            k += 1;
        }
        if k == episode_start {
            i += 1;
            continue;
        }
        return Some(format!(
            "s{}e{}",
            &lower[season_start..j],
            &lower[episode_start..k]
        ));
    }
    None
}

/// Lowercased file name without its directory or extension.
fn file_stem(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_ascii_lowercase(),
        _ => base.to_ascii_lowercase(),
    }
}

/// Whether two file stems name the same title.
///
/// Subtitle names are usually a prefix of the video's (`the.matrix` versus
/// `the.matrix.1999.1080p`), so matching on a shared leading run of dot-separated
/// tokens handles both that and a plain `movie.mkv` / `movie.srt` pair. A single
/// shared token like `the` is not enough, so unrelated films do not collide.
fn shares_title_prefix(a: &str, b: &str) -> bool {
    let a_tokens: Vec<&str> = a.split('.').filter(|token| !token.is_empty()).collect();
    let b_tokens: Vec<&str> = b.split('.').filter(|token| !token.is_empty()).collect();
    if a_tokens.is_empty() || b_tokens.is_empty() {
        return false;
    }
    let common = a_tokens
        .iter()
        .zip(b_tokens.iter())
        .take_while(|(left, right)| left == right)
        .count();
    common >= a_tokens.len().min(b_tokens.len()) || common >= 2
}

/// Lowercased directory portion of a `/`-separated path.
fn dir_of(path: &str) -> String {
    match path.rsplit_once(['/', '\\']) {
        Some((dir, _)) => dir.to_ascii_lowercase(),
        None => String::new(),
    }
}

/// The shape providers publish artwork in.
fn source_aspect(kind: ArtworkKind) -> f32 {
    match kind {
        ArtworkKind::Poster => 2.0 / 3.0,
        ArtworkKind::Backdrop => 16.0 / 9.0,
    }
}

/// A UV sub-rectangle that fills `target_aspect` from a `source_aspect` image
/// without distorting it, cropped centrally.
///
/// This is the "cover" fit: a 16:9 backdrop shown in a 5:1 banner keeps its
/// middle band rather than being squashed vertically.
pub fn cover_uv(source_aspect: f32, target_aspect: f32) -> Rect {
    let full = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));

    if !source_aspect.is_finite()
        || !target_aspect.is_finite()
        || source_aspect <= 0.0
        || target_aspect <= 0.0
    {
        return full;
    }

    let mut uv = full;
    if source_aspect > target_aspect {
        // Source is wider than the hole: keep a centred vertical slice.
        let keep = (target_aspect / source_aspect).clamp(0.0, 1.0);
        let inset = (1.0 - keep) / 2.0;
        uv.min.x = inset;
        uv.max.x = 1.0 - inset;
    } else {
        // Source is taller: keep a centred horizontal band.
        let keep = (source_aspect / target_aspect).clamp(0.0, 1.0);
        let inset = (1.0 - keep) / 2.0;
        uv.min.y = inset;
        uv.max.y = 1.0 - inset;
    }
    uv
}

/// `1 title`, `2 titles`.
fn plural(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

/// What a single file's download is doing, for one row.
#[derive(Debug, Clone, Copy)]
pub struct DownloadState {
    pub kept: bool,
    pub done: bool,
    pub fraction: f32,
}

/// The download state of one file of a title.
pub fn download_state(item: &LibraryItem, file_id: usize) -> DownloadState {
    let kept = item.kept_files.contains(&file_id);
    let file = item.torrent.files.iter().find(|file| file.id == file_id);
    let progress = file.map(|file| file.progress_bytes).unwrap_or(0);
    let length = file.map(|file| file.length).unwrap_or(0);
    DownloadState {
        kept,
        done: length > 0 && progress >= length,
        fraction: if length > 0 {
            (progress as f32 / length as f32).clamp(0.0, 1.0)
        } else {
            0.0
        },
    }
}

/// The Download control for one file: Download, cancel, or — when it is fully
/// on disk — a plain "Downloaded" with a way to remove it.
fn download_buttons(
    ui: &mut egui::Ui,
    torrent_id: usize,
    file_id: usize,
    state: DownloadState,
    action: &mut Option<DetailAction>,
) {
    if state.kept {
        if state.done {
            ui.label(
                egui::RichText::new("Downloaded")
                    .color(theme::OK)
                    .size(11.5),
            );
            if ui
                .small_button("Remove download")
                .on_hover_text("Delete the downloaded file")
                .clicked()
            {
                *action = Some(DetailAction::CancelDownload(torrent_id, file_id));
            }
        } else if ui
            .small_button("Stop download")
            .on_hover_text("Cancel this download")
            .clicked()
        {
            *action = Some(DetailAction::CancelDownload(torrent_id, file_id));
        }
    } else if ui
        .small_button("Download")
        .on_hover_text("Keep this on disk")
        .clicked()
    {
        *action = Some(DetailAction::Download(torrent_id, file_id));
    }
}

fn stat(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.vertical(|ui| {
        ui.label(egui::RichText::new(label).size(10.0).color(theme::TEXT_DIM));
        ui.label(egui::RichText::new(value).size(13.0));
    });
    ui.add_space(8.0);
}

// --------------------------------------------------------------------- add

impl App {
    fn add_screen(&mut self, ui: &mut egui::Ui) {
        ui.add_space(16.0);
        ui.label(egui::RichText::new("Add a torrent").size(22.0).strong());
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Paste a magnet link, an http(s) .torrent URL, or a 40-character info hash. \
                 reel fetches the metadata, picks the video, and starts streaming it.",
            )
            .color(theme::TEXT_DIM),
        );

        ui.add_space(14.0);
        let response = ui.add(
            egui::TextEdit::multiline(&mut self.add_source)
                .hint_text("magnet:?xt=urn:btih:\u{2026}")
                .desired_width(f32::INFINITY)
                .desired_rows(3),
        );

        ui.add_space(8.0);
        ui.checkbox(
            &mut self.add_media_only,
            "Only download playable files (skips samples and extras)",
        );

        ui.add_space(12.0);
        let submit = ui
            .add(egui::Button::new(egui::RichText::new("Add torrent").size(15.0)))
            .clicked();
        let enter = response.has_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);

        if submit || enter {
            self.submit_add();
        }

        let status = self.backend.catalog_status();
        if !status.configured {
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(
                    status
                        .note
                        .clone()
                        .unwrap_or_else(|| "Metadata is unavailable.".into()),
                )
                .color(theme::WARN)
                .size(12.0),
            );
        }

        if !self.library.is_empty() {
            ui.add_space(24.0);
            ui.label(egui::RichText::new("Already in your library").size(16.0).strong());
            ui.add_space(6.0);
            for item in self.library.clone() {
                ui.horizontal(|ui| {
                    ui.label(reel_core::title::truncate(&item.heading(), 50));
                    if ui.small_button("Open").clicked() {
                        self.screen = Screen::Detail(item.torrent.id);
                    }
                });
            }
        }
    }
}

// ------------------------------------------------------------------ search

impl App {
    fn search_screen(&mut self, ui: &mut egui::Ui) {
        let sources = self.backend.search_sources();

        ui.add_space(16.0);
        ui.label(egui::RichText::new("Search").size(22.0).strong());
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Look for something to watch across the configured sources. Adding a result \
                 hands it to the same engine, which streams it while it downloads.",
            )
            .color(theme::TEXT_DIM),
        );

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.search_query)
                    .hint_text("title to search for\u{2026}")
                    .desired_width(420.0),
            );
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let clicked = ui.button("Search").clicked();
            if clicked || enter {
                self.submit_search();
            }
            if self.searching {
                ui.spinner();
            }
        });

        ui.add_space(6.0);
        if sources.is_empty() {
            ui.colored_label(
                theme::WARN,
                "No search sources are configured. See docs/ADDING_A_SOURCE.md.",
            );
        } else {
            let names: Vec<String> = sources
                .iter()
                .map(|source| {
                    if source.configured {
                        source.name.clone()
                    } else {
                        format!("{} (disabled)", source.name)
                    }
                })
                .collect();
            ui.label(
                egui::RichText::new(format!("Sources: {}", names.join(", ")))
                    .size(11.0)
                    .color(theme::TEXT_DIM),
            );
        }

        if let Some(note) = self.search_note.clone() {
            ui.add_space(4.0);
            ui.label(egui::RichText::new(note).size(11.5).color(theme::TEXT_DIM));
        }

        ui.add_space(12.0);

        if self.search_hits.is_empty() {
            ui.label(
                egui::RichText::new(if self.search_for.is_some() {
                    "Nothing matched."
                } else {
                    "Results will appear here."
                })
                .color(theme::TEXT_DIM),
            );
            return;
        }

        let mut add: Option<SearchHit> = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            for hit in self.search_hits.clone() {
                egui::Frame::NONE
                    .fill(theme::SURFACE)
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .corner_radius(theme::CARD_RADIUS)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new(reel_core::title::truncate(&hit.title, 70))
                                        .size(14.0)
                                        .strong(),
                                );

                                let mut facts = Vec::new();
                                if let Some(year) = hit.year {
                                    facts.push(year.to_string());
                                }
                                facts.push(hit.source.clone());
                                if let Some(size) = hit.size_bytes {
                                    facts.push(fmt::human_bytes(size));
                                }
                                if let Some(seeders) = hit.seeders {
                                    facts.push(format!("{seeders} seeders"));
                                }
                                if let Some(popularity) = hit.popularity {
                                    facts.push(format!("{popularity} downloads"));
                                }
                                ui.label(
                                    egui::RichText::new(facts.join("   \u{2022}   "))
                                        .size(11.0)
                                        .color(theme::TEXT_DIM),
                                );
                            });

                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add_enabled(
                                            hit.is_usable(),
                                            egui::Button::new("Add"),
                                        )
                                        .on_disabled_hover_text("This result has no magnet or .torrent URL")
                                        .clicked()
                                    {
                                        add = Some(hit.clone());
                                    }
                                },
                            );
                        });
                    });
                ui.add_space(6.0);
            }
        });

        if let Some(hit) = add {
            match hit.magnet.clone().or_else(|| hit.torrent_url.clone()) {
                Some(source) => {
                    self.backend.add(&source, self.add_media_only);
                    self.set_toast(format!("Adding {}", reel_core::title::truncate(&hit.title, 40)), false);
                }
                None => self.warn("This result has nothing to add"),
            }
        }
    }
}

/// One line summarising a search, including sources that failed.
fn describe_search(results: &reel_catalog::SearchResults) -> Option<String> {
    let mut parts = Vec::new();
    parts.push(plural(results.hits.len(), "result"));
    for (source, message) in &results.failures {
        parts.push(format!("{source} failed: {}", reel_core::title::truncate(message, 80)));
    }
    Some(parts.join("   \u{2022}   "))
}

// ---------------------------------------------------------------- settings

impl App {
    fn settings_screen(&mut self, ui: &mut egui::Ui) {
        let caps = self.backend.capabilities().clone();
        let catalog = self.backend.catalog_status();
        let mut action: Option<SettingsAction> = None;
        let mut check_key: Option<String> = None;
        let mut persist = false;
        let mut remove_key = false;

        // A local draft so edits are explicit and only written on Save.
        let mut draft = self
            .settings_draft
            .take()
            .unwrap_or_else(|| self.backend.settings());

        ui.add_space(16.0);
        ui.label(egui::RichText::new("Settings").size(22.0).strong());
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(
                "These are saved to disk and are the way this app is configured. Environment \
                 variables are only a first-run fallback.",
            )
            .color(theme::TEXT_DIM)
            .size(11.5),
        );
        ui.add_space(10.0);

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.label(egui::RichText::new("Library").size(16.0).strong());
            row(ui, "Active folder", &caps.download_dir);
            ui.horizontal(|ui| {
                ui.add_sized(
                    Vec2::new(150.0, 18.0),
                    egui::Label::new(
                        egui::RichText::new("Download folder").color(theme::TEXT_DIM),
                    ),
                );
                let mut dir = draft.download_dir.clone().unwrap_or_default();
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut dir)
                            .hint_text("default: ~/Downloads/reel")
                            .desired_width(360.0),
                    )
                    .changed()
                {
                    draft.download_dir = Some(dir).filter(|d| !d.trim().is_empty());
                }
            });
            ui.label(
                egui::RichText::new("Changes to the download folder take effect next start.")
                    .color(theme::TEXT_DIM)
                    .size(11.0),
            );
            row(ui, "Streaming API", self.backend.base_url());
            row(ui, "Client name", &caps.client_name);
            ui.checkbox(
                &mut draft.merge_works,
                "Merge copies of the same film or show into one title",
            );
            ui.label(
                egui::RichText::new(
                    "With this on, a 4K and a 1080p copy of a film are one title with a version \
                     list, and overlapping seasons or episodes of a show are merged with a \
                     per-episode choice. Turn it off to list every torrent separately.",
                )
                .color(theme::TEXT_DIM)
                .size(11.0),
            );
            ui.add_space(14.0);

            ui.label(egui::RichText::new("Metadata and artwork").size(16.0).strong());
            row(ui, "Provider", &catalog.provider);
            let status = if catalog.configured {
                format!(
                    "{} of {} titles matched, {} lookups in flight",
                    catalog.enriched,
                    self.library.len(),
                    catalog.pending
                )
            } else {
                "not configured".to_string()
            };
            row(ui, "Status", &status);
            row(ui, "Key", &catalog.key_source);
            row(ui, "Cache folder", &catalog.cache_dir);
            row(
                ui,
                "Cache",
                &format!(
                    "{} files, {}",
                    catalog.cached_metadata,
                    fmt::human_bytes(catalog.cache_bytes)
                ),
            );

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_sized(
                    Vec2::new(150.0, 18.0),
                    egui::Label::new(
                        egui::RichText::new("Metadata API key").color(theme::TEXT_DIM),
                    ),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.api_key_input)
                        .password(true)
                        .hint_text("TMDB v3 key or v4 token")
                        .desired_width(300.0),
                );
                let typed = !self.api_key_input.trim().is_empty();
                if ui.add_enabled(typed, egui::Button::new("Save")).clicked() {
                    draft.tmdb_api_key = Some(self.api_key_input.trim().to_string());
                    persist = true;
                }
                if ui.add_enabled(typed, egui::Button::new("Check")).clicked() {
                    check_key = Some(self.api_key_input.clone());
                }
            });
            ui.label(
                egui::RichText::new(
                    "The metadata provider is TMDB (themoviedb.org); IMDb has no official \
                     public API. A free TMDB account gives you a v3 key or a v4 read access \
                     token starting with eyJ. Either works. A saved key always wins over \
                     REEL_TMDB_API_KEY.",
                )
                .color(theme::TEXT_DIM)
                .size(11.0),
            );

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Look up metadata again").clicked() {
                    action = Some(SettingsAction::RefreshMetadata);
                }
                if ui.button("Clear poster and metadata cache").clicked() {
                    action = Some(SettingsAction::ClearCache);
                }
                let has_key = draft.stored_key().is_some();
                if ui
                    .add_enabled(has_key, egui::Button::new("Remove stored key"))
                    .clicked()
                {
                    remove_key = true;
                }
            });
            if let Some(note) = catalog.note.as_deref() {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(note).color(theme::WARN).size(12.0));
            }
            ui.add_space(14.0);

            ui.label(egui::RichText::new("Streaming and downloads").size(16.0).strong());
            ui.checkbox(
                &mut draft.stream_only,
                "Stream on demand (new multi-file torrents wait until you pick something)",
            );
            ui.label(
                egui::RichText::new(
                    "With this on, watching a file fetches only that file (and its subtitles). \
                     Use the Download buttons on a title to keep individual files, episodes or \
                     a whole season. With it off, new torrents download every playable file \
                     in the background.",
                )
                .color(theme::TEXT_DIM)
                .size(11.0),
            );
            ui.add_space(10.0);

            ui.label(egui::RichText::new("Playback").size(16.0).strong());
            ui.checkbox(&mut draft.subtitles_enabled, "Turn subtitles on when a file has them");
            ui.horizontal(|ui| {
                ui.add_sized(
                    Vec2::new(150.0, 18.0),
                    egui::Label::new(
                        egui::RichText::new("Subtitle language").color(theme::TEXT_DIM),
                    ),
                );
                let mut language = draft.subtitle_language.clone().unwrap_or_default();
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut language)
                            .hint_text("auto")
                            .desired_width(100.0),
                    )
                    .changed()
                {
                    draft.subtitle_language = Some(language).filter(|l| !l.trim().is_empty());
                }
                ui.label(
                    egui::RichText::new("e.g. en. Blank lets the player choose.")
                        .color(theme::TEXT_DIM)
                        .size(11.0),
                );
            });
            ui.horizontal(|ui| {
                ui.add_sized(
                    Vec2::new(150.0, 18.0),
                    egui::Label::new(egui::RichText::new("Default volume").color(theme::TEXT_DIM)),
                );
                ui.add(
                    egui::Slider::new(&mut draft.default_volume, 0.0..=130.0).suffix(" %"),
                );
            });
            ui.horizontal(|ui| {
                ui.add_sized(
                    Vec2::new(150.0, 18.0),
                    egui::Label::new(
                        egui::RichText::new("Stream buffer").color(theme::TEXT_DIM),
                    ),
                );
                ui.add(
                    egui::DragValue::new(&mut draft.stream_buffer_mb)
                        .range(0..=8192)
                        .suffix(" MB"),
                );
                ui.label(
                    egui::RichText::new(
                        "How far ahead a stream may fetch before it pauses. 0 removes the limit.",
                    )
                    .color(theme::TEXT_DIM)
                    .size(11.0),
                );
            });
            ui.horizontal(|ui| {
                ui.add_sized(
                    Vec2::new(150.0, 18.0),
                    egui::Label::new(
                        egui::RichText::new("Session cache").color(theme::TEXT_DIM),
                    ),
                );
                ui.add(
                    egui::DragValue::new(&mut draft.stream_cache_mb)
                        .range(0..=65536)
                        .suffix(" MB"),
                );
                ui.label(
                    egui::RichText::new(
                        "Streamed data kept for instant replay this session. 0 releases it on stop.",
                    )
                    .color(theme::TEXT_DIM)
                    .size(11.0),
                );
            });
            match &caps.player {
                PlayerCapability::Embedded => {
                    row(ui, "Backend", "libmpv (embedded in this window)");
                    if let Some((major, minor)) = caps.mpv_api {
                        row(ui, "mpv client API", &format!("{major}.{minor}"));
                    }
                }
                PlayerCapability::External { program } => {
                    row(ui, "Backend", &format!("external player ({program})"));
                    if let Some(note) = caps.player_note.as_deref() {
                        ui.label(egui::RichText::new(note).color(theme::WARN).size(12.0));
                    }
                }
                PlayerCapability::Unavailable { reason } => {
                    row(ui, "Backend", "unavailable");
                    ui.label(egui::RichText::new(reason).color(theme::DANGER).size(12.0));
                }
            }
            ui.add_space(14.0);

            ui.label(egui::RichText::new("Search sources").size(16.0).strong());
            ui.checkbox(
                &mut draft.enable_bundled_sources,
                "Use the bundled source (Internet Archive)",
            );
            let sources = self.backend.search_sources();
            if sources.is_empty() {
                row(ui, "Sources", "none");
            } else {
                for source in &sources {
                    row(
                        ui,
                        "Source",
                        &if source.configured {
                            source.name.clone()
                        } else {
                            format!("{} (disabled)", source.name)
                        },
                    );
                }
            }
            ui.label(
                egui::RichText::new(
                    "A search backend is anything implementing the SearchBackend trait; add \
                     your own in docs/ADDING_A_SOURCE.md.",
                )
                .color(theme::TEXT_DIM)
                .size(12.0),
            );
            ui.add_space(14.0);

            ui.label(egui::RichText::new("About").size(16.0).strong());
            row(ui, "Version", env!("CARGO_PKG_VERSION"));
            ui.label(
                egui::RichText::new(
                    "reel streams torrents over HTTP with byte-range seeking. Titles are \
                     matched against a metadata provider; artwork is generated from each \
                     title when none is available.",
                )
                .color(theme::TEXT_DIM)
                .size(12.0),
            );
        });

        ui.add_space(10.0);
        if ui
            .add(egui::Button::new(
                egui::RichText::new("Save settings").size(14.0),
            ))
            .clicked()
        {
            persist = true;
        }

        if remove_key {
            draft.tmdb_api_key = None;
            persist = true;
        }

        self.settings_draft = Some(draft.clone());

        if persist {
            draft.normalise();
            self.backend.save_settings(draft.clone());
            self.settings_draft = Some(draft.clone());
            // Apply playback preferences to the running controller.
            self.player
                .set_preferences(crate::player::PlaybackPreferences {
                    subtitles_enabled: draft.subtitles_enabled,
                    subtitle_language: draft.subtitle_language.clone(),
                    volume: draft.default_volume,
                });
            self.api_key_input.clear();
        }
        if let Some(key) = check_key {
            self.backend.test_api_key(key);
        }

        match action {
            Some(SettingsAction::RefreshMetadata) => self.backend.refresh_metadata(),
            Some(SettingsAction::ClearCache) => self.backend.clear_catalog_cache(),
            None => {}
        }
    }
}

enum SettingsAction {
    RefreshMetadata,
    ClearCache,
}

fn row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            Vec2::new(150.0, 18.0),
            egui::Label::new(egui::RichText::new(label).color(theme::TEXT_DIM)),
        );
        ui.label(egui::RichText::new(value).monospace().size(12.0));
    });
    ui.add_space(2.0);
}

// ----------------------------------------------------------------- player

impl App {
    fn player_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Repaint continuously so the video keeps advancing without input.
        ctx.request_repaint();
        self.record_watch_progress();

        // Let the backend cap how far ahead the stream fetches.
        let info = self.player.current().cloned();
        let state = self.player.state();
        if let Some(info) = info {
            self.backend
                .note_playback(info.torrent_id, info.file_id, state.position, state.duration);
        }
        self.maybe_recover_stalled_stream();

        let Some(info) = self.player.current().cloned() else {
            self.screen = self
                .player_origin
                .map(Screen::Detail)
                .unwrap_or(Screen::Library);
            return;
        };

        // Keyboard shortcuts. The player has no text fields, so plain keys are
        // unambiguous here.
        let (space, escape, fullscreen, left, right, up, down) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::F),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
            )
        });
        let activity = ctx.input(|i| {
            i.pointer.delta() != Vec2::ZERO || i.pointer.any_down() || i.pointer.any_click()
        }) || space
            || fullscreen
            || left
            || right
            || up
            || down;
        if activity {
            self.player_last_activity = Instant::now();
            self.player_controls_hidden = false;
        }

        if fullscreen {
            self.player.toggle_fullscreen(ctx);
        }
        if escape {
            if self.player.is_fullscreen() {
                self.player.set_fullscreen(ctx, false);
            } else {
                self.leave_player();
                return;
            }
        }
        if space {
            self.player.toggle_pause();
        }
        if left {
            self.player.seek_relative(-10.0);
        }
        if right {
            self.player.seek_relative(30.0);
        }
        if up {
            let volume = (self.player.volume() + 5.0).min(130.0);
            self.player.set_volume(volume);
        }
        if down {
            let volume = (self.player.volume() - 5.0).max(0.0);
            self.player.set_volume(volume);
        }

        // In fullscreen the chrome fades out when nothing is happening, and
        // comes back the moment the pointer or a key moves.
        if self.player.is_fullscreen() {
            if self.player_last_activity.elapsed() > Duration::from_secs(2) {
                self.player_controls_hidden = true;
            }
        } else {
            self.player_controls_hidden = false;
        }
        let show_chrome = !self.player_controls_hidden;

        if show_chrome {
            egui::Panel::top("player-bar")
                .frame(
                    egui::Frame::NONE
                        .fill(theme::SURFACE)
                        .inner_margin(egui::Margin::symmetric(14, 8)),
                )
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("\u{2b05}  Back").clicked() {
                            self.leave_player();
                        }
                        ui.label(egui::RichText::new(&info.title).size(15.0).strong());

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let label = match self.player.backend() {
                                Some(reel_player::Backend::Embedded) => "libmpv (embedded)",
                                Some(reel_player::Backend::External) => "external player",
                                _ => "unknown backend",
                            };
                            ui.label(
                                egui::RichText::new(label).size(11.0).color(theme::TEXT_DIM),
                            );
                        });
                    });
                });
        }

        // External players own their window: explain rather than show a void.
        if self.player.backend() == Some(reel_player::Backend::External) {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.fill(theme::BG))
                .show(ui, |ui| {
                    ui.add_space(70.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new(format!("Playing {} elsewhere", info.title))
                                .size(22.0)
                                .strong(),
                        );
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new(
                                "libmpv is not available, so playback was handed to a \
                                 separate player process. The picture is in that program's \
                                 own window.",
                            )
                            .color(theme::TEXT_DIM),
                        );
                        ui.add_space(16.0);
                        let running = self.player.is_external_running();
                        ui.label(
                            egui::RichText::new(if running {
                                "The external player is running."
                            } else {
                                "The external player has exited."
                            })
                            .color(if running { theme::OK } else { theme::TEXT_DIM }),
                        );
                    });
                });
            self.draw_toast(ui, ctx);
            return;
        }

        if show_chrome {
            egui::Panel::bottom("player-controls")
                .frame(
                    egui::Frame::NONE
                        .fill(theme::SURFACE)
                        .inner_margin(egui::Margin::symmetric(16, 10)),
                )
                .show(ui, |ui| {
                    let stats = self.playback_stats(info.torrent_id);
                    ui.set_min_width(ui.available_width());
                    self.player.controls_ui(ui, &stats);
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::BLACK))
            .show(ui, |ui| {
                self.player.video_ui(ui, ctx);
            });

        if let Some(error) = self.player.error().map(str::to_string) {
            egui::Panel::bottom("player-error")
                .frame(
                    egui::Frame::NONE
                        .fill(theme::SURFACE_RAISED)
                        .inner_margin(egui::Margin::symmetric(14, 8)),
                )
                .show(ui, |ui| {
                    ui.colored_label(theme::DANGER, error);
                });
        }
    }

    /// A resumed stream that never yields a frame is worse than a re-fetch.
    ///
    /// The cached-resume path is the one that has been seen to stall; a fresh
    /// add always plays. So after a few seconds with no frame, re-add the
    /// stream from scratch once and reopen the player on the new URL.
    fn maybe_recover_stalled_stream(&mut self) {
        // A paused player, or one that reached the end, is not stalled.
        let state = self.player.state();
        if state.paused || state.eof {
            self.player_progress_at = Instant::now();
            return;
        }

        // Frames advancing means it is playing. No new frames for eight seconds
        // means it is wedged — which is what the log showed: the server had
        // megabytes queued and mpv had stopped reading them.
        let frames = self.player.frames_uploaded();
        if frames != self.player_last_frames {
            self.player_last_frames = frames;
            self.player_progress_at = Instant::now();
            self.stream_recovered = false;
            return;
        }
        if self.stream_recovered || self.player_progress_at.elapsed() < Duration::from_secs(8) {
            return;
        }
        let Some(info) = self.player.current().cloned() else {
            return;
        };
        self.stream_recovered = true;
        tracing::warn!(
            torrent_id = info.torrent_id,
            file_id = info.file_id,
            frames,
            "no new frames for 8s; restarting the stream from scratch"
        );
        self.pending_play = Some((info.torrent_id, info.file_id));
        self.backend.restart_stream(info.torrent_id, &[info.file_id]);
        self.set_toast("Reconnecting the stream\u{2026}", false);
    }

    /// Leave the player, saving where we got to.
    fn leave_player(&mut self) {
        if let (Some(info_hash), true) = (
            self.watch_recording_for.clone(),
            self.player.state().loaded,
        ) {
            let state = self.player.state();
            let info = self.player.current().cloned();
            if state.position >= 1.0 {
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

        self.player.close();
        self.player_last_frames = 0;
        self.player_progress_at = Instant::now();
        self.stream_recovered = false;
        self.watch_recording_for = None;
        self.watch_last_recorded = 0.0;
        self.screen = self
            .player_origin
            .map(Screen::Detail)
            .unwrap_or(Screen::Library);
        // A stream-only title gives its temporary storage back now; a download
        // is left alone.
        if let Some(origin) = self.player_origin {
            self.backend.stop_streaming(origin);
        }
        self.refresh();
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
    fn starts_on_the_library_with_rows() {
        let app = app();
        assert_eq!(app.screen(), &Screen::Library);
        assert_eq!(app.library_len(), 3);

        let rows = app.row_titles();
        assert_eq!(
            rows,
            ["Continue watching", "Recently added", "Not started", "Watched"],
            "every row should have something in the sample library"
        );
    }

    #[test]
    fn navigation_changes_screens() {
        let mut app = app();
        app.navigate(Screen::Add);
        assert_eq!(app.screen(), &Screen::Add);
        app.navigate(Screen::Settings);
        assert_eq!(app.screen(), &Screen::Settings);
        app.navigate(Screen::Detail(1));
        assert_eq!(app.screen(), &Screen::Detail(1));
    }

    #[test]
    fn adding_requires_a_source() {
        let mut app = app();
        app.navigate(Screen::Add);

        app.submit_add();
        assert!(app.toast.as_ref().is_some_and(|t| t.error));
        assert_eq!(app.screen(), &Screen::Add, "should stay put on failure");

        app.set_add_source("magnet:?xt=urn:btih:abc");
        app.submit_add();
        assert!(app.toast.as_ref().is_some_and(|t| !t.error));
        assert_eq!(app.add_source, "", "the field should be cleared on submit");
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

        app.backend.download_files(9, &[1]);
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
        app.backend.stop_download_files(9, &[1]);
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
    fn sidecar_subtitles_are_matched_to_their_video() {
        let item = sample_library()
            .into_iter()
            .find(|item| item.torrent.id == 1)
            .expect("the matrix");
        let video = item.torrent.files.iter().find(|f| f.id == 0).expect("video");
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
        assert_eq!(app.remove_selected.len(), 2, "all sources are ticked by default");

        app.open_remove_dialog(work_id, Some(1));
        assert_eq!(app.remove_selected.len(), 1);
        assert!(app.remove_selected.contains(&1));
    }

    #[test]
    fn episode_codes_are_read_from_names() {
        assert_eq!(episode_token("Show.S01E02.1080p.mkv").as_deref(), Some("s01e02"));
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
        assert_eq!(uv, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)));
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
        for (source, target) in [(0.0, 1.0), (1.0, 0.0), (f32::NAN, 1.0), (1.0, f32::INFINITY)] {
            assert_eq!(cover_uv(source, target), full);
        }
    }
}
