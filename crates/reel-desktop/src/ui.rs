//! The application: navigation, the catalogue grid, the detail page, the add
//! form, settings, and the player screen.
//!
//! Rendering is pure egui — immediate mode, GPU-rendered, no webview. All
//! engine access goes through [`Backend`], so the same UI runs against a real
//! engine or an in-memory fake in tests.

use std::time::{Duration, Instant};

use egui::{Color32, CornerRadius, Sense, Vec2};
use reel_core::model::TorrentView;
use reel_core::title::clean_title;
use reel_core::{fmt, media};

use crate::backend::{Backend, BackendEvent, PlayerCapability};
use crate::player::{PlaybackInfo, PlaybackStats, PlayerController};
use crate::theme;

/// Which page is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Library,
    Detail(usize),
    Player,
    Add,
    Settings,
}

pub struct App {
    backend: Box<dyn Backend>,
    screen: Screen,
    /// Cached torrent list, refreshed periodically while browsing.
    library: Vec<TorrentView>,
    last_refresh: Instant,
    toast: Option<Toast>,
    add_source: String,
    add_media_only: bool,
    filter: String,
    player: PlayerController,
    /// Torrent id the player was started from, to return to.
    player_origin: Option<usize>,
    show_delete_confirm: Option<usize>,
    /// The egui context for the current frame, so UI actions triggered deep in
    /// the widget tree can still start playback.
    pending_ctx: Option<egui::Context>,
}

struct Toast {
    message: String,
    error: bool,
    shown: Instant,
}

const REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const TOAST_LIFETIME: Duration = Duration::from_secs(6);

