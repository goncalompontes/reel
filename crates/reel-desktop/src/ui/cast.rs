//! Casting UI: the shell control, the device picker, the "now playing on TV"
//! remote and its compact mini bar.
//!
//! Playback has one destination. Choosing a device connects to it, and from
//! then on anything played is cast instead of opened here (see
//! `App::prepare_play`). Starting a cast never takes over the screen: a mini
//! bar appears and the full remote is opt-in (the shell control or the bar
//! opens it).

use super::*;
use crate::cast::{CastAction, CastStatus, DeviceInfo, PreparePhase};
use crate::components::icon_button;

/// The casting state the shell and the surfaces render from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CastFlow {
    /// Casting is not available; the reason comes from the backend.
    Off,
    /// No device connected.
    Idle,
    /// Connected, nothing playing.
    Ready,
    /// A cast is being prepared.
    Connecting,
    /// The renderer is playing (or paused/buffering).
    Playing,
    /// The last attempt failed.
    Failed,
}

impl App {
    // ---------------------------------------------------------- the shell

    /// The persistent cast entry for the rail and the header: the one place to
    /// see and change where playback goes.
    pub(crate) fn cast_indicator(&mut self, ui: &mut egui::Ui, compact: bool) {
        let flow = self.cast_flow();

        // Casting is opt-in (it opens a LAN port). When it is off, the shell
        // keeps no entry; Settings → Casting is where it is turned on and
        // diagnosed.
        if flow == CastFlow::Off {
            return;
        }

        let device = self.cast_device.clone();
        let fraction = self.cast_fraction();
        let title = self.cast_title();
        let hover = match (&flow, &device) {
            (CastFlow::Idle, _) => "Play on a TV on your network".to_string(),
            (CastFlow::Ready, Some(device)) => {
                format!("Connected to {}\nPlay something to cast it", device.name)
            }
            (CastFlow::Connecting, Some(device)) => {
                format!("Connecting to {}\u{2026}", device.name)
            }
            (CastFlow::Playing, Some(device)) => match &title {
                Some(title) => format!("{title}\nPlaying on {}", device.name),
                None => format!("Playing on {}", device.name),
            },
            (CastFlow::Failed, Some(device)) => {
                format!("Cast to {} did not start", device.name)
            }
            (_, Some(device)) => device.name.clone(),
            (_, None) => "Play on a TV on your network".to_string(),
        };

        let (badge, badge_color) = match flow {
            CastFlow::Connecting => (icons::SPINNER.to_string(), theme::WARN),
            CastFlow::Playing => (icons::TELEVISION.to_string(), theme::OK),
            CastFlow::Failed => (icons::X.to_string(), theme::DANGER),
            CastFlow::Ready => (icons::CHECK.to_string(), theme::ACCENT),
            _ => (icons::TELEVISION.to_string(), theme::TEXT),
        };
        let name = device
            .as_ref()
            .map(|device| reel_core::title::truncate(&device.name, 18))
            .unwrap_or_else(|| "Cast".to_string());
        let filled = !matches!(flow, CastFlow::Idle);

        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), hover.clone())
        });
        let response = response.on_hover_text(hover);
        let painter = ui.painter().clone();

        let hovered = response.hovered();
        let background = if hovered {
            theme::SURFACE_RAISED
        } else if filled {
            theme::ACCENT_DEEP
        } else {
            Color32::TRANSPARENT
        };
        if filled || hovered {
            painter.rect_filled(rect, theme::CONTROL_RADIUS, background);
        }
        let text_color = if matches!(flow, CastFlow::Idle) {
            theme::TEXT
        } else {
            badge_color
        };
        let badge_center = if compact {
            rect.center()
        } else {
            egui::pos2(rect.left() + 17.0, rect.center().y)
        };
        painter.text(
            badge_center,
            egui::Align2::CENTER_CENTER,
            &badge,
            egui::FontId::proportional(15.0),
            text_color,
        );
        match flow {
            CastFlow::Connecting => {
                let time = ui.input(|input| input.time) as f32;
                ring(&painter, badge_center, 11.5, (time * 0.7) % 1.0, theme::WARN, true);
            }
            CastFlow::Playing => {
                if let Some(fraction) = fraction {
                    ring(&painter, badge_center, 11.5, fraction, theme::OK, false);
                }
            }
            _ => {}
        }
        if !compact {
            painter.text(
                egui::pos2(rect.left() + 32.0, rect.center().y),
                egui::Align2::LEFT_CENTER,
                name,
                egui::FontId::proportional(12.5),
                text_color,
            );
        }

        if response.clicked() {
            self.cast_control_click();
        }
        response.context_menu(|ui| {
            if ui.button("Open the cast remote").clicked() {
                self.cast_screen_open = true;
                ui.close();
            }
            if self.cast_device.is_some() {
                if ui.button("Switch device\u{2026}").clicked() {
                    self.switch_device();
                    ui.close();
                }
                if ui.button("Watch here").clicked() {
                    self.stop_cast();
                    ui.close();
                }
                if ui.button("Disconnect").clicked() {
                    self.disconnect_cast();
                    ui.close();
                }
            }
        });
    }

    /// The local player OSD's cast button: connect, or open the remote.
    pub(crate) fn cast_button(&mut self, ui: &mut egui::Ui) {
        let unavailable = self.backend.cast_unavailable();
        let connected = self.cast_device.is_some();
        let label = if connected { "On TV" } else { "Cast" };
        let response = ui.add_enabled(
            unavailable.is_none(),
            egui::Button::new(egui::RichText::new(label).size(13.0)),
        );
        let response = match &unavailable {
            Some(reason) => response.on_disabled_hover_text(reason),
            None if connected => response.on_hover_text("Show what is playing on the TV"),
            None => response.on_hover_text("Play on a TV on your network"),
        };
        if response.clicked() {
            self.cast_control_click();
        }
    }

    /// Clicking the shell control: open the picker when idle, the remote when
    /// connected.
    pub(crate) fn cast_control_click(&mut self) {
        if self.cast_device.is_some() {
            self.cast_screen_open = true;
        } else {
            self.open_cast_picker();
        }
    }

    /// Open the picker for whatever the screen implies, or an empty picker when
    /// nothing is selected. Public for tests and the debug tool.
    pub fn open_cast_picker(&mut self) {
        match self.cast_target() {
            Some(target) => self.request_cast(target),
            None => {
                self.backend.refresh_cast_devices();
                self.cast_open = true;
            }
        }
    }

    /// Open the picker to change device while connected, keeping the current
    /// cast running until a new device is chosen.
    fn switch_device(&mut self) {
        self.cast_pending = self.cast_last.clone();
        self.backend.refresh_cast_devices();
        self.cast_open = true;
    }

    // --------------------------------------------------------- the flow

    fn cast_flow(&self) -> CastFlow {
        if self.backend.cast_unavailable().is_some() {
            return CastFlow::Off;
        }
        if self.cast_device.is_none() {
            return CastFlow::Idle;
        }
        match self.backend.cast_status() {
            None => CastFlow::Ready,
            Some(CastStatus::Preparing { .. }) => CastFlow::Connecting,
            Some(CastStatus::Active { .. }) => CastFlow::Playing,
            Some(CastStatus::Failed { .. }) => CastFlow::Failed,
        }
    }

    /// The title being cast, when a status names one.
    fn cast_title(&self) -> Option<String> {
        match self.backend.cast_status()? {
            CastStatus::Preparing { title, .. } | CastStatus::Active { title, .. } => Some(title),
            CastStatus::Failed { .. } => None,
        }
    }

    /// Playback progress as a 0–1 fraction, when both ends are known.
    fn cast_fraction(&self) -> Option<f32> {
        if let Some(CastStatus::Active {
            position: Some(position),
            duration: Some(duration),
            ..
        }) = self.backend.cast_status()
        {
            let total = duration.as_secs_f64();
            if total > 0.0 {
                return Some((position.as_secs_f64() / total).clamp(0.0, 1.0) as f32);
            }
        }
        None
    }

    /// A " on <device>" suffix for Play controls, so the destination is always
    /// stated. Empty when playing locally.
    pub(crate) fn cast_suffix(&self) -> String {
        match &self.cast_device {
            Some(device) => format!(
                " on {}",
                reel_core::title::truncate(&device.name, 16)
            ),
            None => String::new(),
        }
    }

    /// The device name, for surfaces that show it separately from the title.
    pub(crate) fn cast_device_name(&self) -> Option<String> {
        self.cast_device.as_ref().map(|device| device.name.clone())
    }

    // ------------------------------------------------------- the picker

    /// The file the current screen implies should be cast.
    fn cast_target(&self) -> Option<CastTarget> {
        if let Some(info) = self.player.current() {
            let state = self.player.state();
            let start_at = (state.loaded && state.position >= 1.0).then_some(state.position);
            return Some(CastTarget {
                torrent_id: info.torrent_id,
                info_hash: info.info_hash.clone(),
                file_id: info.file_id,
                title: info.title.clone(),
                start_at,
            });
        }
        let work = self.selected_work()?;
        let (torrent_id, file_id) = self.work_target(&work)?;
        let item = self.item(torrent_id)?;
        Some(CastTarget {
            torrent_id,
            info_hash: item.entry.info_hash.clone(),
            file_id,
            title: work.heading(),
            start_at: resume_start_at(&item, file_id),
        })
    }

    /// Ask to cast a specific file, opening the device picker.
    pub(crate) fn request_cast(&mut self, target: CastTarget) {
        // A new title starts on the preferred-language track, not the last pick.
        self.cast_audio_index = None;
        self.cast_pending = Some(target);
        self.backend.refresh_cast_devices();
        self.cast_open = true;
    }

    /// The device picker, as a modal so it owns focus and cannot be lost behind
    /// other content.
    pub(crate) fn cast_picker(&mut self, ctx: &egui::Context) {
        if !self.cast_open {
            return;
        }
        if self.backend.cast_unavailable().is_some() {
            self.cast_open = false;
            self.settings_section = SettingsSection::Casting;
            self.go_to(Screen::Settings);
            return;
        }

        let mut chosen: Option<DeviceInfo> = None;
        let mut rescan = false;
        let mut add = false;
        let mut watch_here = false;
        let mut close = false;

        let mut devices = self.backend.cast_devices();
        let last_udn = self.backend.settings().cast_last_device_udn;
        if let Some(udn) = &last_udn {
            devices.sort_by_key(|device| u8::from(&device.udn != udn));
        }
        let discovering = self.backend.cast_discovering();
        let connected = self.cast_device.clone();
        let pending = self.cast_pending.clone();

        let modal = egui::Modal::new(egui::Id::new("cast-picker")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Play on a device").size(18.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icon_button(ui, icons::X).clicked() {
                        close = true;
                    }
                });
            });

            // What will be cast, and where it continues from.
            if let Some(target) = &pending {
                ui.add_space(2.0);
                let (poster_seed, reference) = self
                    .work_id_for_torrent(target.torrent_id)
                    .and_then(|work_id| self.work(work_id))
                    .map(|work| (work.title.clone(), work.poster().cloned()))
                    .unwrap_or_else(|| (target.title.clone(), None));
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(Vec2::new(42.0, 62.0), egui::Sense::hover());
                    Self::paint_art(
                        ui,
                        rect,
                        reference.as_ref(),
                        &poster_seed,
                        ArtworkKind::Poster,
                        theme::CARD_RADIUS,
                    );
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(&target.title).size(14.0).strong());
                        match target.start_at.filter(|from| *from >= 1.0) {
                            Some(from) => ui.label(
                                egui::RichText::new(format!(
                                    "Resumes from {}",
                                    fmt::human_duration(from)
                                ))
                                .size(11.0)
                                .color(theme::ACCENT),
                            ),
                            None => ui.label(
                                egui::RichText::new("Starts from the beginning")
                                    .size(11.0)
                                    .color(theme::TEXT_DIM),
                            ),
                        };
                    });
                });
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
            }

            ui.horizontal(|ui| {
                if discovering {
                    ui.spinner();
                    ui.label(egui::RichText::new("Searching\u{2026}").color(theme::TEXT_DIM));
                } else {
                    ui.label(egui::RichText::new("Nearby").color(theme::TEXT_DIM).size(11.5));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ghost_button(ui, "Rescan").clicked() {
                        rescan = true;
                    }
                });
            });
            ui.add_space(2.0);

            if devices.is_empty() && !discovering {
                ui.label(
                    egui::RichText::new(
                        "No devices found. Make sure the TV is on and on the same network.",
                    )
                    .color(theme::TEXT_DIM)
                    .size(11.5),
                );
            }
            for device in &devices {
                let is_connected = connected
                    .as_ref()
                    .is_some_and(|current| current.udn == device.udn);
                let model = device.model.clone().unwrap_or_default();
                let glyph = if is_connected {
                    icons::TELEVISION
                } else {
                    icons::MONITOR_PLAY
                };
                // The protocol is shown so a DLNA TV and a Chromecast are
                // distinguishable at a glance.
                let kind = device.kind.label();
                let detail = if model.is_empty() {
                    let status = if is_connected {
                        "Connected".to_string()
                    } else if last_udn.as_deref() == Some(device.udn.as_str()) {
                        "Last used".to_string()
                    } else {
                        "Available".to_string()
                    };
                    format!("{kind} \u{00b7} {status}")
                } else {
                    format!("{model} \u{00b7} {kind}")
                };
                let label = format!("{glyph}  {}\n     {detail}", device.name);
                let mut button =
                    egui::Button::new(egui::RichText::new(label).size(13.0))
                        .min_size(Vec2::new(ui.available_width(), 0.0));
                if is_connected {
                    button = button.fill(theme::ACCENT_DEEP);
                }
                let response = ui.add(button);
                let response = if is_connected {
                    response.on_hover_text("The connected device")
                } else {
                    response
                };
                if response.clicked() {
                    chosen = Some(device.clone());
                }
            }

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Add by address").size(12.0));
                ui.add(
                    egui::TextEdit::singleline(&mut self.cast_address)
                        .hint_text("192.168.1.50")
                        .desired_width(120.0),
                );
                if ghost_button(ui, "Add").clicked() {
                    add = true;
                }
            });
            ui.label(
                egui::RichText::new("Use this when the TV does not appear (some networks block discovery).")
                    .size(11.0)
                    .color(theme::TEXT_DIM),
            );

            if connected.is_some() {
                ui.add_space(10.0);
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new(format!("{}  Disconnect and play here", icons::X))
                                .size(12.0),
                        )
                        .fill(Color32::TRANSPARENT)
                        .stroke(egui::Stroke::new(1.0, theme::BORDER))
                        .corner_radius(theme::CONTROL_RADIUS),
                    )
                    .clicked()
                {
                    watch_here = true;
                }
            }
        });
        if modal.backdrop_response.clicked() {
            close = true;
        }

        if rescan {
            self.backend.refresh_cast_devices();
        }
        if add {
            let address = self.cast_address.trim().to_string();
            if !address.is_empty() {
                self.backend.add_cast_device(address);
            }
        }
        if let Some(device) = chosen {
            match self.cast_pending.take() {
                Some(target) => self.connect_cast(device, target),
                None => self.connect_only(device),
            }
            self.cast_open = false;
        }
        if watch_here {
            self.disconnect_cast();
            self.cast_open = false;
        }
        if close {
            self.cast_open = false;
        }
    }

    // --------------------------------------------------------- connecting

    /// Route a play to the connected device, handing the picture over.
    pub(crate) fn cast_here(
        &mut self,
        device: DeviceInfo,
        torrent_id: usize,
        file_id: usize,
        start_at: Option<f64>,
    ) {
        let Some(item) = self.item(torrent_id) else {
            self.warn("That torrent is no longer in the library");
            return;
        };
        let target = CastTarget {
            torrent_id,
            info_hash: item.entry.info_hash.clone(),
            file_id,
            title: item.heading(),
            start_at,
        };
        // A new title starts on the preferred-language track.
        self.cast_audio_index = None;
        self.connect_cast(device, target);
    }

    /// Connect to a device and cast `target` to it.
    ///
    /// The remote is **not** opened: playback surfaces through the mini bar, so
    /// casting never takes over what the user is doing.
    pub(crate) fn connect_cast(&mut self, device: DeviceInfo, target: CastTarget) {
        self.hand_over_to_cast();
        self.remember_device(&device);
        self.cast_device = Some(device.clone());
        self.cast_last = Some(target.clone());
        self.cast_stop_requested = false;
        self.cast_resume_on_end = false;
        // A new device starts unmuted; the button state must not carry over.
        self.cast_muted = false;
        self.backend.start_cast(
            device.udn,
            target.torrent_id,
            target.info_hash,
            target.file_id,
            target.title,
            target.start_at.map(Duration::from_secs_f64),
            self.cast_subtitles,
            self.cast_audio_index,
        );
    }

    /// Connect with nothing to cast yet; the next play goes to the device.
    pub(crate) fn connect_only(&mut self, device: DeviceInfo) {
        self.hand_over_to_cast();
        self.remember_device(&device);
        self.cast_device = Some(device.clone());
        self.cast_last = None;
        self.cast_stop_requested = false;
        self.cast_resume_on_end = false;
        self.cast_muted = false;
        self.cast_audio_index = None;
        self.set_toast(format!("Connected to {}", device.name), false);
    }

    /// Remember the last device for the next picker, and offer a quick
    /// reconnect the next time the app starts.
    fn remember_device(&mut self, device: &DeviceInfo) {
        self.backend.remember_cast_device(&device.name, &device.udn);
    }

    // ------------------------------------------------------ the remote

    /// The full-screen casting remote, opened on demand.
    pub(crate) fn cast_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ctx.request_repaint_after(Duration::from_millis(500));

        let Some(device) = self.cast_device.clone() else {
            self.cast_screen_open = false;
            return;
        };
        let status = self.backend.cast_status();
        let target = self.cast_last.clone();

        let mut action: Option<CastAction> = None;
        let mut watch_here = false;
        let mut disconnect = false;
        let mut retry = false;
        let mut switch = false;
        let mut toggle_subtitles = false;
        let mut toggle_mute = false;
        let mut pick_audio: Option<u32> = None;
        let mut back = false;
        let mut play_next: Option<(usize, usize)> = None;

        // Escape returns to browsing, leaving the cast running.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            back = true;
        }

        let title = self
            .cast_title()
            .or_else(|| target.as_ref().map(|target| target.title.clone()))
            .unwrap_or_else(|| match &status {
                Some(CastStatus::Failed { .. }) => "Casting failed".to_string(),
                _ => device.name.clone(),
            });
        let eyebrow = match &status {
            Some(CastStatus::Preparing { .. }) | Some(CastStatus::Active { .. }) => {
                format!("Playing on {}", device.name)
            }
            Some(CastStatus::Failed { .. }) => format!("Cast to {}", device.name),
            None => format!("Connected to {}", device.name),
        };

        egui::Panel::top("cast-screen-bar")
            .frame(
                egui::Frame::NONE
                    .fill(theme::SURFACE)
                    .inner_margin(egui::Margin::symmetric(14, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .button(format!("{}  Browse", icons::ARROW_LEFT))
                        .clicked()
                    {
                        back = true;
                    }
                    ui.label(egui::RichText::new(&title).size(15.0).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.menu_button(icons::DOTS_THREE, |ui| {
                            if ui.button("Switch device\u{2026}").clicked() {
                                switch = true;
                                ui.close();
                            }
                            if ui.button("Watch here").clicked() {
                                watch_here = true;
                                ui.close();
                            }
                            if ui.button("Disconnect").clicked() {
                                disconnect = true;
                                ui.close();
                            }
                        });
                        ui.label(
                            egui::RichText::new(format!("on {}", device.name))
                                .size(11.5)
                                .color(theme::TEXT_DIM),
                        );
                    });
                });
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme::BG))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(18.0);
                        // Backdrop artwork, cropped, so the remote looks like the
                        // rest of the app rather than an empty page.
                        let width = 560.0_f32.min(ui.available_width() - 40.0).max(240.0);
                        let (rect, _) = ui.allocate_exact_size(
                            Vec2::new(width, width * 9.0 / 16.0),
                            egui::Sense::hover(),
                        );
                        let reference = target
                            .as_ref()
                            .and_then(|target| self.work_id_for_torrent(target.torrent_id))
                            .and_then(|work_id| self.work(work_id))
                            .and_then(|work| work.backdrop().cloned());
                        Self::paint_art(
                            ui,
                            rect,
                            reference.as_ref(),
                            &title,
                            ArtworkKind::Backdrop,
                            theme::PANEL_RADIUS,
                        );
                        theme::paint_scrim(&ui.painter().clone(), rect, theme::PANEL_RADIUS, 200);

                        ui.add_space(14.0);
                        ui.label(
                            egui::RichText::new(&eyebrow)
                                .size(12.0)
                                .color(theme::ACCENT),
                        );
                        ui.add_space(2.0);
                        ui.label(egui::RichText::new(&title).size(26.0).strong());
                        if let Some(target) = &target {
                            if let Some(item) = self.item(target.torrent_id) {
                                let mut facts: Vec<String> = Vec::new();
                                if let Some(meta) = item.entry.metadata.as_ref() {
                                    if let Some(year) = meta.year {
                                        facts.push(year.to_string());
                                    }
                                    if let Some(runtime) = meta.runtime_label() {
                                        facts.push(runtime);
                                    }
                                }
                                // The episode actually being cast, matched by the
                                // file, not the release's first episode: a season
                                // pack lists them all, and casting episode 6 must
                                // not read as episode 1.
                                if let Some(code) = episode_code(&item, target.file_id) {
                                    facts.push(code);
                                }
                                if !facts.is_empty() {
                                    ui.label(
                                        egui::RichText::new(facts.join("  \u{00b7}  "))
                                            .size(12.0)
                                            .color(theme::TEXT_DIM),
                                    );
                                }
                            }
                        }
                        ui.add_space(16.0);

                        match &status {
                            Some(CastStatus::Preparing { phase, .. }) => {
                                phase_steps(ui, *phase, width);
                                ui.add_space(18.0);
                            }
                            Some(CastStatus::Active {
                                state,
                                position,
                                duration,
                                volume,
                                ..
                            }) => {
                                let playing = state.is_playing();
                                let total = duration.map(|d| d.as_secs_f64()).unwrap_or(0.0);
                                let pos = position.map(|p| p.as_secs_f64());
                                if let Some(action_pos) =
                                    seek_scrubber(ui, pos, total, width)
                                {
                                    action = Some(CastAction::Seek(
                                        Duration::from_secs_f64(action_pos),
                                    ));
                                } else {
                                    ui.label(
                                        egui::RichText::new(if total > 0.0 {
                                            " "
                                        } else {
                                            "Live \u{00b7} position unavailable"
                                        })
                                        .size(11.0)
                                        .color(theme::TEXT_DIM),
                                    );
                                }
                                ui.add_space(12.0);
                                ui.allocate_ui(Vec2::new(width, 0.0), |ui| {
                                    ui.horizontal(|ui| {
                                        if crate::components::primary_button(
                                            ui,
                                            if playing { "Pause" } else { "Play" },
                                        )
                                        .clicked()
                                        {
                                            action = Some(if playing {
                                                CastAction::Pause
                                            } else {
                                                CastAction::Resume
                                            });
                                        }
                                        if ghost_button(ui, "Watch here")
                                            .on_hover_text("Stop the TV and resume on this screen")
                                            .clicked()
                                        {
                                            watch_here = true;
                                        }
                                    });
                                });
                                // Subtitles are a sidecar the TV fetches; the
                                // choice is part of the cast metadata, so
                                // toggling re-casts at the current position.
                                ui.add_space(10.0);
                                ui.allocate_ui(Vec2::new(width, 0.0), |ui| {
                                    ui.horizontal(|ui| {
                                        let label = if self.cast_subtitles {
                                            "Subtitles: on"
                                        } else {
                                            "Subtitles: off"
                                        };
                                        if ui
                                            .selectable_label(self.cast_subtitles, label)
                                            .on_hover_text(
                                                "Offer subtitles to the TV; pick the track in the TV's own subtitle menu",
                                            )
                                            .clicked()
                                        {
                                            toggle_subtitles = true;
                                        }
                                    });
                                });
                                // An audio-track picker, when the file has more
                                // than one. Picking re-casts so the chosen stream
                                // is mapped (a renderer cannot switch tracks).
                                let audio_tracks = self.backend.cast_audio_tracks();
                                if audio_tracks.len() > 1 {
                                    ui.add_space(10.0);
                                    ui.allocate_ui(Vec2::new(width, 0.0), |ui| {
                                        ui.horizontal_wrapped(|ui| {
                                            ui.label(
                                                egui::RichText::new("Audio")
                                                    .size(11.0)
                                                    .color(theme::TEXT_DIM),
                                            );
                                            for track in &audio_tracks {
                                                if ui
                                                    .selectable_label(
                                                        track.selected,
                                                        &track.label,
                                                    )
                                                    .clicked()
                                                    && !track.selected
                                                {
                                                    pick_audio = Some(track.index);
                                                }
                                            }
                                        });
                                    });
                                }
                                // Volume is only offered when the renderer
                                // exposes RenderingControl; a renderer without
                                // it reports no volume.
                                if let Some(current) = volume {
                                    ui.add_space(10.0);
                                    ui.allocate_ui(Vec2::new(width, 0.0), |ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                egui::RichText::new("Volume")
                                                    .size(11.0)
                                                    .color(theme::TEXT_DIM),
                                            );
                                            let mut value = *current as f32;
                                            if ui
                                                .add(
                                                    egui::Slider::new(&mut value, 0.0..=100.0)
                                                        .show_value(false),
                                                )
                                                .changed()
                                            {
                                                action = Some(CastAction::SetVolume(
                                                    value.round() as u8,
                                                ));
                                            }
                                            let label =
                                                if self.cast_muted { "Unmute" } else { "Mute" };
                                            if ui
                                                .selectable_label(self.cast_muted, label)
                                                .clicked()
                                            {
                                                toggle_mute = true;
                                            }
                                        });
                                    });
                                }
                                if let Some(target) = &target {
                                    if let Some((tid, fid, code)) =
                                        self.up_next(target.torrent_id, target.file_id)
                                    {
                                        ui.add_space(16.0);
                                        if ghost_button(
                                            ui,
                                            &format!("\u{25b6}  Play {code} on the TV"),
                                        )
                                        .clicked()
                                        {
                                            play_next = Some((tid, fid));
                                        }
                                    }
                                }
                            }
                            Some(CastStatus::Failed { message }) => {
                                ui.label(
                                    egui::RichText::new("Couldn't play on the TV")
                                        .size(20.0)
                                        .strong(),
                                );
                                ui.add_space(6.0);
                                ui.label(
                                    egui::RichText::new(message)
                                        .color(theme::DANGER)
                                        .size(12.0),
                                );
                                ui.add_space(16.0);
                                ui.allocate_ui(Vec2::new(width, 0.0), |ui| {
                                    ui.horizontal(|ui| {
                                        if crate::components::primary_button(ui, "Try again")
                                            .clicked()
                                        {
                                            retry = true;
                                        }
                                        if ghost_button(ui, "Switch device\u{2026}").clicked() {
                                            switch = true;
                                        }
                                        if ghost_button(ui, "Watch here").clicked() {
                                            watch_here = true;
                                        }
                                    });
                                });
                            }
                            None => {
                                ui.label(
                                    egui::RichText::new("Nothing is playing on the TV")
                                        .size(18.0)
                                        .strong(),
                                );
                                ui.add_space(6.0);
                                ui.label(
                                    egui::RichText::new(
                                        "Play something and it will play here, on the TV.",
                                    )
                                    .color(theme::TEXT_DIM),
                                );
                            }
                        }
                        ui.add_space(20.0);
                    });
                });
            });

        if back {
            self.cast_screen_open = false;
        }
        if let Some(action) = action {
            self.backend.cast_action(action);
        }
        if watch_here {
            self.stop_cast();
        }
        if retry
            && let Some(target) = target.clone()
        {
            self.connect_cast(device.clone(), target);
        }
        if switch {
            self.switch_device();
        }
        if disconnect {
            self.disconnect_cast();
        }
        if toggle_subtitles {
            self.cast_subtitles = !self.cast_subtitles;
            // Re-cast at the current position so the renderer receives the new
            // subtitle metadata.
            let start = match &status {
                Some(CastStatus::Active {
                    position: Some(position),
                    ..
                }) => Some(position.as_secs_f64()),
                _ => target.as_ref().and_then(|target| target.start_at),
            };
            if let Some(target) = target.clone() {
                let target = CastTarget { start_at: start, ..target };
                self.connect_cast(device.clone(), target);
            }
        }
        if toggle_mute {
            self.cast_muted = !self.cast_muted;
            self.backend.cast_action(CastAction::SetMute(self.cast_muted));
        }
        if let Some(index) = pick_audio {
            // Re-cast at the current position so the chosen stream is mapped;
            // the renderer cannot switch tracks on its own.
            self.cast_audio_index = Some(index);
            let start = match &status {
                Some(CastStatus::Active {
                    position: Some(position),
                    ..
                }) => Some(position.as_secs_f64()),
                _ => target.as_ref().and_then(|target| target.start_at),
            };
            if let Some(target) = target.clone() {
                let target = CastTarget { start_at: start, ..target };
                self.connect_cast(device.clone(), target);
            }
        }
        if let Some((torrent_id, file_id)) = play_next {
            self.prepare_play(torrent_id, file_id, None, false);
        }
    }

    // ------------------------------------------------------- the mini bar

    /// The compact "now casting" bar, shown while a cast runs and the remote is
    /// closed.
    pub(crate) fn cast_bar(&mut self, ui: &mut egui::Ui) {
        let Some(status) = self.backend.cast_status() else {
            return;
        };
        let device = self.cast_device.clone();
        let fraction = self.cast_fraction();

        let mut action: Option<CastAction> = None;
        let mut open_view = false;
        let mut watch_here = false;
        let mut dismiss = false;

        ui.horizontal(|ui| {
            if let Some(fraction) = fraction {
                ui.scope(|ui| {
                    ui.set_min_size(Vec2::new(20.0, 20.0));
                    let (rect, _) =
                        ui.allocate_exact_size(Vec2::new(20.0, 20.0), egui::Sense::hover());
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        icons::TELEVISION,
                        egui::FontId::proportional(18.0),
                        theme::ACCENT,
                    );
                    ring(
                        &ui.painter().clone(),
                        rect.center(),
                        9.0,
                        fraction,
                        theme::ACCENT,
                        false,
                    );                });
            } else {
                ui.label(
                    egui::RichText::new(icons::TELEVISION)
                        .size(18.0)
                        .color(theme::ACCENT),
                );
            }
            ui.add_space(6.0);

            ui.vertical(|ui| match &status {
                CastStatus::Preparing { title, phase, .. } => {
                    ui.label(egui::RichText::new(title).size(14.5).strong());
                    let mut line = prepare_label(*phase).to_string();
                    if let Some(device) = &device {
                        line.push_str(&format!("  \u{00b7}  on {}", device.name));
                    }
                    ui.label(
                        egui::RichText::new(line)
                            .size(11.0)
                            .color(theme::TEXT_DIM),
                    );
                }
                CastStatus::Active {
                    title,
                    position,
                    duration,
                    ..
                } => {
                    ui.label(egui::RichText::new(title).size(14.5).strong());
                    let pos = position.map(|p| fmt::human_duration(p.as_secs_f64()));
                    let total = duration.map(|d| fmt::human_duration(d.as_secs_f64()));
                    let mut line = match (pos, total) {
                        (Some(pos), Some(total)) => format!("{pos} / {total}"),
                        (Some(pos), None) => pos,
                        _ => "Live".to_string(),
                    };
                    if let Some(device) = &device {
                        line.push_str(&format!("  \u{00b7}  on {}", device.name));
                    }
                    ui.label(
                        egui::RichText::new(line)
                            .size(11.0)
                            .color(theme::TEXT_DIM),
                    );
                }
                CastStatus::Failed { message } => {
                    ui.label(egui::RichText::new("Casting failed").size(14.5).strong());
                    ui.label(
                        egui::RichText::new(message)
                            .size(11.0)
                            .color(theme::DANGER),
                    );
                }
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                match &status {
                    CastStatus::Active { state, .. } => {
                        let playing = state.is_playing();
                        if crate::components::icon_button(
                            ui,
                            if playing { icons::PAUSE } else { icons::PLAY },
                        )
                        .on_hover_text(if playing { "Pause on the TV" } else { "Play on the TV" })
                        .clicked()
                        {
                            action = Some(if playing {
                                CastAction::Pause
                            } else {
                                CastAction::Resume
                            });
                        }
                        if ghost_button(ui, "Watch here")
                            .on_hover_text("Stop the TV and resume on this screen")
                            .clicked()
                        {
                            watch_here = true;
                        }
                        if icon_button(ui, icons::CARET_DOWN)
                            .on_hover_text("Open the cast remote")
                            .clicked()
                        {
                            open_view = true;
                        }
                    }
                    CastStatus::Preparing { .. } => {
                        ui.spinner();
                        if ghost_button(ui, "Cancel").clicked() {
                            dismiss = true;
                        }
                    }
                    CastStatus::Failed { .. } => {
                        if ghost_button(ui, "Dismiss").clicked() {
                            dismiss = true;
                        }
                    }
                }
            });
        });

        if let Some(action) = action {
            self.backend.cast_action(action);
        }
        if watch_here {
            self.stop_cast();
        }
        if dismiss {
            self.disconnect_cast();
        }
        if open_view {
            self.cast_screen_open = true;
        }
    }

    // ------------------------------------------------------------ ending

    /// Stop the TV and resume on this machine (or clear a failed cast).
    pub(crate) fn stop_cast(&mut self) {
        if matches!(self.backend.cast_status(), Some(CastStatus::Failed { .. })) {
            self.disconnect_cast();
        } else {
            self.cast_stop_requested = true;
            self.cast_resume_on_end = true;
            self.backend.cast_action(CastAction::Stop);
        }
    }

    /// End the cast without resuming here. An active renderer is asked to stop
    /// (and the connection cleared once it confirms); a pending or failed one is
    /// cancelled immediately.
    pub(crate) fn disconnect_cast(&mut self) {
        match self.backend.cast_status() {
            Some(CastStatus::Active { .. }) => {
                self.cast_stop_requested = true;
                self.cast_resume_on_end = false;
                self.backend.cast_action(CastAction::Stop);
            }
            _ => {
                self.cast_device = None;
                self.cast_last = None;
                self.cast_screen_open = false;
                self.cast_stop_requested = false;
                self.cast_resume_on_end = false;
                self.backend.dismiss_cast();
            }
        }
    }

    /// The picture is moving to a renderer: close the local player if it is
    /// open, leaving the stream running for the cast.
    pub(crate) fn hand_over_to_cast(&mut self) {
        if !self.player_open && self.player.current().is_none() {
            return;
        }
        self.record_watch_progress();
        self.player.close();
        self.player_open = false;
        self.watch_recording_for = None;
        self.player_origin = None;
        self.player_controls_rect = None;
        self.player_topbar_rect = None;
    }

    /// The renderer stopped — the user stopped it, or the TV did.
    pub(crate) fn on_cast_ended(
        &mut self,
        position: Option<Duration>,
        duration: Option<Duration>,
    ) {
        let device = self.cast_device.take();
        let target = self.cast_last.take();
        self.cast_screen_open = false;
        let deliberate = std::mem::take(&mut self.cast_stop_requested);
        let resume = std::mem::take(&mut self.cast_resume_on_end);

        if !deliberate {
            let name = device
                .map(|device| device.name)
                .unwrap_or_else(|| "the TV".to_string());
            self.set_toast(format!("Stopped casting to {name}"), true);
            return;
        }
        if !resume {
            return;
        }
        let Some(target) = target else {
            return;
        };
        let Some(position) = position.map(|position| position.as_secs_f64()) else {
            return;
        };
        if position < 1.0 || cast_ended_at_the_end(position, duration) {
            return;
        }
        self.play_file_at(target.torrent_id, target.file_id, position);
        self.set_toast(
            format!("Resuming here at {}", fmt::human_duration(position)),
            false,
        );
    }
}

