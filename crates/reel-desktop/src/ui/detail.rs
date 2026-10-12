//! The Work (title) screen: identity, releases and episodes.

use super::*;

// ------------------------------------------------------------------ detail

impl App {
    pub(crate) fn detail_screen(&mut self, ui: &mut egui::Ui) {
        let Some(work) = self.selected_work() else {
            ui.label("That title is no longer in the library.");
            if ui.button("Back").clicked() {
                self.back();
            }
            return;
        };

        let mut action: Option<DetailAction> = None;
        let work_id = work.lead_torrent_id();

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal_top(|ui| {
                // Left column: what this is, and the one primary action.
                ui.allocate_ui_with_layout(
                    Vec2::new(WORK_LEFT_COL, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| self.work_identity_column(ui, &work, &mut action),
                );
                ui.add_space(18.0);
                ui.separator();
                ui.add_space(18.0);
                // Right column: releases, or episodes and their sources.
                ui.vertical(|ui| {
                    if work.is_series() {
                        self.work_episode_list(ui, &work, &mut action);
                        self.work_sources(ui, &work, &mut action);
                    } else {
                        self.work_release_list(ui, &work, &mut action);
                    }
                    self.storage_line(ui, &work);
                });
            });
        });

        match action {
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
            Some(DetailAction::Download {
                key,
                torrent_id,
                file_id,
            }) => {
                self.backend.download_media(vec![MediaDownload {
                    key,
                    torrent_id,
                    files: vec![file_id],
                }]);
                self.invalidate();
            }
            Some(DetailAction::CancelDownload {
                key,
                torrent_id,
                file_id,
            }) => {
                self.backend.stop_media_download(vec![MediaDownload {
                    key,
                    torrent_id,
                    files: vec![file_id],
                }]);
                self.invalidate();
            }
            Some(DetailAction::DownloadEpisodes(items)) => {
                self.backend.download_media(items);
                self.invalidate();
            }
            None => {}
        }
    }

    /// The left column of a Work view: identity, then Play and a "More" menu.
    pub(crate) fn work_identity_column(
        &self,
        ui: &mut egui::Ui,
        work: &Work,
        action: &mut Option<DetailAction>,
    ) {
        work_identity(ui, work);
        ui.add_space(12.0);

        let lead = self.lead_item(work);
        let resumable = work.resume_position();
        let has_playable = if work.is_series() {
            !work.episodes().is_empty()
        } else {
            !work.versions().is_empty()
        };
        let verb = match resumable {
            Some(position) => format!("Resume {}", fmt::human_duration(position)),
            None => "Play".to_string(),
        };
        // Where playback will go is part of the label: a connected renderer is
        // the target, not this machine.
        let play_label = format!("\u{25b6}  {verb}{}", self.cast_suffix());
        let play_hint = match self.cast_device_name() {
            Some(name) => format!("Plays on {name}"),
            None => "Plays here".to_string(),
        };
        if ui
            .add_enabled(
                has_playable,
                egui::Button::new(egui::RichText::new(play_label).size(15.0))
                    .min_size(Vec2::new(ui.available_width(), 34.0)),
            )
            .on_hover_text(play_hint)
            .clicked()
        {
            *action = Some(DetailAction::PlayWork);
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.menu_button("More", |ui| {
                if resumable.is_some() && ui.button("Start over").clicked() {
                    *action = Some(DetailAction::PlayWorkFromStart);
                    ui.close();
                }
                if ui.button("Mark watched").clicked() {
                    *action = Some(DetailAction::MarkWatched);
                    ui.close();
                }
                if let Some(file) = lead.as_ref().and_then(|item| item.torrent.primary_file()) {
                    let url = file
                        .stream
                        .url
                        .clone()
                        .unwrap_or_else(|| file.stream.path.clone());
                    if ui
                        .button("Copy stream URL")
                        .on_hover_text(url.clone())
                        .clicked()
                    {
                        ui.ctx().copy_text(url);
                        ui.close();
                    }
                }
                if ui.button("Remove").clicked() {
                    *action = Some(DetailAction::OpenRemove);
                    ui.close();
                }
            });
            let copies = work.torrent_count();
            ui.label(
                egui::RichText::new(if copies > 1 {
                    format!("{copies} copies")
                } else {
                    "1 copy".to_string()
                })
                .size(11.0)
                .color(theme::TEXT_DIM),
            );
        });

        if let Some(item) = lead.as_ref() {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                stat(
                    ui,
                    "downloaded",
                    &fmt::human_bytes(item.torrent.stats.progress_bytes),
                );
                stat(
                    ui,
                    "down",
                    &fmt::human_rate(item.torrent.stats.download_bps),
                );
            });
            ui.horizontal(|ui| {
                stat(ui, "peers", &item.torrent.stats.peers.live.to_string());
                stat(ui, "eta", &fmt::human_eta(item.torrent.stats.eta_seconds));
            });
            if let Some(error) = item.torrent.stats.error.as_deref() {
                ui.add_space(4.0);
                ui.colored_label(theme::DANGER, error);
            }
        }
    }

    /// The film view: one release per copy, best first, each with its own controls.
    pub(crate) fn work_release_list(
        &mut self,
        ui: &mut egui::Ui,
        work: &Work,
        action: &mut Option<DetailAction>,
    ) {
        let versions = work.versions();
        section_header(ui, format!("Releases ({})", versions.len()));
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(
                "Play a release, keep one on disk, or remove it. Best first; each is independent.",
            )
            .color(theme::TEXT_DIM)
            .size(11.0),
        );
        ui.add_space(6.0);
        self.pause_notice(ui, work);

        for (index, version) in versions.iter().enumerate() {
            // A copy is "kept" because the user asked for that copy, not because
            // the title as a whole is a download.
            let item = self.item(version.torrent_id);
            let state = item
                .as_ref()
                .map(|item| download_state(item, version.file_id))
                .unwrap_or(DownloadState {
                    kept: false,
                    done: false,
                    fraction: 0.0,
                });
            let paused = item
                .as_ref()
                .is_some_and(|item| item.torrent.stats.is_paused());
            let finished = item.as_ref().is_some_and(|item| item.torrent.finished);

            egui::Frame::NONE
                .fill(theme::SURFACE)
                .inner_margin(egui::Margin::symmetric(12, 10))
                .corner_radius(theme::CARD_RADIUS)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            let quality = version.attributes.quality_label();
                            let label = if index == 0 {
                                format!("{quality}  \u{00b7}  best")
                            } else {
                                quality
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
                                &movie_key(work),
                                version.torrent_id,
                                version.file_id,
                                state,
                                action,
                            );
                            if ui
                                .add_enabled(
                                    !finished,
                                    egui::Button::new(if paused { "Resume" } else { "Pause" })
                                        .small(),
                                )
                                .clicked()
                            {
                                *action =
                                    Some(DetailAction::SetPaused(version.torrent_id, !paused));
                            }
                            if ui
                                .small_button("Remove")
                                .on_hover_text("Remove this release")
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
    pub(crate) fn pause_notice(&self, ui: &mut egui::Ui, work: &Work) {
        let downloading = work.members.iter().any(|member| {
            self.item(member.torrent_id())
                .is_some_and(|item| item.downloading)
        });
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
    pub(crate) fn extras_list(&self, ui: &mut egui::Ui, work: &Work) {
        let extras = work.extras();
        if extras.is_empty() {
            return;
        }
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("Extras")
                .size(13.0)
                .color(theme::TEXT_DIM),
        );
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
    pub(crate) fn work_episode_list(
        &mut self,
        ui: &mut egui::Ui,
        work: &Work,
        action: &mut Option<DetailAction>,
    ) {
        let episodes = work.episodes();
        let seasons = work.seasons();
        section_header(ui, plural(episodes.len(), "episode"));

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
        self.pause_notice(ui, work);
        ui.add_space(6.0);

        // Season selector: one row of tabs instead of a stack of sections.
        let mut chosen = self
            .search_series_views
            .get(&work.key)
            .and_then(|view| view.season);
        if chosen.is_none() {
            chosen = seasons
                .iter()
                .map(|season| season.number)
                .find(|number| number.is_some())
                .flatten()
                .or_else(|| seasons.first().and_then(|season| season.number));
        }
        if seasons.len() > 1 {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new("Season")
                        .size(11.0)
                        .color(theme::TEXT_DIM),
                );
                for season in &seasons {
                    let label = match season.number {
                        Some(number) => number.to_string(),
                        None => "Specials".to_string(),
                    };
                    if ui
                        .selectable_label(season.number == chosen, label)
                        .clicked()
                    {
                        chosen = season.number;
                    }
                }
            });
        }
        // Remember the choice; the search side shares the same key.
        {
            let entry = self
                .search_series_views
                .entry(work.key.clone())
                .or_default();
            entry.season = chosen;
        }

        ui.horizontal(|ui| {
            if ui
                .small_button("Download season")
                .on_hover_text("Fetch the best copy of every episode in this season")
                .clicked()
            {
                let downloads = self.season_downloads(work, chosen);
                *action = Some(DetailAction::DownloadEpisodes(downloads));
            }
            if ui
                .small_button("Download all episodes")
                .on_hover_text("Fetch the best copy of every episode, in every season")
                .clicked()
            {
                let all: Vec<MediaDownload> = episodes
                    .iter()
                    .filter_map(|episode| {
                        let variant = self.chosen_variant(work, episode)?;
                        Some(MediaDownload {
                            key: episode_key(work, episode),
                            torrent_id: variant.torrent_id,
                            files: vec![variant.file_id],
                        })
                    })
                    .collect();
                *action = Some(DetailAction::DownloadEpisodes(all));
            }
        });
        ui.add_space(6.0);

        for season in &seasons {
            if season.number != chosen {
                continue;
            }
            for episode in &season.episodes {
                self.episode_row(ui, work, episode, action);
            }
        }

        self.extras_list(ui, work);
    }

    /// One episode row, with a copy chooser when the library holds several.
    pub(crate) fn episode_row(
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
                .and_then(|item| item.entry.watch_by_file.get(&variant.file_id).cloned())
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

        let key = episode_key(work, episode);
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
        let chosen_pair = chosen
            .as_ref()
            .map(|variant| (variant.torrent_id, variant.file_id));

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
                            } else if progress.is_resumable() {
                                // Below the resume threshold Play starts from
                                // the beginning, so do not promise a resume.
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
                            let key = episode_key(work, episode);
                            if ui.small_button("Play").clicked() {
                                *action = Some(DetailAction::Play(torrent_id, file_id));
                            }
                            download_buttons(ui, &key, torrent_id, file_id, state, action);
                        }

                        if variants.len() > 1 {
                            ui.menu_button(format!("copy: {quality}"), |ui| {
                                for variant in &variants {
                                    let selected =
                                        chosen_pair == Some((variant.torrent_id, variant.file_id));
                                    let label = format!(
                                        "{}  \u{00b7}  {}",
                                        variant.attributes.quality_label(),
                                        fmt::human_bytes(variant.size)
                                    );
                                    if ui.selectable_label(selected, label).clicked() {
                                        self.backend.choose_source(
                                            key.clone(),
                                            variant.torrent_id,
                                            variant.file_id,
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
    pub(crate) fn work_sources(&mut self, ui: &mut egui::Ui, work: &Work, action: &mut Option<DetailAction>) {
        ui.add_space(16.0);
        ui.separator();
        ui.add_space(10.0);
        section_header(ui, format!("Sources ({})", work.torrent_count()));
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
                                egui::RichText::new(reel_core::title::truncate(
                                    &item.heading(),
                                    56,
                                ))
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
    pub(crate) fn storage_line(&self, ui: &mut egui::Ui, work: &Work) {
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

pub(crate) enum DetailAction {
    OpenRemove,
    RemoveSource(usize),
    SetPaused(usize, bool),
    MarkWatched,
    Play(usize, usize),
    PlayWork,
    PlayWorkFromStart,
    Download {
        key: String,
        torrent_id: usize,
        file_id: usize,
    },
    CancelDownload {
        key: String,
        torrent_id: usize,
        file_id: usize,
    },
    /// Keep every episode of a season (or the whole work) at once.
    DownloadEpisodes(Vec<MediaDownload>),
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
        .filter(
            |file| match (episode_token(&file.name), video_token.as_deref()) {
                // The episode code is the strongest signal: it keeps S01E03's
                // subtitles away from S01E02 even in a shared folder.
                (Some(sub), Some(token)) => sub == token,
                _ => {
                    dir_of(&file.path) == video_dir
                        && shares_title_prefix(&file_stem(&file.name), &video_stem)
                }
            },
        )
        .map(|file| file.id)
        .collect()
}

/// The `SxxEyy` episode code in a name, lowercased, if there is one.
pub(crate) fn episode_token(name: &str) -> Option<String> {
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
pub(crate) fn file_stem(name: &str) -> String {
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
pub(crate) fn shares_title_prefix(a: &str, b: &str) -> bool {
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
pub(crate) fn dir_of(path: &str) -> String {
    match path.rsplit_once(['/', '\\']) {
        Some((dir, _)) => dir.to_ascii_lowercase(),
        None => String::new(),
    }
}

/// The shape providers publish artwork in.
pub(crate) fn source_aspect(kind: ArtworkKind) -> f32 {
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
pub(crate) fn plural(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

/// Whether the player is doing something that legitimately produces no new
/// frames right now: still starting up, waiting for the demuxer cache, paused,
/// finished, or buffering.
pub(crate) fn stream_looks_busy(state: &reel_player::PlayerState) -> bool {
    !state.loaded
        || state.paused
        || state.eof
        || state.idle
        || state.paused_for_cache
        || state
            .buffering_percent
            .is_some_and(|percent| percent < 100.0)
}

/// Decide whether playback looks wedged rather than merely slow.
///
/// A wedge is narrow: nothing is loading or buffering, the torrent is not
/// delivering bytes, no seek is in flight, and *both* the picture (frame count)
/// and the playback clock (position) have stopped. A slow torrent, a rebuffer, a
/// still scene and a seek each fail at least one of those, so they are never
/// mistaken for a wedge.
#[allow(clippy::too_many_arguments)]
pub(crate) fn playback_is_wedged(
    busy: bool,
    fetching: bool,
    frames: u64,
    last_frames: u64,
    position: f64,
    last_position: f64,
    recent_seek: bool,
) -> bool {
    if busy || fetching || recent_seek {
        return false;
    }
    frames == last_frames && (position - last_position).abs() < 0.25
}

/// Where playback of one file should start.
///
/// The file's own position is the answer whenever there is one. The torrent's
/// whole-title position is only borrowed by a **film**, whose single feature is
/// the only thing its position can mean: for a series it would apply episode 1's
/// position to episode 2, which is exactly the bug this separates.
pub fn resume_start_at(item: &LibraryItem, file_id: usize) -> Option<f64> {
    let per_file = item
        .entry
        .watch_by_file
        .get(&file_id)
        .filter(|progress| progress.is_resumable())
        .map(|progress| progress.position);
    per_file.or_else(|| {
        (!item.entry.is_series())
            .then(|| item.resume_position())
            .flatten()
    })
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
pub(crate) fn download_buttons(
    ui: &mut egui::Ui,
    key: &str,
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
                *action = Some(DetailAction::CancelDownload {
                    key: key.to_string(),
                    torrent_id,
                    file_id,
                });
            }
        } else if ui
            .small_button("Stop download")
            .on_hover_text("Cancel this download")
            .clicked()
        {
            *action = Some(DetailAction::CancelDownload {
                key: key.to_string(),
                torrent_id,
                file_id,
            });
        }
    } else if ui
        .small_button("Download")
        .on_hover_text("Keep this on disk")
        .clicked()
    {
        *action = Some(DetailAction::Download {
            key: key.to_string(),
            torrent_id,
            file_id,
        });
    }
}

/// Whether any of a work's members is the hit with the given key.
pub(crate) fn work_contains_hit(work: &Work, shown: &[SearchHit], anchor: &str) -> bool {
    work.members.iter().any(|member| {
        shown
            .get(member.torrent_id())
            .is_some_and(|hit| hit.key() == anchor)
    })
}

/// Poster, title, facts and overview: the identity block the library Work view
/// and the search Result view share.
pub(crate) fn work_identity(ui: &mut egui::Ui, work: &Work) {
    let (poster, _) = ui.allocate_exact_size(Vec2::new(200.0, 300.0), Sense::hover());
    App::paint_art(
        ui,
        poster,
        work.poster(),
        &work.title,
        ArtworkKind::Poster,
        theme::CARD_RADIUS,
    );
    ui.add_space(12.0);
    page_title(ui, work.heading());
    ui.add_space(6.0);

    // Facts as chips, streaming-app style.
    ui.horizontal_wrapped(|ui| {
        chip(
            ui,
            if work.is_series() { "Series" } else { "Film" },
            theme::ACCENT,
        );
        if let Some(metadata) = work.metadata.as_ref() {
            if let Some(rating) = display_rating(metadata) {
                chip(ui, &format!("{} {rating:.1}", icons::STAR), theme::WARN);
            }
            if let Some(runtime) = metadata.runtime_label() {
                neutral_chip(ui, &runtime);
            }
        }
        let count = if work.is_series() {
            plural(work.episodes().len(), "episode")
        } else {
            plural(work.versions().len(), "release")
        };
        neutral_chip(ui, &count);
    });

    if let Some(metadata) = work.metadata.as_ref() {
        if !metadata.genres.is_empty() {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for genre in &metadata.genres {
                    neutral_chip(ui, genre);
                }
            });
        }
        if let Some(overview) = metadata.overview.as_deref() {
            ui.add_space(8.0);
            ui.label(egui::RichText::new(overview).size(12.5).color(theme::TEXT));
        }
    }
}

