//! The Settings screen.

use super::*;
use reel_catalog::ConversionMode;

// ---------------------------------------------------------------- settings

impl App {
    pub(crate) fn settings_screen(&mut self, ui: &mut egui::Ui) {
        let caps = self.backend.capabilities().clone();
        let catalog = self.backend.catalog_status();
        let mut action: Option<SettingsAction> = None;
        let mut check_key: Option<String> = None;
        let mut remove_key = false;

        // The editable copy, kept between frames so text fields hold their
        // buffer. It is written to disk automatically when it changes.
        let mut draft = self
            .settings_draft
            .take()
            .unwrap_or_else(|| self.backend.settings());

        let saved = draft == self.backend.settings();
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            page_title(ui, "Settings");
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(if saved { "saved" } else { "saving\u{2026}" })
                    .size(11.0)
                    .color(if saved { theme::TEXT_DIM } else { theme::WARN }),
            );
        });
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new("Saved to disk as you change them; there is no Save button.")
                .color(theme::TEXT_DIM)
                .size(11.5),
        );
        ui.add_space(10.0);

        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(190.0, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    for section in SettingsSection::ALL {
                        let selected = section == self.settings_section;
                        if ui
                            .add_sized(
                                [ui.available_width(), 30.0],
                                egui::Button::new(egui::RichText::new(section.label()).size(13.5))
                                    .selected(selected),
                            )
                            .clicked()
                        {
                            self.settings_section = section;
                        }
                    }
                },
            );
            ui.add_space(18.0);
            ui.separator();
            ui.add_space(18.0);
            ui.vertical(|ui| {
                egui::ScrollArea::vertical().show(ui, |ui| match self.settings_section {
                    SettingsSection::General => self.settings_general(ui, &mut draft, &caps),
                    SettingsSection::Metadata => {
                        self.settings_metadata(
                            ui,
                            &mut draft,
                            &catalog,
                            &mut action,
                            &mut check_key,
                            &mut remove_key,
                        );
                    }
                    SettingsSection::Playback => self.settings_playback(ui, &mut draft, &caps),
                    SettingsSection::Downloads => self.settings_downloads(ui, &mut draft),
                    SettingsSection::Network => self.settings_network(ui, &mut draft),
                    SettingsSection::Casting => self.settings_casting(ui, &mut draft),
                    SettingsSection::Search => self.settings_search(ui, &mut draft),
                    SettingsSection::About => self.settings_about(ui),
                });
            });
        });

        if remove_key {
            draft.tmdb_api_key = None;
        }

        // Autosave. Waiting for the pointer to come up means a slider drag
        // writes once, on release, rather than every frame.
        let pointer_down = ui.ctx().input(|i| i.pointer.any_down());
        if !pointer_down && draft != self.backend.settings() {
            draft.normalise();
            self.backend.save_settings(draft.clone());
            self.player
                .set_preferences(crate::player::PlaybackPreferences {
                    subtitles_enabled: draft.subtitles_enabled,
                    subtitle_language: draft.subtitle_language.clone(),
                    volume: draft.default_volume,
                });
            self.api_key_input.clear();
        }
        self.settings_draft = Some(draft);

        if let Some(key) = check_key {
            self.backend.test_api_key(key);
        }
        match action {
            Some(SettingsAction::RefreshMetadata) => self.backend.refresh_metadata(),
            Some(SettingsAction::ClearCache) => self.backend.clear_catalog_cache(),
            None => {}
        }
    }

    pub(crate) fn settings_general(
        &self,
        ui: &mut egui::Ui,
        draft: &mut CatalogSettings,
        caps: &crate::backend::BackendCapabilities,
    ) {
        section_header(ui, "General");
        ui.add_space(4.0);
        row(ui, "Active folder", &caps.download_dir);
        ui.horizontal(|ui| {
            ui.add_sized(
                Vec2::new(150.0, 18.0),
                egui::Label::new(egui::RichText::new("Download folder").color(theme::TEXT_DIM)),
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
        ui.add_space(6.0);
        row(ui, "Streaming API", self.backend.base_url());
        row(ui, "Client name", &caps.client_name);
        ui.add_space(6.0);
        ui.checkbox(
            &mut draft.merge_works,
            "Merge copies of the same film or show into one title",
        );
        ui.label(
            egui::RichText::new(
                "With this on, a 4K and a 1080p copy of a film are one title with a release \
                 list, and overlapping seasons or episodes of a show are merged with a \
                 per-episode choice. Turn it off to list every torrent separately.",
            )
            .color(theme::TEXT_DIM)
            .size(11.0),
        );
    }

    pub(crate) fn settings_metadata(
        &mut self,
        ui: &mut egui::Ui,
        draft: &mut CatalogSettings,
        catalog: &crate::backend::CatalogStatus,
        action: &mut Option<SettingsAction>,
        check_key: &mut Option<String>,
        remove_key: &mut bool,
    ) {
        section_header(ui, "Metadata and artwork");
        ui.add_space(4.0);
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

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_sized(
                Vec2::new(150.0, 18.0),
                egui::Label::new(egui::RichText::new("Metadata API key").color(theme::TEXT_DIM)),
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
            }
            if ui.add_enabled(typed, egui::Button::new("Check")).clicked() {
                *check_key = Some(self.api_key_input.clone());
            }
        });
        ui.label(
            egui::RichText::new(
                "The metadata provider is TMDB (themoviedb.org); IMDb has no official public \
                 API. A free TMDB account gives a v3 key or a v4 read access token starting \
                 with eyJ. Either works. A saved key always wins over REEL_TMDB_API_KEY.",
            )
            .color(theme::TEXT_DIM)
            .size(11.0),
        );

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    draft.stored_key().is_some(),
                    egui::Button::new("Remove stored key"),
                )
                .clicked()
            {
                *remove_key = true;
            }
            if ui.button("Look up metadata again").clicked() {
                *action = Some(SettingsAction::RefreshMetadata);
            }
            if ui.button("Clear poster and metadata cache").clicked() {
                *action = Some(SettingsAction::ClearCache);
            }
        });
        if let Some(note) = catalog.note.as_deref() {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(note).color(theme::WARN).size(12.0));
        }
    }

    pub(crate) fn settings_playback(
        &self,
        ui: &mut egui::Ui,
        draft: &mut CatalogSettings,
        caps: &crate::backend::BackendCapabilities,
    ) {
        section_header(ui, "Playback");
        ui.add_space(4.0);
        ui.checkbox(
            &mut draft.subtitles_enabled,
            "Turn subtitles on when a file has them",
        );
        ui.horizontal(|ui| {
            ui.add_sized(
                Vec2::new(150.0, 18.0),
                egui::Label::new(egui::RichText::new("Subtitle language").color(theme::TEXT_DIM)),
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
            ui.add(egui::Slider::new(&mut draft.default_volume, 0.0..=130.0).suffix(" %"));
        });
        ui.add_space(6.0);
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
    }

    pub(crate) fn settings_downloads(&self, ui: &mut egui::Ui, draft: &mut CatalogSettings) {
        section_header(ui, "Streaming and downloads");
        ui.add_space(4.0);
        ui.checkbox(
            &mut draft.stream_only,
            "Stream on demand (new multi-file torrents wait until you pick something)",
        );
        ui.label(
            egui::RichText::new(
                "With this on, watching a file fetches only that file (and its subtitles). Use \
                 the Download buttons on a title to keep individual files, episodes or a whole \
                 season. With it off, new torrents download every playable file in the \
                 background.",
            )
            .color(theme::TEXT_DIM)
            .size(11.0),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_sized(
                Vec2::new(150.0, 18.0),
                egui::Label::new(egui::RichText::new("Stream buffer").color(theme::TEXT_DIM)),
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
                egui::Label::new(egui::RichText::new("Session cache").color(theme::TEXT_DIM)),
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
    }

    pub(crate) fn settings_network(&self, ui: &mut egui::Ui, draft: &mut CatalogSettings) {
        section_header(ui, "Torrent engine");
        ui.add_space(4.0);

        let mut port = draft.listen_port.unwrap_or(0) as i32;
        ui.horizontal(|ui| {
            ui.label("Listen port (0 = automatic)");
            if ui
                .add(egui::DragValue::new(&mut port).range(0..=65535))
                .changed()
            {
                draft.listen_port = (port != 0).then_some(port as u16);
            }
        });

        let mut peer_limit = draft.peer_limit.unwrap_or(0) as i64;
        ui.horizontal(|ui| {
            ui.label("Peer limit per torrent (0 = default)");
            if ui
                .add(egui::DragValue::new(&mut peer_limit).range(0..=10_000))
                .changed()
            {
                draft.peer_limit = (peer_limit != 0).then_some(peer_limit as usize);
            }
        });

        let mut download = draft.download_limit_kib.unwrap_or(0) as i64;
        ui.horizontal(|ui| {
            ui.label("Download limit KiB/s (0 = unlimited)");
            if ui
                .add(
                    egui::DragValue::new(&mut download)
                        .range(0..=1_000_000_000)
                        .speed(64.0),
                )
                .changed()
            {
                draft.download_limit_kib = (download != 0).then_some(download as u32);
            }
        });

        let mut upload = draft.upload_limit_kib.unwrap_or(0) as i64;
        ui.horizontal(|ui| {
            ui.label("Upload limit KiB/s (0 = unlimited)");
            if ui
                .add(
                    egui::DragValue::new(&mut upload)
                        .range(0..=1_000_000_000)
                        .speed(64.0),
                )
                .changed()
            {
                draft.upload_limit_kib = (upload != 0).then_some(upload as u32);
            }
        });

        ui.add_space(6.0);
        ui.checkbox(&mut draft.dht_enabled, "Use the DHT");
        ui.checkbox(&mut draft.trackers_enabled, "Announce to trackers");
        ui.checkbox(&mut draft.enable_upnp, "Open the listen port with UPnP");
        ui.checkbox(&mut draft.ipv4_only, "IPv4 only");
        ui.label(
            egui::RichText::new(
                "Port, DHT and tracker changes take effect on the next start; the rate limits \
                 apply to torrents added after saving.",
            )
            .color(theme::TEXT_DIM)
            .size(11.0),
        );
    }

    pub(crate) fn settings_casting(&self, ui: &mut egui::Ui, draft: &mut CatalogSettings) {
        section_header(ui, "Casting");
        ui.add_space(4.0);
        ui.checkbox(
            &mut draft.allow_lan_streaming,
            "Stream to DLNA/UPnP devices on the local network",
        );
        ui.label(
            egui::RichText::new(
                "Opens a token-gated media port on your LAN. Required to cast to a TV; \
                 the port is only reachable while reel is running.",
            )
            .color(theme::TEXT_DIM)
            .size(11.0),
        );

        let mut port = draft.cast_media_port as i32;
        ui.horizontal(|ui| {
            ui.label("LAN media port (0 = automatic)");
            if ui
                .add(egui::DragValue::new(&mut port).range(0..=65535))
                .changed()
            {
                draft.cast_media_port = port as u16;
            }
        });
        ui.label(
            egui::RichText::new(
                "If a firewall is running (e.g. ufw), allow this port from your LAN — \
                 otherwise the TV cannot fetch the stream and casting fails with UPnP \
                 error 716.",
            )
            .color(theme::TEXT_DIM)
            .size(11.0),
        );
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new("When a file is not directly playable")
                .size(11.5)
                .color(theme::TEXT_DIM),
        );
        ui.horizontal(|ui| {
            for (mode, label, hint) in [
                (
                    ConversionMode::Off,
                    "Off",
                    "Only cast files the TV can already play",
                ),
                (
                    ConversionMode::Remux,
                    "Rewrap",
                    "Change the container, never re-encode",
                ),
                (
                    ConversionMode::Auto,
                    "Convert",
                    "Re-encode only when necessary",
                ),
            ] {
                if ui
                    .selectable_label(draft.conversion_mode == mode, label)
                    .on_hover_text(hint)
                    .clicked()
                {
                    draft.conversion_mode = mode;
                }
            }
        });

        ui.add_space(6.0);
        let mut height = draft.conversion_max_height as i32;
        ui.horizontal(|ui| {
            ui.label("Max height when converting");
            if ui
                .add(
                    egui::DragValue::new(&mut height)
                        .range(144..=4320)
                        .suffix(" px"),
                )
                .changed()
            {
                draft.conversion_max_height = height.max(144) as u32;
            }
        });

        ui.add_space(6.0);
        ui.checkbox(
            &mut draft.allow_hevc_passthrough,
            "Trust the TV with H.265 (HEVC) and AC3/EAC3",
        )
        .on_hover_text(
            "Remux 4K HDR/HEVC and surround audio instead of re-encoding, so it \
             starts immediately. Enable if your TV supports them (most modern 4K \
             TVs do); otherwise leave it off.",
        );

        optional_text(
            ui,
            "ffmpeg path (blank = bundled, then PATH)",
            &mut draft.ffmpeg_path,
            "/usr/bin/ffmpeg",
        );

        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "Applies immediately. Toggling the setting or changing the LAN address \
                 restarts the media server and ends any cast in progress.",
            )
            .color(theme::TEXT_DIM)
            .size(11.0),
        );
    }

    pub(crate) fn settings_search(&self, ui: &mut egui::Ui, draft: &mut CatalogSettings) {
        section_header(ui, "Search sources");
        ui.add_space(4.0);
        ui.checkbox(
            &mut draft.ext_to_enabled,
            "ext.to (a general torrent index behind Cloudflare)",
        );
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("ext.to")
                .size(13.0)
                .color(theme::TEXT_DIM),
        );
        optional_text(
            ui,
            "Cookie (optional)",
            &mut draft.ext_to_cookie,
            "cf_clearance=\u{2026}",
        );
        optional_text(
            ui,
            "User agent (optional)",
            &mut draft.ext_to_user_agent,
            "the browser the cookie came from",
        );
        optional_text(
            ui,
            "Base URL (optional)",
            &mut draft.ext_to_base_url,
            "https://ext.to",
        );
        optional_text(
            ui,
            "Solver URL (optional)",
            &mut draft.ext_to_solver_url,
            "only used if the built-in solver cannot clear a challenge",
        );

        ui.add_space(6.0);
        let sources = self.backend.search_sources();
        if sources.is_empty() {
            row(ui, "Active sources", "none");
        } else {
            let names: Vec<String> = sources
                .iter()
                .map(|source| {
                    if source.configured {
                        source.name.clone()
                    } else {
                        format!("{} (needs setup)", source.name)
                    }
                })
                .collect();
            row(ui, "Active sources", &names.join(", "));
        }
        ui.label(
            egui::RichText::new(
                "A search backend is anything implementing the SearchBackend trait; ext.to is \
                 the bundled example.",
            )
            .color(theme::TEXT_DIM)
            .size(12.0),
        );
    }

    pub(crate) fn settings_about(&self, ui: &mut egui::Ui) {
        section_header(ui, "About");
        ui.add_space(4.0);
        row(ui, "Version", env!("CARGO_PKG_VERSION"));
        ui.label(
            egui::RichText::new(
                "reel streams torrents over HTTP with byte-range seeking. Titles are matched \
                 against a metadata provider; artwork is generated from each title when none \
                 is available.",
            )
            .color(theme::TEXT_DIM)
            .size(12.0),
        );
    }
}