/// The `S01E06` code for the file being cast, matched by file id. A season pack
/// lists every episode, so matching the file is what keeps the remote in step
/// with what the TV is actually playing.
fn episode_code(item: &LibraryItem, file_id: usize) -> Option<String> {
    item.entry
        .release
        .episodes
        .iter()
        .find(|episode| episode.file_id == file_id)
        .and_then(|episode| match (episode.season, episode.episode) {
            (Some(season), Some(number)) => Some(format!("S{season:02}E{number:02}")),
            _ => None,
        })
}

/// What a preparation phase says in one line.
fn prepare_label(phase: PreparePhase) -> &'static str {
    match phase {
        PreparePhase::Connecting => "Contacting the TV\u{2026}",
        PreparePhase::WaitingForStream => "Buffering from peers\u{2026}",
        PreparePhase::Checking => "Checking what the TV can play\u{2026}",
        PreparePhase::Preparing { .. } => "Preparing the video\u{2026}",
        PreparePhase::Starting => "Starting playback\u{2026}",
    }
}

/// The connect story: which steps are done, and which is current.
fn phase_steps(ui: &mut egui::Ui, phase: PreparePhase, width: f32) {
    const LABELS: [&str; 4] = [
        "Contacting the TV",
        "Buffering the video",
        "Preparing the video",
        "Starting playback",
    ];
    let current = match phase {
        PreparePhase::Connecting => 0,
        PreparePhase::WaitingForStream => 1,
        PreparePhase::Checking | PreparePhase::Preparing { .. } => 2,
        PreparePhase::Starting => 3,
    };
    let percent = match phase {
        PreparePhase::Preparing { percent } => percent,
        _ => None,
    };
    ui.allocate_ui(Vec2::new(width, 0.0), |ui| {
        ui.vertical(|ui| {
            for (index, label) in LABELS.iter().enumerate() {
                let (glyph, color) = if index < current {
                    (icons::CHECK.to_string(), theme::OK)
                } else if index == current {
                    (icons::SPINNER.to_string(), theme::ACCENT)
                } else {
                    ("\u{00b7}".to_string(), theme::TEXT_DIM)
                };
                let text = if index == current {
                    match percent {
                        Some(percent) => format!("{label}\u{2026} {percent}%"),
                        None => format!("{label}\u{2026}"),
                    }
                } else {
                    (*label).to_string()
                };
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(glyph).size(12.0).color(color));
                    ui.label(
                        egui::RichText::new(text)
                            .size(12.0)
                            .color(if index == current {
                                theme::TEXT
                            } else {
                                theme::TEXT_DIM
                            }),
                    );
                });
                ui.add_space(2.0);
            }
        });
    });
}