impl App {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        let mut app = Self {
            backend,
            screen: Screen::Library,
            library: Vec::new(),
            last_refresh: Instant::now() - REFRESH_INTERVAL,
            toast: None,
            add_source: String::new(),
            add_media_only: true,
            filter: String::new(),
            player: PlayerController::new(),
            player_origin: None,
            show_delete_confirm: None,
            pending_ctx: None,
        };
        app.refresh();
        app
    }

    /// The screen currently shown. Exposed for tests.
    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// Number of torrents in the cached library. Exposed for tests.
    pub fn library_len(&self) -> usize {
        self.library.len()
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

    pub fn play_file(&mut self, ctx: &egui::Context, torrent_id: usize, file_id: usize) {
        let Some(view) = self.backend.view(torrent_id) else {
            self.warn("That torrent is no longer in the library");
            return;
        };
        let Some(file) = view.files.iter().find(|f| f.id == file_id) else {
            self.warn("That file is no longer in the torrent");
            return;
        };

        let clean = clean_title(view.name.as_deref().unwrap_or(&view.info_hash));
        let info = PlaybackInfo {
            torrent_id,
            title: clean.display(),
            file_name: file.name.clone(),
            fallback_duration: None,
        };

        let url = file
            .stream
            .url
            .clone()
            .unwrap_or_else(|| format!("{}{}", self.backend.base_url(), file.stream.path));

        let capability = self.backend.capabilities().player.clone();
        self.player_origin = Some(torrent_id);

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

    fn refresh(&mut self) {
        self.library = self.backend.torrents();
        self.last_refresh = Instant::now();
    }

    fn maybe_refresh(&mut self) {
        // The player does not need the library list, and the detail page wants
        // fresh numbers, so both refresh on the same cadence.
        if self.screen != Screen::Settings && self.last_refresh.elapsed() >= REFRESH_INTERVAL {
            self.refresh();
        }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        for event in self.backend.take_events() {
            match event {
                BackendEvent::Info(message) => {
                    self.toast = Some(Toast { message, error: false, shown: Instant::now() });
                }
                BackendEvent::Error(message) => {
                    self.toast = Some(Toast { message, error: true, shown: Instant::now() });
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
            }
            ctx.request_repaint();
        }
    }

    fn set_toast(&mut self, message: impl Into<String>, error: bool) {
        self.toast = Some(Toast { message: message.into(), error, shown: Instant::now() });
    }

    fn warn(&mut self, message: impl Into<String>) {
        self.set_toast(message, true);
    }

    pub fn view(&self, id: usize) -> Option<TorrentView> {
        self.library.iter().find(|v| v.id == id).cloned()
    }

    fn selected(&self) -> Option<TorrentView> {
        match self.screen {
            Screen::Detail(id) => self.view(id),
            _ => None,
        }
    }

    /// Stats for the player overlay, taken from the engine rather than mpv.
    fn playback_stats(&self, torrent_id: usize) -> PlaybackStats {
        let Some(view) = self.view(torrent_id) else {
            return PlaybackStats::default();
        };
        let file_percent = match view.primary_file_id.and_then(|id| view.files.get(id)) {
            Some(file) if file.length > 0 => {
                // Report the whole-torrent percentage: the engine's per-file
                // progress is not exposed, and for a media-only add the two are
                // effectively the same.
                view.stats.percent
            }
            _ => view.stats.percent,
        };
        PlaybackStats {
            download_bps: view.stats.download_bps,
            peers: view.stats.peers.live,
            file_percent,
        }
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
                    .inner_margin(egui::Margin::symmetric(20, 16)),
            )
            .show(ui, |ui| match self.screen.clone() {
                Screen::Library => self.library_screen(ui),
                Screen::Detail(_) => self.detail_screen(ui),
                Screen::Add => self.add_screen(ui),
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

                // Aggregate transfer rate across every torrent.
                let (down, peers): (u64, u32) = self
                    .library
                    .iter()
                    .fold((0, 0), |(d, p), v| {
                        (d + v.stats.download_bps, p + v.stats.peers.live)
                    });
                if down > 0 {
                    ui.label(
                        egui::RichText::new(format!("{} \u{2b07}", fmt::human_rate(down)))
                            .color(theme::TEXT_DIM),
                    );
                }
                ui.label(
                    egui::RichText::new(format!("{} peers", peers)).color(theme::TEXT_DIM),
                );
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
                    ui.colored_label(color, "•");
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
            .view(id)
            .map(|v| clean_title(v.name.as_deref().unwrap_or(&v.info_hash)).display())
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
                        "Removing only forgets the torrent. Deleting also removes the \
                         downloaded files from disk.",
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

// ----------------------------------------------------------------- library

impl App {
    fn library_screen(&mut self, ui: &mut egui::Ui) {
        let filter = self.filter.trim().to_lowercase();
        let visible: Vec<TorrentView> = self
            .library
            .iter()
            .filter(|view| {
                if filter.is_empty() {
                    return true;
                }
                let name = view.name.clone().unwrap_or_default().to_lowercase();
                name.contains(&filter)
            })
            .cloned()
            .collect();

        if self.library.is_empty() {
            self.empty_library(ui);
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            // A hero banner for the most recently added torrent gives the page
            // a catalogue feel rather than a bare list.
            if let Some(featured) = visible.first().cloned() {
                self.hero(ui, &featured);
                ui.add_space(18.0);
            }

            let label = if filter.is_empty() {
                "In your library".to_string()
            } else {
                format!("Matching \"{}\"", self.filter.trim())
            };
            ui.label(egui::RichText::new(label).size(16.0).strong());
            ui.add_space(8.0);

            if visible.is_empty() {
                ui.label(
                    egui::RichText::new("Nothing matches that filter.").color(theme::TEXT_DIM),
                );
                return;
            }

            self.card_grid(ui, &visible);
        });
    }

    fn empty_library(&mut self, ui: &mut egui::Ui) {
        ui.add_space(60.0);
        ui.vertical_centered(|ui| {
            ui.label(egui::RichText::new("Your library is empty").size(24.0).strong());
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(
                    "Add a magnet link or a .torrent URL and reel will start \
                     fetching the video, streaming it while it downloads.",
                )
                .color(theme::TEXT_DIM),
            );
            ui.add_space(20.0);
            if ui.button("  Add a torrent  ").clicked() {
                self.screen = Screen::Add;
            }
        });
    }

    fn hero(&mut self, ui: &mut egui::Ui, view: &TorrentView) {
        let clean = clean_title(view.name.as_deref().unwrap_or(&view.info_hash));
        let height = 190.0;
        let width = ui.available_width();
        let (rect, hero_response) =
            ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
        let painter = ui.painter().clone();

        let base = theme::poster_color(&clean.title);
        painter.rect_filled(rect, theme::PANEL_RADIUS, base);

        // A darker band on the right keeps the text side readable.
        let mut shade = rect;
        shade.set_left(rect.left() + rect.width() * 0.45);
        painter.rect_filled(shade, theme::PANEL_RADIUS, theme::poster_shade(&clean.title));

        let text_rect = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 22.0, rect.top() + 22.0),
            egui::pos2(rect.right() - 260.0, rect.bottom() - 18.0),
        );

        ui.put(
            egui::Rect::from_min_size(text_rect.left_top(), Vec2::new(90.0, 16.0)),
            egui::Label::new(
                egui::RichText::new("LATEST").size(11.0).strong().color(theme::ACCENT),
            )
            .selectable(false),
        );
        ui.put(
            egui::Rect::from_min_size(
                egui::pos2(text_rect.left(), text_rect.top() + 20.0),
                Vec2::new(text_rect.width(), 38.0),
            ),
            egui::Label::new(
                egui::RichText::new(reel_core::title::truncate(&clean.display(), 46))
                    .size(28.0)
                    .color(Color32::WHITE),
            )
            .selectable(false),
        );

        let resume = ui
            .put(
                egui::Rect::from_min_size(
                    egui::pos2(rect.right() - 232.0, rect.bottom() - 60.0),
                    Vec2::new(200.0, 34.0),
                ),
                egui::Button::new(
                    egui::RichText::new(if view.finished {
                        "\u{25b6}  Play"
                    } else {
                        "\u{25b6}  Resume"
                    })
                    .size(15.0),
                ),
            )
            .clicked();

        let progress_rect = egui::Rect::from_min_size(
            egui::pos2(text_rect.left(), rect.bottom() - 40.0),
            Vec2::new(text_rect.width(), 6.0),
        );
        let mut filled = progress_rect;
        filled.set_right(progress_rect.left() + progress_rect.width() * (view.stats.percent as f32 / 100.0));
        painter.rect_filled(progress_rect, CornerRadius::same(3), theme::BG);
        painter.rect_filled(filled, CornerRadius::same(3), theme::ACCENT);
        painter.text(
            egui::pos2(text_rect.left(), rect.bottom() - 62.0),
            egui::Align2::LEFT_BOTTOM,
            format!(
                "{} of {}  \u{2022}  {}  \u{2022}  {} peers",
                fmt::human_bytes(view.stats.progress_bytes),
                fmt::human_bytes(view.stats.total_bytes),
                view.state,
                view.stats.peers.live
            ),
            egui::FontId::proportional(12.0),
            theme::TEXT_DIM,
        );

        if resume {
            self.play_primary(view.id);
        } else if hero_response.clicked() {
            self.screen = Screen::Detail(view.id);
        }
    }

    fn play_primary(&mut self, id: usize) {
        let Some(view) = self.view(id) else { return };
        let file_id = view
            .primary_file_id
            .or_else(|| view.files.iter().find(|f| f.is_video).map(|f| f.id));
        match file_id {
            Some(file_id) => {
                let ctx = self.pending_ctx.clone();
                match ctx {
                    Some(ctx) => self.play_file(&ctx, id, file_id),
                    None => self.warn("Playback is not ready yet"),
                }
            }
            None => self.warn("This torrent has no playable file"),
        }
    }

    fn card_grid(&mut self, ui: &mut egui::Ui, views: &[TorrentView]) {
        const CARD_W: f32 = 178.0;
        const CARD_H: f32 = 250.0;
        const GAP: f32 = 14.0;

        let available = ui.available_width();
        let per_row = (((available + GAP) / (CARD_W + GAP)).floor() as usize).max(1);
        let mut clicked: Option<usize> = None;
        let mut play: Option<usize> = None;

        for row in views.chunks(per_row) {
            ui.horizontal(|ui| {
                for view in row {
                    let (rect, response) =
                        ui.allocate_exact_size(Vec2::new(CARD_W, CARD_H), Sense::click());
                    let painter = ui.painter().clone();
                    let clean = clean_title(view.name.as_deref().unwrap_or(&view.info_hash));

                    // Artwork: a stable colour per title until real posters
                    // arrive with the metadata layer.
                    painter.rect_filled(rect, theme::CARD_RADIUS, theme::poster_color(&clean.title));
                    let mut badge = rect;
                    badge.set_top(rect.bottom() - 74.0);
                    painter.rect_filled(badge, theme::CARD_RADIUS, theme::poster_shade(&clean.title));

                    painter.text(
                        rect.center() - Vec2::new(0.0, 26.0),
                        egui::Align2::CENTER_CENTER,
                        reel_core::title::initials(&clean.title),
                        egui::FontId::proportional(46.0),
                        Color32::from_white_alpha(38),
                    );

                    // Title and meta as real labels rather than painted text, so
                    // they are visible to accessibility tools (and to tests).
                    ui.put(
                        egui::Rect::from_min_size(
                            egui::pos2(rect.left() + 10.0, rect.bottom() - 64.0),
                            Vec2::new(rect.width() - 20.0, 20.0),
                        ),
                        egui::Label::new(
                            egui::RichText::new(reel_core::title::truncate(&clean.title, 20))
                                .size(14.0)
                                .color(Color32::WHITE),
                        )
                        .selectable(false),
                    );
                    ui.put(
                        egui::Rect::from_min_size(
                            egui::pos2(rect.left() + 10.0, rect.bottom() - 46.0),
                            Vec2::new(rect.width() - 20.0, 16.0),
                        ),
                        egui::Label::new(
                            egui::RichText::new(match clean.year {
                                Some(year) => format!("{year}"),
                                None => reel_core::title::truncate(&view.info_hash, 10),
                            })
                            .size(11.0)
                            .color(theme::TEXT_DIM),
                        )
                        .selectable(false),
                    );

                    // Progress bar.
                    let bar = egui::Rect::from_min_size(
                        egui::pos2(rect.left() + 10.0, rect.bottom() - 26.0),
                        Vec2::new(rect.width() - 20.0, 5.0),
                    );
                    let mut filled = bar;
                    filled.set_right(bar.left() + bar.width() * (view.stats.percent as f32 / 100.0));
                    painter.rect_filled(bar, CornerRadius::same(2), theme::BG);
                    painter.rect_filled(filled, CornerRadius::same(2), theme::OK);

                    // State dot.
                    painter.circle_filled(
                        egui::pos2(rect.right() - 16.0, rect.top() + 16.0),
                        5.0,
                        theme::state_color(&view.state),
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

                    if response.clicked() {
                        clicked = Some(view.id);
                    }
                    // Double click plays, matching the rest of the app.
                    if response.double_clicked() {
                        play = Some(view.id);
                    }
                }
            });
            ui.add_space(GAP);
        }

        if let Some(id) = clicked {
            if play.is_none() {
                self.screen = Screen::Detail(id);
            }
        }
        if let Some(id) = play {
            self.play_with_ctx(id);
        }
    }

    fn play_with_ctx(&mut self, id: usize) {
        let ctx = self.pending_ctx.clone();
        match ctx {
            Some(ctx) => {
                let Some(view) = self.view(id) else { return };
                match view.primary_file_id {
                    Some(file_id) => self.play_file(&ctx, id, file_id),
                    None => self.warn("This torrent has no playable file yet"),
                }
            }
            None => self.warn("Playback is not ready yet"),
        }
    }
}

// ------------------------------------------------------------------ detail

impl App {
    fn detail_screen(&mut self, ui: &mut egui::Ui) {
        let Some(view) = self.selected() else {
            ui.label("That torrent is no longer in the library.");
            if ui.button("Back to library").clicked() {
                self.screen = Screen::Library;
            }
            return;
        };

        let clean = clean_title(view.name.as_deref().unwrap_or(&view.info_hash));
        let mut action: Option<DetailAction> = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("\u{2b05}  Library").clicked() {
                    action = Some(DetailAction::Navigate(Screen::Library));
                }
                ui.label(egui::RichText::new(format!("torrent #{}", view.id)).color(theme::TEXT_DIM));
            });

            ui.add_space(10.0);

            ui.horizontal(|ui| {
                // Poster
                let (rect, _) = ui.allocate_exact_size(Vec2::new(190.0, 268.0), Sense::hover());
                let painter = ui.painter().clone();
                painter.rect_filled(rect, theme::CARD_RADIUS, theme::poster_color(&clean.title));
                painter.text(
                    rect.center() - Vec2::new(0.0, 20.0),
                    egui::Align2::CENTER_CENTER,
                    reel_core::title::initials(&clean.title),
                    egui::FontId::proportional(52.0),
                    Color32::from_white_alpha(40),
                );

                ui.add_space(18.0);

                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(clean.display()).size(26.0).strong());
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "{}  \u{2022}  {}  \u{2022}  {}",
                            view.state,
                            fmt::human_bytes(view.stats.total_bytes),
                            reel_core::title::truncate(&view.info_hash, 16)
                        ))
                        .color(theme::TEXT_DIM),
                    );

                    ui.add_space(12.0);
                    ui.add(
                        egui::ProgressBar::new((view.stats.percent / 100.0) as f32)
                            .desired_width(420.0)
                            .fill(theme::ACCENT)
                            .text(format!("{:.1}%", view.stats.percent)),
                    );

                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        stat(ui, "downloaded", &fmt::human_bytes(view.stats.progress_bytes));
                        stat(ui, "down", &fmt::human_rate(view.stats.download_bps));
                        stat(ui, "up", &fmt::human_rate(view.stats.upload_bps));
                        stat(ui, "peers", &view.stats.peers.live.to_string());
                        stat(ui, "eta", &fmt::human_eta(view.stats.eta_seconds));
                    });

                    if let Some(error) = view.stats.error.as_deref() {
                        ui.add_space(8.0);
                        ui.colored_label(theme::DANGER, error);
                    }

                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        let playable = view.primary_file_id.or_else(|| {
                            view.files.iter().find(|f| f.is_video).map(|f| f.id)
                        });
                        let enabled = playable.is_some();

                        if ui
                            .add_enabled(
                                enabled,
                                egui::Button::new(egui::RichText::new("\u{25b6}  Play").size(15.0)),
                            )
                            .clicked()
                        {
                            if let Some(file_id) = playable {
                                action = Some(DetailAction::Play(file_id));
                            }
                        }

                        let pause_label = if view.stats.is_paused() { "\u{25b6}  Resume" } else { "\u{23f8}  Pause" };
                        if ui.add_enabled(!view.finished, egui::Button::new(pause_label)).clicked() {
                            action = Some(DetailAction::SetPaused(!view.stats.is_paused()));
                        }

                        if ui.button("\u{1f5d1}  Remove").clicked() {
                            action = Some(DetailAction::ConfirmRemove);
                        }

                        if let Some(path) = view.primary_file() {
                            let url = path
                                .stream
                                .url
                                .clone()
                                .unwrap_or_else(|| path.stream.path.clone());
                            if ui
                                .button("\u{1f4cb}  Copy stream URL")
                                .on_hover_text(url.clone())
                                .clicked()
                            {
                                ui.ctx().copy_text(url);
                            }
                        }
                    });
                });
            });

            ui.add_space(20.0);
            ui.separator();
            ui.add_space(10.0);

            ui.label(egui::RichText::new(format!("Files ({})", view.files.len())).size(16.0).strong());
            ui.add_space(6.0);

            for file in &view.files {
                let row = ui.horizontal(|ui| {
                    let icon = if file.is_video {
                        "\u{1f3ac}"
                    } else if file.is_audio {
                        "\u{1f3b5}"
                    } else if file.is_subtitle {
                        "\u{1f4ac}"
                    } else {
                        "\u{1f4c4}"
                    };
                    ui.label(icon);
                    ui.label(
                        egui::RichText::new(reel_core::title::truncate(&file.path, 60))
                            .color(if file.included { theme::TEXT } else { theme::TEXT_DIM }),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if media::is_media_file(&file.name)
                            && ui.small_button("Play").clicked()
                        {
                            action = Some(DetailAction::Play(file.id));
                        }
                        ui.label(
                            egui::RichText::new(fmt::human_bytes(file.length))
                                .color(theme::TEXT_DIM),
                        );
                        if !file.included {
                            ui.label(
                                egui::RichText::new("skipped").color(theme::WARN).size(11.0),
                            );
                        }
                    });
                });
                let _ = row;
                ui.separator();
            }

            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(format!("Stored in {}", view.output_folder))
                    .color(theme::TEXT_DIM)
                    .size(11.0),
            );
        });

        match action {
            Some(DetailAction::Navigate(screen)) => self.screen = screen,
            Some(DetailAction::SetPaused(paused)) => self.backend.set_paused(view.id, paused),
            Some(DetailAction::ConfirmRemove) => self.show_delete_confirm = Some(view.id),
            Some(DetailAction::Play(file_id)) => {
                let ctx = self.pending_ctx.clone();
                match ctx {
                    Some(ctx) => self.play_file(&ctx, view.id, file_id),
                    None => self.warn("Playback is not ready yet"),
                }
            }
            None => {}
        }
    }
}