/// The Settings panes, in the order the sidebar lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    General,
    Metadata,
    Playback,
    Downloads,
    Network,
    Casting,
    Search,
    About,
}

impl SettingsSection {
    pub(crate) const ALL: [SettingsSection; 8] = [
        SettingsSection::General,
        SettingsSection::Metadata,
        SettingsSection::Playback,
        SettingsSection::Downloads,
        SettingsSection::Network,
        SettingsSection::Casting,
        SettingsSection::Search,
        SettingsSection::About,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            SettingsSection::General => "General",
            SettingsSection::Metadata => "Metadata",
            SettingsSection::Playback => "Playback",
            SettingsSection::Downloads => "Downloads",
            SettingsSection::Network => "Network",
            SettingsSection::Casting => "Casting",
            SettingsSection::Search => "Search sources",
            SettingsSection::About => "About",
        }
    }
}

pub(crate) enum SettingsAction {
    RefreshMetadata,
    ClearCache,
}

/// Edit an optional text setting under a small label. Committing blank clears
/// it, so the settings file only ever holds real values.
pub(crate) fn optional_text(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<String>,
    hint: &str,
) {
    ui.label(egui::RichText::new(label).size(11.5).color(theme::TEXT_DIM));
    let mut text = value.clone().unwrap_or_default();
    let changed = ui
        .add(
            egui::TextEdit::singleline(&mut text)
                .hint_text(hint)
                .desired_width(f32::INFINITY),
        )
        .changed();
    if changed {
        *value = if text.trim().is_empty() {
            None
        } else {
            Some(text)
        };
    }
    ui.add_space(4.0);
}