/// A seek slider with monospaced times. Returns the target position when the
/// user releases the handle.
fn seek_scrubber(ui: &mut egui::Ui, position: Option<f64>, total: f64, width: f32) -> Option<f64> {
    if total <= 0.0 {
        return None;
    }
    let mut released = None;
    ui.allocate_ui(Vec2::new(width, 0.0), |ui| {
        ui.spacing_mut().slider_width = width;
        let mut pos = position.unwrap_or(0.0);
        let slider = ui.add(egui::Slider::new(&mut pos, 0.0..=total).show_value(false));
        if slider.drag_stopped() {
            released = Some(pos);
        }
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(fmt::human_duration(pos))
                    .size(11.0)
                    .color(theme::TEXT_DIM),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(fmt::human_duration(total))
                        .size(11.0)
                        .color(theme::TEXT_DIM),
                );
            });
        });
    });
    released
}

/// A thin progress dial around a control, for the shell and mini bar.
///
/// `spinning` draws a fixed 28% arc whose start rotates with `fraction`, for
/// the connecting state; otherwise the arc spans `fraction` of the circle.
fn ring(
    painter: &egui::Painter,
    center: egui::Pos2,
    radius: f32,
    fraction: f32,
    color: Color32,
    spinning: bool,
) {
    painter.circle_stroke(center, radius, egui::Stroke::new(2.0, theme::SURFACE_RAISED));
    let quarter = std::f32::consts::FRAC_PI_2;
    let tau = std::f32::consts::TAU;
    let (start, sweep) = if spinning {
        (fraction.clamp(0.0, 1.0) * tau - quarter, tau * 0.28)
    } else {
        (-quarter, tau * fraction.clamp(0.0, 1.0))
    };
    if sweep.abs() < 0.02 {
        return;
    }
    painter.add(egui::epaint::PathShape::line(
        arc_points(center, radius, start, start + sweep),
        egui::Stroke::new(2.0, color),
    ));
}