enum DetailAction {
    Navigate(Screen),
    SetPaused(bool),
    ConfirmRemove,
    Play(usize),
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
        ui.add_space(20.0);
        ui.label(egui::RichText::new("Add a torrent").size(22.0).strong());
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Paste a magnet link, an http(s) .torrent URL, or a 40-character info hash. \
                 reel fetches the metadata, picks the video, and starts streaming it.",
            )
            .color(theme::TEXT_DIM),
        );

        ui.add_space(16.0);
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

        if !self.library.is_empty() {
            ui.add_space(28.0);
            ui.label(egui::RichText::new("Already in your library").size(16.0).strong());
            ui.add_space(6.0);
            for view in self.library.clone() {
                let clean = clean_title(view.name.as_deref().unwrap_or(&view.info_hash));
                ui.horizontal(|ui| {
                    ui.label(reel_core::title::truncate(&clean.display(), 50));
                    if ui.small_button("Open").clicked() {
                        self.screen = Screen::Detail(view.id);
                    }
                });
            }
        }
    }
}

// ---------------------------------------------------------------- settings

impl App {
    fn settings_screen(&mut self, ui: &mut egui::Ui) {
        let caps = self.backend.capabilities().clone();

        ui.add_space(20.0);
        ui.label(egui::RichText::new("Settings").size(22.0).strong());
        ui.add_space(14.0);

        ui.label(egui::RichText::new("Library").size(16.0).strong());
        row(ui, "Download folder", &caps.download_dir);
        row(ui, "Streaming API", self.backend.base_url());
        row(ui, "Client name", &caps.client_name);
        ui.add_space(16.0);

        ui.label(egui::RichText::new("Playback").size(16.0).strong());
        match &caps.player {
            PlayerCapability::Embedded => {
                row(ui, "Backend", "libmpv (embedded in this window)");
                if let Some((major, minor)) = caps.mpv_api {
                    row(ui, "mpv client API", &format!("{major}.{minor}"));
                }
                ui.label(
                    egui::RichText::new(
                        "Video is decoded by libmpv and rendered into the app window. \
                         No browser, no webview.",
                    )
                    .color(theme::TEXT_DIM),
                );
            }
            PlayerCapability::External { program } => {
                row(ui, "Backend", &format!("external player ({program})"));
                if let Some(note) = caps.player_note.as_deref() {
                    ui.label(egui::RichText::new(note).color(theme::WARN));
                }
                ui.label(
                    egui::RichText::new(
                        "Embedded playback is unavailable, so video opens in a separate \
                         player window.",
                    )
                    .color(theme::TEXT_DIM),
                );
            }
            PlayerCapability::Unavailable { reason } => {
                row(ui, "Backend", "unavailable");
                ui.label(egui::RichText::new(reason).color(theme::DANGER));
            }
        }
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let mut volume = self.player.volume();
            ui.label("Volume");
            if ui
                .add(egui::Slider::new(&mut volume, 0.0..=130.0).suffix(" %"))
                .changed()
            {
                self.player.set_volume(volume);
            }
        });

        ui.add_space(16.0);
        ui.label(egui::RichText::new("About").size(16.0).strong());
        row(ui, "Version", env!("CARGO_PKG_VERSION"));
        ui.label(
            egui::RichText::new(
                "reel streams torrents over HTTP with byte-range seeking. The catalogue \
                 and metadata providers are the next milestone; artwork is currently \
                 generated from each title.",
            )
            .color(theme::TEXT_DIM),
        );
    }
}

