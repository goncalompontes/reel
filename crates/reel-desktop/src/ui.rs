//! The application: navigation, the catalog rows, the detail page, the add
//! form, settings, and the player screen.
//!
//! Rendering is pure egui — immediate mode, GPU-rendered, no webview. All state
//! comes from [`Backend`], so the same UI runs against a real engine or an
//! in-memory fake in tests.

use std::time::{Duration, Instant};

use egui::{Color32, CornerRadius, Rect, Sense, Vec2};
use reel_catalog::{ArtworkKind, CatalogSettings, RowKind, SearchHit};
use reel_core::fmt;
use reel_core::media;

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
    /// Row layout, rebuilt with the library rather than every frame.
    rows: Vec<RowLayout>,
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
    show_delete_confirm: Option<usize>,
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
            rows: Vec::new(),
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
            show_delete_confirm: None,
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

    /// Start playing a file, resuming from where it was left off.
    pub fn play_file(&mut self, ctx: &egui::Context, torrent_id: usize, file_id: usize) {
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

        // Sidecar subtitles ride along with the video, both as files to fetch
        // and as URLs for mpv to load.
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

        // Watching one episode should fetch one episode — and the torrent has to
        // be running. A pack is added paused, so selecting files alone would
        // leave playback waiting forever.
        let others = item
            .torrent
            .files
            .iter()
            .filter(|f| f.included && f.id != file_id && !companions.contains(&f.id))
            .count();
        let mut selection = vec![file_id];
        selection.extend(companions.iter().copied());
        selection.sort_unstable();
        selection.dedup();
        self.start_streaming(torrent_id, &selection);
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
        self.watch_last_recorded = start_at.unwrap_or(0.0);
        self.watch_recording_for = Some(item.entry.info_hash.clone());

        match self.player.open(ctx, &capability, &url, info) {
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

        let entries: Vec<reel_catalog::CatalogEntry> =
            self.library.iter().map(|item| item.entry.clone()).collect();
        self.rows = reel_catalog::build_rows(&entries, ROW_LIMIT)
            .into_iter()
            .map(|row| RowLayout {
                kind: row.kind,
                title: row.title,
                ids: row.entries.iter().map(|entry| entry.torrent_id).collect(),
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
                    self.screen = Screen::Detail(id);
                }
                BackendEvent::Metadata { .. } | BackendEvent::WatchUpdated => {
                    self.refresh();
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

    fn selected(&self) -> Option<LibraryItem> {
        match self.screen {
            Screen::Detail(id) => self.item(id),
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

    /// Fetch exactly these files of a torrent, and make sure it is running.
    ///
    /// Public so the flow the UI reaches by ticking a box can also be driven
    /// from a test. Resuming matters: a multi-file torrent is added paused, so
    /// without it a checked box would never actually download.
    pub fn select_files(&mut self, torrent_id: usize, files: &[usize]) {
        self.start_streaming(torrent_id, files);
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
        self.draw_delete_confirm(ui, &ctx);
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

    fn draw_delete_confirm(&mut self, _ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(id) = self.show_delete_confirm else {
            return;
        };
        let title = self
            .item(id)
            .map(|item| item.heading())
            .unwrap_or_else(|| format!("torrent {id}"));

        let mut close = false;
        let mut confirmed: Option<bool> = None;

        egui::Window::new("Remove torrent")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(format!("Remove {title}?"));
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Removing only forgets the torrent and any saved position. Deleting \
                         also removes the downloaded files from disk.",
                    )
                    .color(theme::TEXT_DIM),
                );
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    if ui.button("Forget torrent").clicked() {
                        confirmed = Some(false);
                    }
                    if ui
                        .button(egui::RichText::new("Delete files too").color(theme::DANGER))
                        .clicked()
                    {
                        confirmed = Some(true);
                    }
                });
            });

        if let Some(delete_files) = confirmed {
            self.backend.remove(id, delete_files);
            self.set_toast("Removed torrent", false);
            self.refresh();
            if self.screen == Screen::Detail(id) {
                self.screen = Screen::Library;
            }
            close = true;
        }
        if close {
            self.show_delete_confirm = None;
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
    fn paint_art(ui: &egui::Ui, rect: Rect, item: &LibraryItem, kind: ArtworkKind, radius: CornerRadius) {
        let reference = match kind {
            ArtworkKind::Poster => item.entry.poster(),
            ArtworkKind::Backdrop => item.entry.backdrop(),
        };

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

        let seed = item.entry.title();
        let base = match kind {
            ArtworkKind::Poster => theme::poster_color(&seed),
            ArtworkKind::Backdrop => theme::poster_shade(&seed),
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

    /// A poster tile: artwork, title, progress, and a resume bar.
    fn card(&mut self, ui: &mut egui::Ui, item: &LibraryItem, width: f32, height: f32) -> CardHit {
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
        let painter = ui.painter().clone();

        Self::paint_art(ui, rect, item, ArtworkKind::Poster, theme::CARD_RADIUS);

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
                egui::RichText::new(reel_core::title::truncate(&item.entry.title(), 22))
                    .size(13.5)
                    .color(Color32::WHITE),
            )
            .selectable(false),
        );

        // Second line: the year, or a resume hint once there is one.
        let subtitle = match item.resume_position() {
            Some(position) => format!("\u{25b6} resume from {}", fmt::human_duration(position)),
            None => match item.entry.year() {
                Some(year) => year.to_string(),
                None => reel_core::title::truncate(item.info_hash(), 10),
            },
        };
        ui.put(
            Rect::from_min_size(
                egui::pos2(rect.left() + 8.0, rect.bottom() - 36.0),
                Vec2::new(rect.width() - 16.0, 16.0),
            ),
            egui::Label::new(
                egui::RichText::new(subtitle)
                    .size(10.5)
                    .color(if item.resume_position().is_some() {
                        theme::ACCENT
                    } else {
                        theme::TEXT_DIM
                    }),
            )
            .selectable(false),
        );

        // Download progress along the bottom edge.
        if item.torrent.stats.percent < 99.5 {
            let bar = Rect::from_min_size(
                egui::pos2(rect.left(), rect.bottom() - 4.0),
                Vec2::new(rect.width(), 4.0),
            );
            let mut filled = bar;
            filled.set_right(bar.left() + bar.width() * (item.torrent.stats.percent as f32 / 100.0));
            painter.rect_filled(bar, CornerRadius::ZERO, Color32::from_black_alpha(150));
            painter.rect_filled(filled, CornerRadius::ZERO, theme::OK);
        }

        painter.circle_filled(
            egui::pos2(rect.right() - 14.0, rect.top() + 14.0),
            5.0,
            theme::state_color(&item.torrent.stats.state),
        );

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

        if self.library.is_empty() {
            self.empty_library(ui);
            return;
        }

        // When filtering, show one flat grid: rows would only fragment the
        // matches.
        if !filter.is_empty() {
            let matches: Vec<LibraryItem> = self
                .library
                .iter()
                .filter(|item| {
                    let haystack = format!(
                        "{} {}",
                        item.entry.title(),
                        item.entry.display_title
                    )
                    .to_lowercase();
                    haystack.contains(&filter)
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
            if let Some(featured) = self.library.first().cloned() {
                self.hero(ui, &featured);
                ui.add_space(16.0);
            }

            for row in self.rows.clone() {
                let items: Vec<LibraryItem> = row
                    .ids
                    .iter()
                    .filter_map(|id| self.library.iter().find(|i| i.torrent.id == *id).cloned())
                    .collect();
                if items.is_empty() {
                    continue;
                }

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&row.title).size(16.0).strong());
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(plural(items.len(), "title"))
                            .size(11.0)
                            .color(theme::TEXT_DIM),
                    );
                });
                ui.add_space(6.0);
                self.row_strip(ui, &items, row.kind);
                ui.add_space(20.0);
            }
        });
    }

    /// A horizontally scrolling strip of posters.
    fn row_strip(&mut self, ui: &mut egui::Ui, items: &[LibraryItem], kind: RowKind) {
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
                    for item in items {
                        let hit = self.card(ui, item, width, height);
                        if hit.double_clicked {
                            play = Some(item.torrent.id);
                        } else if hit.clicked {
                            open = Some(item.torrent.id);
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
    fn card_grid(&mut self, ui: &mut egui::Ui, items: &[LibraryItem]) {
        const CARD_W: f32 = 168.0;
        // 2:3, the shape of a real poster, so artwork is not stretched.
        const CARD_H: f32 = 252.0;
        const GAP: f32 = 12.0;

        let available = ui.available_width();
        let per_row = (((available + GAP) / (CARD_W + GAP)).floor() as usize).max(1);
        let mut open: Option<usize> = None;
        let mut play: Option<usize> = None;

        for chunk in items.chunks(per_row) {
            ui.horizontal(|ui| {
                for item in chunk {
                    let hit = self.card(ui, item, CARD_W, CARD_H);
                    if hit.double_clicked {
                        play = Some(item.torrent.id);
                    } else if hit.clicked {
                        open = Some(item.torrent.id);
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
    fn hero(&mut self, ui: &mut egui::Ui, item: &LibraryItem) {
        let height = 240.0;
        let width = ui.available_width();
        let (rect, hero_response) =
            ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
        let painter = ui.painter().clone();

        Self::paint_art(ui, rect, item, ArtworkKind::Backdrop, theme::PANEL_RADIUS);

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
                egui::RichText::new(reel_core::title::truncate(&item.heading(), 52))
                    .size(30.0)
                    .color(Color32::WHITE),
            )
            .halign(egui::Align::LEFT)
            .selectable(false),
        );

        // Metadata line: genres and rating when we have them, otherwise the
        // transfer state.
        let meta = match item.entry.metadata.as_ref() {
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
                parts.join("   \u{2022}   ")
            }
            None => format!(
                "{}   \u{2022}   {} peers   \u{2022}   {:.0}% downloaded",
                item.torrent.stats.state,
                item.torrent.stats.peers.live,
                item.torrent.stats.percent
            ),
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

        let label = match item.resume_position() {
            Some(position) => format!("\u{25b6}  Resume {}", fmt::human_duration(position)),
            None if item.torrent.finished => "\u{25b6}  Play".to_string(),
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

        if play {
            self.play_with_ctx(item.torrent.id);
        } else if hero_response.clicked() {
            self.screen = Screen::Detail(item.torrent.id);
        }
    }

    fn play_with_ctx(&mut self, id: usize) {
        let Some(ctx) = self.pending_ctx.clone() else {
            self.warn("Playback is not ready yet");
            return;
        };
        let Some(item) = self.item(id) else { return };

        let file_id = item
            .torrent
            .primary_file_id
            .or_else(|| item.torrent.files.iter().find(|f| f.is_video).map(|f| f.id));

        match file_id {
            Some(file_id) => self.play_file(&ctx, id, file_id),
            None => self.warn("This torrent has no playable file yet"),
        }
    }
}

// ------------------------------------------------------------------ detail

impl App {
    fn detail_screen(&mut self, ui: &mut egui::Ui) {
        let Some(item) = self.selected() else {
            ui.label("That torrent is no longer in the library.");
            if ui.button("Back to library").clicked() {
                self.screen = Screen::Library;
            }
            return;
        };

        let mut action: Option<DetailAction> = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("\u{2b05}  Library").clicked() {
                    action = Some(DetailAction::Navigate(Screen::Library));
                }
                ui.label(
                    egui::RichText::new(format!("torrent #{}", item.torrent.id))
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
            Self::paint_art(ui, header, &item, ArtworkKind::Backdrop, theme::PANEL_RADIUS);
            let mut scrim = header;
            scrim.set_left(header.left() + header.width() * 0.4);
            painter.rect_filled(scrim, theme::PANEL_RADIUS, Color32::from_black_alpha(150));

            ui.add_space(-header_height + 24.0);
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                let poster_size = Vec2::new(160.0, 230.0);
                let (poster, _) = ui.allocate_exact_size(poster_size, Sense::hover());
                Self::paint_art(ui, poster, &item, ArtworkKind::Poster, theme::CARD_RADIUS);

                ui.add_space(20.0);
                ui.vertical(|ui| {
                    ui.add_space(40.0);
                    ui.label(
                        egui::RichText::new(item.heading())
                            .size(28.0)
                            .strong()
                            .color(Color32::WHITE),
                    );

                    if let Some(metadata) = item.entry.metadata.as_ref() {
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
                        facts.push(fmt::human_bytes(item.torrent.stats.total_bytes));
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
                    } else {
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

            let playable = item
                .torrent
                .primary_file_id
                .or_else(|| item.torrent.files.iter().find(|f| f.is_video).map(|f| f.id));

            ui.horizontal(|ui| {
                let resume = item.resume_position();
                let label = match resume {
                    Some(position) => format!("\u{25b6}  Resume {}", fmt::human_duration(position)),
                    None => "\u{25b6}  Play".to_string(),
                };
                if ui
                    .add_enabled(
                        playable.is_some(),
                        egui::Button::new(egui::RichText::new(label).size(15.0)),
                    )
                    .clicked()
                {
                    if let Some(file_id) = playable {
                        action = Some(DetailAction::Play(file_id));
                    }
                }

                if resume.is_some() && ui.button("Start over").clicked() {
                    if let Some(file_id) = playable {
                        action = Some(DetailAction::PlayFrom(file_id, 0.0));
                    }
                }

                if ui.button("Mark watched").clicked() {
                    action = Some(DetailAction::MarkWatched(
                        item.entry.info_hash.clone(),
                        item.torrent.primary_file_id.unwrap_or(0),
                    ));
                }

                let pause_label = if item.torrent.stats.is_paused() {
                    "\u{25b6}  Resume download"
                } else {
                    "\u{23f8}  Pause download"
                };
                if ui
                    .add_enabled(!item.torrent.finished, egui::Button::new(pause_label))
                    .clicked()
                {
                    action = Some(DetailAction::SetPaused(!item.torrent.stats.is_paused()));
                }

                if ui.button("\u{1f5d1}  Remove").clicked() {
                    action = Some(DetailAction::ConfirmRemove);
                }

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
            });

            ui.add_space(14.0);

            if let Some(metadata) = item.entry.metadata.as_ref() {
                if let Some(overview) = metadata.overview.as_deref() {
                    ui.label(egui::RichText::new(overview).size(13.5).color(theme::TEXT));
                    ui.add_space(12.0);
                }
            }

            ui.add(
                egui::ProgressBar::new((item.torrent.stats.percent / 100.0) as f32)
                    .desired_width(440.0)
                    .fill(theme::ACCENT)
                    .text(format!("{:.1}%", item.torrent.stats.percent)),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                stat(ui, "downloaded", &fmt::human_bytes(item.torrent.stats.progress_bytes));
                stat(ui, "down", &fmt::human_rate(item.torrent.stats.download_bps));
                stat(ui, "up", &fmt::human_rate(item.torrent.stats.upload_bps));
                stat(ui, "peers", &item.torrent.stats.peers.live.to_string());
                stat(ui, "eta", &fmt::human_eta(item.torrent.stats.eta_seconds));
            });

            if let Some(error) = item.torrent.stats.error.as_deref() {
                ui.add_space(6.0);
                ui.colored_label(theme::DANGER, error);
            }

            ui.add_space(16.0);
            ui.separator();
            ui.add_space(10.0);

            // A series is episodes; a film is a file. Showing a film's file list
            // is fine. Showing one for a series means asking the user to read
            // release names to find episode three.
            if item.entry.is_series() {
                self.episode_list(ui, &item, &mut action);
            } else {
                self.file_list(ui, &item, &mut action);
            }

            self.storage_line(ui, &item);
        });

        match action {
            Some(DetailAction::Navigate(screen)) => self.screen = screen,
            Some(DetailAction::SetPaused(paused)) => self.backend.set_paused(item.torrent.id, paused),
            Some(DetailAction::ConfirmRemove) => self.show_delete_confirm = Some(item.torrent.id),
            Some(DetailAction::SelectFiles(files)) | Some(DetailAction::FetchAll(files)) => {
                self.select_files(item.torrent.id, &files);
            }
            Some(DetailAction::MarkWatched(hash, file_id)) => {
                self.backend
                    .mark_finished(&hash, file_id, Some(item.entry.title()));
                self.refresh();
            }
            Some(DetailAction::Play(file_id)) => {
                let ctx = self.pending_ctx.clone();
                match ctx {
                    Some(ctx) => self.play_file(&ctx, item.torrent.id, file_id),
                    None => self.warn("Playback is not ready yet"),
                }
            }
            Some(DetailAction::PlayFrom(file_id, position)) => {
                self.backend.forget_watch(item.info_hash(), file_id);
                self.refresh();
                let ctx = self.pending_ctx.clone();
                if let Some(ctx) = ctx {
                    self.play_file(&ctx, item.torrent.id, file_id);
                    if position > 0.0 {
                        self.player.seek_relative(position);
                    }
                }
            }
            None => {}
        }
    }
    /// The film view: the files themselves.
    /// The film view: the files themselves.
    ///
    /// A film is one video and possibly some extras, so a plain list is the
    /// honest presentation. The file-selection controls still apply, because a
    /// film release often carries a sample that should not be fetched.
    fn file_list(&mut self, ui: &mut egui::Ui, item: &LibraryItem, action: &mut Option<DetailAction>) {
        let included: Vec<usize> = item
            .torrent
            .files
            .iter()
            .filter(|f| f.included)
            .map(|f| f.id)
            .collect();
        let narrowed = included.len() < item.torrent.files.len();

        ui.label(
            egui::RichText::new(format!("Files ({})", item.torrent.files.len()))
                .size(16.0)
                .strong(),
        );
        ui.add_space(6.0);

        if narrowed {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "Fetching {} of {} files",
                        included.len(),
                        item.torrent.files.len()
                    ))
                    .size(12.0)
                    .color(theme::ACCENT),
                );
                if ui.small_button("Fetch every file").clicked() {
                    *action = Some(DetailAction::FetchAll(
                        item.torrent.files.iter().map(|f| f.id).collect(),
                    ));
                }
            });
            ui.add_space(6.0);
        }

        for file in &item.torrent.files {
            ui.horizontal(|ui| {
                let icon = if file.is_video {
                    "\u{1f3ac}"
                } else if file.is_audio {
                    "\u{1f3b5}"
                } else if file.is_subtitle {
                    "\u{1f4ac}"
                } else {
                    "\u{1f4c4}"
                };

                // Un-ticking the last remaining file would leave the torrent
                // with nothing to fetch, so that one is locked on.
                let mut wanted = file.included;
                let toggle = ui.add_enabled(
                    !(file.included && included.len() == 1),
                    egui::Checkbox::without_text(&mut wanted),
                );
                if toggle.changed() {
                    let mut next = included.clone();
                    if wanted {
                        next.push(file.id);
                        next.sort_unstable();
                    } else {
                        next.retain(|id| *id != file.id);
                    }
                    *action = Some(DetailAction::SelectFiles(next));
                }

                ui.label(icon);
                ui.label(
                    egui::RichText::new(reel_core::title::truncate(&file.path, 56)).color(
                        if file.included { theme::TEXT } else { theme::TEXT_DIM },
                    ),
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if media::is_media_file(&file.name) && ui.small_button("Play").clicked() {
                        *action = Some(DetailAction::Play(file.id));
                    }
                    if !file.included
                        && ui
                            .small_button("Download")
                            .on_hover_text("Fetch and keep this file as well")
                            .clicked()
                    {
                        let mut next = included.clone();
                        if !next.contains(&file.id) {
                            next.push(file.id);
                            next.sort_unstable();
                        }
                        *action = Some(DetailAction::SelectFiles(next));
                    }
                    ui.label(
                        egui::RichText::new(fmt::human_bytes(file.length)).color(theme::TEXT_DIM),
                    );
                    if file.included && file.progress_bytes > 0 {
                        let percent =
                            (file.progress_bytes as f64 / file.length.max(1) as f64) * 100.0;
                        ui.label(
                            egui::RichText::new(format!("{percent:.0}%"))
                                .size(11.0)
                                .color(theme::OK),
                        );
                    } else if !file.included {
                        ui.label(
                            egui::RichText::new("not fetching")
                                .color(theme::WARN)
                                .size(11.0),
                        );
                    }
                });
            });
            ui.separator();
        }

    }

    /// What is stored where, shown under either view.
    fn storage_line(&self, ui: &mut egui::Ui, item: &LibraryItem) {
        ui.add_space(6.0);
        let mut facts = vec![item.torrent.output_folder.clone()];
        if let Some(metadata) = item.entry.metadata.as_ref() {
            facts.push(format!("matched on {}", metadata.source));
        }
        let attributes = item.entry.attributes().summary();
        if !attributes.is_empty() {
            facts.push(attributes.join(" "));
        }
        ui.label(
            egui::RichText::new(facts.join("   \u{2022}   "))
                .color(theme::TEXT_DIM)
                .size(11.0),
        );
    }

    /// The series view: one row per episode, joined to the file that holds it.
    ///
    /// Rows are resolved to a season and episode number first, and only then
    /// rendered, because there are two ways a file can say which episode it is:
    /// in its name, or by its air date for a show that numbers by date.
    fn episode_list(
        &mut self,
        ui: &mut egui::Ui,
        item: &LibraryItem,
        action: &mut Option<DetailAction>,
    ) {
        let metadata = item.entry.metadata.as_ref();
        let release = &item.entry.release;

        // Resolve every file to an episode, then order by season and number.
        let mut rows: Vec<ResolvedEpisode<'_>> = release
            .episodes
            .iter()
            .filter(|file| !file.extra)
            .map(|file| {
                // A dated show: the provider's episode list is the only place a
                // date becomes a season and episode number.
                let by_date = file.air_date.as_deref().and_then(|date| {
                    metadata.and_then(|m| {
                        m.episodes
                            .iter()
                            .find(|episode| episode.air_date.as_deref() == Some(date))
                    })
                });

                let season = by_date.map(|e| e.season).or(file.season);
                let number = file.episode.or_else(|| by_date.map(|e| e.number));
                let info = match (season, number) {
                    (Some(season), Some(number)) => metadata.and_then(|m| m.episode(season, number)),
                    _ => by_date,
                };

                ResolvedEpisode {
                    file,
                    season,
                    number,
                    info,
                }
            })
            .collect();

        rows.sort_by_key(|row| {
            (
                row.season.unwrap_or(u32::MAX),
                row.number.unwrap_or(u32::MAX),
                row.file.file_id,
            )
        });

        let seasons: Vec<u32> = {
            let mut list: Vec<u32> = rows.iter().filter_map(|row| row.season).collect();
            list.sort_unstable();
            list.dedup();
            list
        };

        // Header: what this is, and how much of it is here.
        let title = metadata.map(|m| m.title.clone());
        let mut heading = match (&title, seasons.as_slice()) {
            (Some(title), [season]) => format!("{title} \u{2014} Season {season}"),
            (Some(title), _) => title.clone(),
            (None, [season]) => format!("Season {season}"),
            (None, _) => "Episodes".to_string(),
        };
        let numbered = rows.iter().filter(|row| row.number.is_some()).count();
        if numbered > 0 {
            heading.push_str(&format!(
                "  \u{2022}  {numbered} episode{}",
                if numbered == 1 { "" } else { "s" }
            ));
        } else if release.is_season_pack {
            heading.push_str("  \u{2022}  season pack");
        }
        if seasons.len() > 1 {
            heading.push_str(&format!("  \u{2022}  {} seasons", seasons.len()));
        }
        if release.is_dated() {
            heading.push_str("  \u{2022}  numbered by air date");
        }

        ui.label(egui::RichText::new(heading).size(16.0).strong());
        ui.add_space(2.0);
        if let Some(overview) = metadata.and_then(|m| m.overview.as_deref()) {
            ui.label(
                egui::RichText::new(reel_core::title::truncate(overview, 160))
                    .size(12.0)
                    .color(theme::TEXT_DIM),
            );
        }
        ui.add_space(8.0);

        let included: Vec<usize> = item
            .torrent
            .files
            .iter()
            .filter(|f| f.included)
            .map(|f| f.id)
            .collect();
        let narrowed = included.len() < item.torrent.files.len();
        if narrowed {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "Fetching {} of {} files",
                        included.len(),
                        item.torrent.files.len()
                    ))
                    .size(12.0)
                    .color(theme::ACCENT),
                );
                if ui.small_button("Fetch every episode").clicked() {
                    *action = Some(DetailAction::FetchAll(
                        item.torrent.files.iter().map(|f| f.id).collect(),
                    ));
                }
            });
            ui.add_space(6.0);
        }

        // File ids per season, so a season heading can offer a one-click
        // download without walking the rows again.
        let mut season_files: std::collections::BTreeMap<u32, Vec<usize>> =
            std::collections::BTreeMap::new();
        for row in &rows {
            if let Some(season) = row.season {
                season_files.entry(season).or_default().push(row.file.file_id);
            }
        }

        let mut current_season: Option<u32> = None;
        for row in rows {
            // A season heading, but only when there is more than one to tell
            // apart: one season does not need a label saying so.
            if seasons.len() > 1 && row.season != current_season {
                if let Some(season) = row.season {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("Season {season}"))
                                .size(13.0)
                                .strong()
                                .color(theme::ACCENT),
                        );
                        if ui
                            .small_button("Download season")
                            .on_hover_text("Fetch and keep every episode in this season")
                            .clicked()
                        {
                            let mut next = included.clone();
                            if let Some(ids) = season_files.get(&season) {
                                for id in ids {
                                    if !next.contains(id) {
                                        next.push(*id);
                                    }
                                }
                            }
                            next.sort_unstable();
                            *action = Some(DetailAction::SelectFiles(next));
                        }
                    });
                }
                current_season = row.season;
            }

            let file = item
                .torrent
                .files
                .iter()
                .find(|f| f.id == row.file.file_id);
            let (length, progress, is_included) = file
                .map(|f| (f.length, f.progress_bytes, f.included))
                .unwrap_or((row.file.length, 0, false));
            let watched = item.entry.watch_for_file(row.file.file_id);

            egui::Frame::NONE
                .fill(theme::SURFACE)
                .inner_margin(egui::Margin::symmetric(10, 8))
                .corner_radius(theme::CARD_RADIUS)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (thumb, _) =
                            ui.allocate_exact_size(Vec2::new(96.0, 54.0), Sense::hover());
                        if let Some(uri) = row.info.and_then(|i| i.still_uri()) {
                            egui::Image::new(uri)
                                .corner_radius(CornerRadius::same(4))
                                .paint_at(ui, thumb);
                        } else {
                            ui.painter().rect_filled(
                                thumb,
                                CornerRadius::same(4),
                                theme::SURFACE_RAISED,
                            );
                            ui.painter().text(
                                thumb.center(),
                                egui::Align2::CENTER_CENTER,
                                row.number
                                    .map(|n| format!("E{n:02}"))
                                    .unwrap_or_else(|| "?".to_string()),
                                egui::FontId::proportional(14.0),
                                theme::TEXT_DIM,
                            );
                        }

                        ui.add_space(6.0);
                        ui.vertical(|ui| {
                            let code = match (row.season, row.number) {
                                (Some(season), Some(number)) => {
                                    format!("S{season:02}E{number:02}")
                                }
                                (_, Some(number)) => format!("Episode {number}"),
                                // A season pack with no per-file numbering, or a
                                // dated show whose date matched nothing.
                                (Some(season), None) => format!("S{season:02} (unnumbered)"),
                                (None, None) => row
                                    .file
                                    .air_date
                                    .clone()
                                    .unwrap_or_else(|| "Unnumbered".to_string()),
                            };
                            let name = row
                                .info
                                .and_then(|i| i.name.clone())
                                .unwrap_or_else(|| "(no title found)".to_string());
                            ui.label(
                                egui::RichText::new(format!("{code}  {name}"))
                                    .size(13.5)
                                    .strong(),
                            );

                            let mut facts = Vec::new();
                            if let Some(runtime) = row.info.and_then(|i| i.runtime_minutes) {
                                facts.push(format!("{runtime}m"));
                            }
                            if let Some(air) = row.info.and_then(|i| i.air_date.as_deref()) {
                                facts.push(air.to_string());
                            }
                            facts.push(fmt::human_bytes(length));
                            if let Some(progress) = watched {
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
                        });

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Play").clicked() {
                                *action = Some(DetailAction::Play(row.file.file_id));
                            }
                            if !is_included
                                && ui
                                    .small_button("Download")
                                    .on_hover_text("Fetch and keep this episode as well")
                                    .clicked()
                            {
                                let mut next = included.clone();
                                if !next.contains(&row.file.file_id) {
                                    next.push(row.file.file_id);
                                    next.sort_unstable();
                                }
                                *action = Some(DetailAction::SelectFiles(next));
                            }
                            if is_included && progress > 0 {
                                let percent = (progress as f64 / length.max(1) as f64) * 100.0;
                                ui.label(
                                    egui::RichText::new(format!("{percent:.0}%"))
                                        .size(11.0)
                                        .color(theme::OK),
                                );
                            } else if !is_included {
                                ui.label(
                                    egui::RichText::new("not fetching")
                                        .size(11.0)
                                        .color(theme::WARN),
                                );
                            }

                            let mut wanted = is_included;
                            let toggle = ui.add_enabled(
                                !(is_included && included.len() == 1),
                                egui::Checkbox::without_text(&mut wanted),
                            );
                            if toggle.changed() {
                                let mut next = included.clone();
                                if wanted {
                                    next.push(row.file.file_id);
                                    next.sort_unstable();
                                } else {
                                    next.retain(|id| *id != row.file.file_id);
                                }
                                *action = Some(DetailAction::SelectFiles(next));
                            }
                        });
                    });
                });
            ui.add_space(4.0);
        }

        // Extras and anything unnumbered, listed separately so they cannot be
        // mistaken for episodes.
        let leftovers: Vec<&reel_catalog::release::EpisodeFile> = release
            .episodes
            .iter()
            .filter(|e| e.extra)
            .collect();
        if !leftovers.is_empty() {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("Extras")
                    .size(13.0)
                    .color(theme::TEXT_DIM),
            );
            for extra in leftovers {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(reel_core::title::truncate(&extra.path, 60))
                            .size(12.0)
                            .color(theme::TEXT_DIM),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(fmt::human_bytes(extra.length))
                                .size(11.0)
                                .color(theme::TEXT_DIM),
                        );
                    });
                });
            }
        }
    }

}

/// A file resolved to an episode, by name or by air date.
struct ResolvedEpisode<'a> {
    file: &'a reel_catalog::release::EpisodeFile,
    season: Option<u32>,
    number: Option<u32>,
    info: Option<&'a reel_catalog::model::EpisodeInfo>,
}

enum DetailAction {
    Navigate(Screen),
    SetPaused(bool),
    ConfirmRemove,
    MarkWatched(String, usize),
    Play(usize),
    PlayFrom(usize, f64),
    /// Fetch exactly these files.
    SelectFiles(Vec<usize>),
    FetchAll(Vec<usize>),
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
        self.watch_recording_for = None;
        self.watch_last_recorded = 0.0;
        self.screen = self
            .player_origin
            .map(Screen::Detail)
            .unwrap_or(Screen::Library);
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
    fn sidecar_subtitles_are_matched_to_their_video() {
        let item = sample_library()
            .into_iter()
            .find(|item| item.torrent.id == 1)
            .expect("the matrix");
        let video = item.torrent.files.iter().find(|f| f.id == 0).expect("video");
        let subs = companion_subtitles(&item.torrent.files, video);
        assert_eq!(subs, vec![2], "The.Matrix.en.srt belongs to the film");
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