fn arc_points(
    center: egui::Pos2,
    radius: f32,
    start: f32,
    end: f32,
) -> Vec<egui::Pos2> {
    let steps = 24;
    let mut points = Vec::with_capacity(steps + 1);
    for index in 0..=steps {
        let t = index as f32 / steps as f32;
        let angle = start + (end - start) * t;
        points.push(egui::pos2(
            center.x + radius * angle.cos(),
            center.y + radius * angle.sin(),
        ));
    }
    points
}

/// Whether a cast that ended had reached the end of the media, so there is
/// nothing worth resuming on this machine.
///
/// A live stream (fragmented remux/transcode) reports its duration as the
/// playhead, so a short bogus total must not read as "finished": a real total
/// is required, and the caller passes the media's true duration.
fn cast_ended_at_the_end(position: f64, duration: Option<Duration>) -> bool {
    duration.is_some_and(|duration| {
        let total = duration.as_secs_f64();
        total > 60.0 && position >= total - 30.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{sample_season_pack, sample_two_season_pack};

    #[test]
    fn the_episode_label_matches_the_file_being_cast() {
        // Not the release's first episode: casting file 2 of a pack is E03.
        let pack = sample_season_pack();
        assert_eq!(episode_code(&pack, 0).as_deref(), Some("S01E01"));
        assert_eq!(episode_code(&pack, 2).as_deref(), Some("S01E03"));
    }

    #[test]
    fn the_episode_label_carries_the_season_from_the_file() {
        // A two-season pack: the file name decides the season, not the pack.
        let pack = sample_two_season_pack();
        assert_eq!(episode_code(&pack, 2).as_deref(), Some("S02E01"));
        assert_eq!(episode_code(&pack, 3).as_deref(), Some("S02E02"));
    }

    #[test]
    fn a_file_with_no_episode_has_no_label() {
        let pack = sample_season_pack();
        assert_eq!(episode_code(&pack, 99), None);
    }

    #[test]
    fn the_end_check_needs_a_real_duration() {
        // A live stream whose reported "duration" follows the playhead must not
        // read as the end; the caller supplies the media's true duration.
        assert!(!cast_ended_at_the_end(300.0, None));
        assert!(!cast_ended_at_the_end(300.0, Some(Duration::from_secs(1200))));
        // A short bogus total is ignored.
        assert!(!cast_ended_at_the_end(30.0, Some(Duration::from_secs(35))));
        // A real total, near the end: finished.
        assert!(cast_ended_at_the_end(1190.0, Some(Duration::from_secs(1200))));
    }
}