fn row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            Vec2::new(150.0, 18.0),
            egui::Label::new(egui::RichText::new(label).color(theme::TEXT_DIM)),
        );
        ui.label(egui::RichText::new(value).monospace());
    });
    ui.add_space(2.0);
}

// ----------------------------------------------------------------- player

impl App {
    fn player_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Repaint continuously so the video keeps advancing even without input.
        ctx.request_repaint();

        let Some(info) = self.player.current().cloned() else {
            // Playback ended or was never started.
            self.screen = self
                .player_origin
                .map(Screen::Detail)
                .unwrap_or(Screen::Library);
            return;
        };

        egui::Panel::top("player-bar")
            .frame(
                egui::Frame::NONE
                    .fill(theme::SURFACE)
                    .inner_margin(egui::Margin::symmetric(14, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("\u{2b05}  Back").clicked() {
                        self.player.close();
                        self.screen = self
                            .player_origin
                            .map(Screen::Detail)
                            .unwrap_or(Screen::Library);
                    }
                    ui.label(egui::RichText::new(&info.title).size(15.0).strong());

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let backend = self.player.backend();
                        let label = match backend {
                            Some(reel_player::Backend::Embedded) => "libmpv (embedded)",
                            Some(reel_player::Backend::External) => "external player",
                            _ => "unknown backend",
                        };
                        ui.label(egui::RichText::new(label).size(11.0).color(theme::TEXT_DIM));
                    });
                });
            });

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
                                 separate player process. The picture is in that \
                                 program's own window.",
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
    use crate::testing::sample_torrents;

    fn app() -> App {
        App::new(Box::new(crate::backend::FakeBackend::new(
            sample_torrents(),
        )))
    }

    #[test]
    fn starts_on_the_library_with_torrents() {
        let app = app();
        assert_eq!(app.screen(), &Screen::Library);
        assert_eq!(app.library_len(), 3);
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
        assert!(app.view(1).expect("torrent 1").stats.is_paused());

        app.backend.remove(1, false);
        app.refresh();
        assert!(app.view(1).is_none());
        assert_eq!(app.library_len(), 2);
    }

    #[test]
    fn playable_files_are_detected() {
        let app = app();
        let view = app.view(1).expect("torrent 1");
        assert_eq!(view.primary_file_id, Some(0));
        assert!(view.primary_file().is_some());
    }
}
