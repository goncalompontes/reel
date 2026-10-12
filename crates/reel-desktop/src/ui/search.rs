//! Discover and the per-result screen: search, grouping and filters.

use super::*;

// ------------------------------------------------------------------ search

impl App {
    /// Discover before any search: a landing page with one big, centred field.
    pub(crate) fn discover_landing(&mut self, ui: &mut egui::Ui, sources: &[SourceInfo]) {
        let width = ui.available_width().min(680.0);
        ui.add_space(72.0);
        ui.vertical_centered(|ui| {
            ui.label(
                egui::RichText::new("What do you want to watch?")
                    .size(30.0)
                    .family(theme::semibold(ui.ctx())),
            );
            ui.add_space(8.0);
            caption(ui, "Search every configured source for a film or series.");
            ui.add_space(26.0);

            let field = ui.add_sized(
                [width, 46.0],
                egui::TextEdit::singleline(&mut self.search_query)
                    .id(egui::Id::new("discover-search"))
                    .hint_text("Search titles\u{2026}")
                    .font(egui::FontId::proportional(18.0))
                    .horizontal_align(egui::Align::Center),
            );
            if self.focus_search {
                field.request_focus();
                self.focus_search = false;
            }
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

            ui.add_space(14.0);
            let submit = ui
                .add_sized(
                    [150.0, 36.0],
                    egui::Button::new(
                        egui::RichText::new("Search")
                            .size(14.0)
                            .family(theme::semibold(ui.ctx())),
                    )
                    .fill(theme::ACCENT_DEEP)
                    .corner_radius(theme::CONTROL_RADIUS),
                )
                .clicked()
                || enter;
            if submit {
                self.submit_search();
            }

            if !sources.is_empty() {
                ui.add_space(22.0);
                let names: Vec<&str> = sources.iter().map(|source| source.name.as_str()).collect();
                ui.label(
                    egui::RichText::new(format!("Sources: {}", names.join("  \u{00b7}  ")))
                        .size(11.0)
                        .color(theme::TEXT_DIM),
                );
            }
        });
    }

    pub(crate) fn search_screen(&mut self, ui: &mut egui::Ui) {
        let sources = self.backend.search_sources();

        // Before the first search, Discover is a landing page with one big,
        // centred field. Afterwards it is a results view with compact controls.
        if self.search_for.is_none() && !self.searching {
            self.discover_landing(ui, &sources);
            return;
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            page_title(ui, "Discover");
            ui.add_space(10.0);
            let field = ui.add_sized(
                [380.0, 30.0],
                egui::TextEdit::singleline(&mut self.search_query)
                    .id(egui::Id::new("discover-search"))
                    .hint_text("Search titles\u{2026}"),
            );
            if self.focus_search {
                field.request_focus();
                self.focus_search = false;
            }
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("Search").clicked() || enter {
                self.submit_search();
            }
            if !self.search_hits.is_empty() {
                ui.add_space(4.0);
                neutral_chip(ui, &plural(self.search_hits.len(), "result"));
            }
        });
        ui.add_space(8.0);

        // The toolbar: filters, sort, and how results are laid out.
        let active_filters = [
            !self.search_year.trim().is_empty(),
            !self.search_season.trim().is_empty(),
            !self.search_episode.trim().is_empty(),
            !self.search_min_seeders.trim().is_empty(),
        ]
        .iter()
        .filter(|active| **active)
        .count();

        ui.horizontal(|ui| {
            let filters_label = if active_filters > 0 {
                format!("{}  Filters \u{00b7} {active_filters}", icons::FUNNEL)
            } else {
                format!("{}  Filters", icons::FUNNEL)
            };
            if ui
                .add(
                    egui::Button::new(egui::RichText::new(filters_label).size(13.0))
                        .selected(self.filters_open)
                        .corner_radius(theme::CONTROL_RADIUS),
                )
                .clicked()
            {
                self.filters_open = !self.filters_open;
            }

            egui::ComboBox::from_id_salt("search-sort")
                .selected_text(self.search_sort.label())
                .show_ui(ui, |ui| {
                    for sort in [
                        SearchSort::Relevance,
                        SearchSort::Seeders,
                        SearchSort::Size,
                        SearchSort::Newest,
                    ] {
                        if ui
                            .selectable_value(&mut self.search_sort, sort, sort.label())
                            .clicked()
                        {
                            // Re-sort and redraw now rather than on the next input.
                            ui.ctx().request_repaint();
                        }
                    }
                });

            if self.searching || self.search_streaming {
                ui.spinner();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let glyph = match self.search_layout {
                    SearchLayout::List => icons::LIST,
                    SearchLayout::Grid => icons::SQUARES_FOUR,
                };
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new(format!(
                                "{glyph}  {}",
                                self.search_layout.label()
                            ))
                            .size(13.0),
                        )
                        .corner_radius(theme::CONTROL_RADIUS),
                    )
                    .on_hover_text("Switch between a poster grid and a compact list")
                    .clicked()
                {
                    self.search_layout = match self.search_layout {
                        SearchLayout::Grid => SearchLayout::List,
                        SearchLayout::List => SearchLayout::Grid,
                    };
                    // Row heights are not interchangeable between layouts.
                    self.search_work_heights.clear();
                }
            });
        });

        // Expanded filter fields.
        if self.filters_open {
            ui.add_space(6.0);
            egui::Frame::new()
                .fill(theme::SURFACE)
                .corner_radius(theme::CONTROL_RADIUS)
                .inner_margin(egui::Margin::symmetric(12, 10))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        labelled_field(ui, "Year", &mut self.search_year, 66.0);
                        labelled_field(ui, "Season", &mut self.search_season, 66.0);
                        labelled_field(ui, "Episode", &mut self.search_episode, 66.0);
                        labelled_field(ui, "Min seeders", &mut self.search_min_seeders, 84.0);
                        ui.add_space(4.0);
                        if ghost_button(ui, "Clear").clicked() {
                            self.search_year.clear();
                            self.search_season.clear();
                            self.search_episode.clear();
                            self.search_min_seeders.clear();
                        }
                    });
                });
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if sources.is_empty() {
                ui.colored_label(theme::WARN, "No search sources are configured.");
            } else {
                for source in &sources {
                    let color = if source.configured {
                        theme::OK
                    } else {
                        theme::TEXT_DIM
                    };
                    chip(ui, &source.name, color);
                    if !source.configured {
                        caption(ui, "disabled");
                    }
                }
            }
        });

        if let Some(note) = self.search_note.clone() {
            ui.add_space(4.0);
            ui.label(egui::RichText::new(note).size(11.5).color(theme::TEXT_DIM));
        }

        ui.add_space(10.0);

        // Everything below is derived from the hits, and deriving it is not
        // cheap: every release name is parsed, every hit's metadata is cloned,
        // and the hits are regrouped into works. Cache the result and rebuild it
        // only when an input changes, never on a plain redraw.
        let signature = self.search_signature();
        let stale = self
            .search_view
            .as_ref()
            .is_none_or(|view| view.signature != signature);
        if stale {
            let rebuilt = self.build_search_view(signature);
            self.search_view = Some(rebuilt);
        }

        // Take the view out of `self` so the card renderers, which need
        // `&mut self`, can borrow it while drawing. It goes back afterwards.
        let view = match self.search_view.take() {
            Some(view) => view,
            None => return,
        };

        if view.shown.is_empty() {
            let (title, body) = if self.search_failed {
                ("The search failed", "Check the message above and try again.")
            } else if self.search_for.is_some() && !self.search_hits.is_empty() {
                (
                    "No results match the filters",
                    "Clear a filter or widen the seeders minimum.",
                )
            } else if self.search_for.is_some() {
                (
                    "Nothing matched",
                    "Try another spelling, a year, or a different source.",
                )
            } else {
                (
                    "Search for something to watch",
                    "Type a title in the search field above and press Enter.",
                )
            };
            self.search_view = Some(view);
            empty_state(ui, title, body);
            return;
        }

        // Selecting a row or card pushes the Result view. Only what is on
        // screen is laid out; measured heights are reserved for the rest.
        let mut open: Option<String> = None;
        match self.search_layout {
            SearchLayout::Grid => {
                const CARD_W: f32 = 168.0;
                const CARD_H: f32 = 252.0;
                const GAP: f32 = 14.0;
                let available = ui.available_width();
                let per_row = (((available + GAP) / (CARD_W + GAP)).floor() as usize).max(1);
                let rows = view.works.len().div_ceil(per_row);
                let mut heights = std::mem::take(&mut self.search_work_heights);
                virtual_list(ui, rows, &mut heights, |ui, row| {
                    ui.horizontal(|ui| {
                        for work in view.works.iter().skip(row * per_row).take(per_row) {
                            let hit = self.card(ui, work, CARD_W, CARD_H);
                            if hit.double_clicked || hit.clicked {
                                open = Some(work.key.clone());
                            }
                            ui.add_space(GAP);
                        }
                    });
                });
                self.search_work_heights = heights;
            }
            SearchLayout::List => {
                let mut heights = std::mem::take(&mut self.search_work_heights);
                virtual_list(ui, view.works.len(), &mut heights, |ui, index| {
                    self.search_work_row(ui, &view.works[index], &mut open);
                });
                self.search_work_heights = heights;
            }
        }
        // Anchor the Result view on a member hit, which survives the work's
        // identity changing as metadata lands.
        let anchor = open.as_deref().and_then(|key| {
            view.works
                .iter()
                .find(|work| work.key == key)
                .and_then(|work| work.members.first())
                .and_then(|member| view.shown.get(member.torrent_id()))
                .map(SearchHit::key)
        });
        self.search_view = Some(view);

        if let Some(key) = open {
            self.result_anchor = anchor;
            self.nav.open(Screen::Result(key));
        }
    }

    /// One Discover row: a title, its kind, and what it holds. Clicking it
    /// opens the Result view rather than expanding in place.
    pub(crate) fn search_work_row(&mut self, ui: &mut egui::Ui, work: &Work, open: &mut Option<String>) {
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 96.0), Sense::click());
        // Expose the row as one labelled button: its text is painted, so
        // without this it would be invisible to AccessKit (and to the tests).
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), work.heading())
        });
        let painter = ui.painter().clone();
        painter.rect_filled(
            rect,
            theme::CARD_RADIUS,
            if response.hovered() {
                theme::SURFACE_RAISED
            } else {
                theme::SURFACE
            },
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

        // Laid out by hand, exactly like the library card, so the row's own
        // response is the only interactive widget over it.
        let pad = 12.0;
        let poster = Rect::from_min_size(
            egui::pos2(rect.left() + pad, rect.top() + 10.0),
            Vec2::new(51.0, 76.0),
        );
        Self::paint_art(
            ui,
            poster,
            work.poster(),
            &work.title,
            ArtworkKind::Poster,
            theme::CARD_RADIUS,
        );

        // Painted rather than added as labels: `ui.put` lays a label out
        // centred, and a child Ui would sit on top of the row and steal its
        // hover. The painter gives exact, left-aligned text with nothing
        // interactive in the way.
        let text_left = poster.right() + 14.0;
        let galley = painter.layout_no_wrap(
            reel_core::title::truncate(&work.heading(), 70),
            egui::FontId::new(15.5, theme::semibold(ui.ctx())),
            theme::TEXT,
        );
        painter.galley(egui::pos2(text_left, rect.top() + 12.0), galley, theme::TEXT);

        // A pill row: kind, rating, runtime, quality, release count.
        let mut pills: Vec<(String, Color32)> = Vec::new();
        pills.push((
            if work.is_series() { "Series" } else { "Film" }.to_string(),
            theme::ACCENT,
        ));
        if let Some(metadata) = work.metadata.as_ref() {
            if let Some(rating) = display_rating(metadata) {
                pills.push((format!("{} {rating:.1}", icons::STAR), theme::WARN));
            }
            if let Some(runtime) = metadata.runtime_label() {
                pills.push((runtime, theme::TEXT_DIM));
            }
        }
        if let Some(best) = work
            .members
            .iter()
            .max_by_key(|member| member.entry.release.attributes.quality_rank())
        {
            let quality = best.attributes().quality_label();
            if !quality.is_empty() && quality != "quality unknown" {
                pills.push((quality, theme::TEXT));
            }
        }
        pills.push((plural(work.torrent_count(), "release"), theme::TEXT_DIM));

        let mut x = text_left;
        let pill_y = rect.top() + 40.0;
        for (text, color) in pills {
            let r = Self::paint_pill(
                ui,
                egui::pos2(x, pill_y),
                &text,
                color,
                theme::SURFACE_RAISED,
            );
            x = r.right() + 6.0;
        }

        if let Some(metadata) = work.metadata.as_ref() {
            let mut line = metadata.genres.join(" \u{00b7} ");
            if let Some(overview) = metadata.overview.as_deref() {
                if !line.is_empty() {
                    line.push_str("   \u{00b7}   ");
                }
                line.push_str(overview);
            }
            if !line.is_empty() {
                painter.text(
                    egui::pos2(text_left, rect.top() + 64.0),
                    egui::Align2::LEFT_TOP,
                    reel_core::title::truncate(&line, 110),
                    egui::FontId::proportional(11.0),
                    theme::TEXT_DIM,
                );
            }
        }

        if response.clicked() {
            *open = Some(work.key.clone());
        }
    }

    /// A search title opened from Discover: its releases, and the actions that
    /// used to live on the expanding card.
    pub(crate) fn search_result_screen(&mut self, ui: &mut egui::Ui, key: &str) {
        // Discover is not drawn while this view is open, so nothing else would
        // refresh the derived view as more hits and metadata arrive. Rebuild it
        // here on the same signature rule.
        let signature = self.search_signature();
        let stale = self
            .search_view
            .as_ref()
            .is_none_or(|view| view.signature != signature);
        if stale {
            let rebuilt = self.build_search_view(signature);
            self.search_view = Some(rebuilt);
        }

        // Take the derived view out so the renderers can borrow `self`.
        let Some(view) = self.search_view.take() else {
            self.search_result_missing(ui);
            return;
        };
        // Find by key, or by the anchoring hit when the work's identity has
        // changed under it as metadata arrived.
        let work = view
            .works
            .iter()
            .find(|work| work.key.as_str() == key)
            .or_else(|| {
                let anchor = self.result_anchor.as_deref()?;
                view.works
                    .iter()
                    .find(|work| work_contains_hit(work, &view.shown, anchor))
            })
            .cloned();
        let Some(work) = work else {
            self.search_view = Some(view);
            self.search_result_missing(ui);
            return;
        };

        let mut actions = SearchActions::default();

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal_top(|ui| {
                // Left column: identity and the one primary action.
                ui.allocate_ui_with_layout(
                    Vec2::new(WORK_LEFT_COL, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        work_identity(ui, &work);
                        ui.add_space(12.0);
                        let best = work.members.iter().max_by_key(|member| {
                            let seeders = view
                                .shown
                                .get(member.torrent_id())
                                .and_then(|hit| hit.seeders)
                                .unwrap_or(0);
                            (member.entry.release.attributes.quality_rank(), seeders)
                        });
                        if let Some(hit) =
                            best.and_then(|member| view.shown.get(member.torrent_id()))
                        {
                            let can_play = hit.is_usable() || hit.detail_url.is_some();
                            let pending = self.search_actions.get(&hit.key()).copied();
                            let suffix = self.cast_suffix();
                            if let Some(phase) = pending {
                                search_action_indicator(ui, phase);
                            }
                            if ui
                                .add_enabled(
                                    can_play && pending.is_none(),
                                    egui::Button::new(
                                        egui::RichText::new(format!(
                                            "{}  Play best{suffix}",
                                            icons::PLAY
                                        ))
                                        .size(14.0)
                                        .family(theme::semibold(ui.ctx())),
                                    )
                                    .fill(theme::ACCENT_DEEP)
                                    .corner_radius(theme::CONTROL_RADIUS)
                                    .min_size(Vec2::new(ui.available_width(), 34.0)),
                                )
                                .on_hover_text("Add and play the best release of this title")
                                .clicked()
                            {
                                actions.play = Some(hit.clone());
                            }
                        }
                    },
                );
                ui.add_space(18.0);
                ui.separator();
                ui.add_space(18.0);
                // Right column: the title's releases, grouped by kind.
                ui.vertical(|ui| {
                    self.search_work_categories(
                        ui,
                        &work,
                        &view.shown,
                        &view.releases,
                        &mut actions,
                    );
                });
            });
        });

        self.search_view = Some(view);
        self.apply_search_actions(ui, actions);
    }

    pub(crate) fn search_result_missing(&mut self, ui: &mut egui::Ui) {
        empty_state(
            ui,
            "This title is no longer in the current results.",
            "The results may have changed since it was opened.",
        );
        ui.add_space(8.0);
        if ui.button("Back to results").clicked() {
            self.back();
        }
    }

    pub(crate) fn apply_search_actions(&mut self, ui: &egui::Ui, actions: SearchActions) {
        if let Some(url) = actions.open {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
        }
        if let Some(magnet) = actions.copy {
            ui.ctx().copy_text(magnet);
            self.set_toast("Magnet copied", false);
        }
        if let Some(hit) = actions.add {
            self.start_search_action(hit, false);
        }
        if let Some(hit) = actions.play {
            self.start_search_action(hit, true);
        }
    }

    /// The categories inside a work: resolution tiers for a film, seasons for a
    /// series. Each is a section of torrent rows.
    pub(crate) fn search_work_categories(
        &mut self,
        ui: &mut egui::Ui,
        work: &Work,
        shown: &[SearchHit],
        releases: &[Release],
        actions: &mut SearchActions,
    ) {
        if work.is_series() {
            self.search_series_categories(ui, work, shown, actions);
            return;
        }

        // A film: resolution tiers, best first.
        let mut categories: Vec<(i64, String, Vec<usize>)> = Vec::new();
        for member in &work.members {
            let index = member.torrent_id();
            let release = &member.entry.release;
            let (order, name) = resolution_category(release.attributes.resolution.as_deref());
            match categories
                .iter_mut()
                .find(|(_, existing, _)| *existing == name)
            {
                Some((_, _, indices)) => indices.push(index),
                None => categories.push((order, name, vec![index])),
            }
        }
        categories.sort_by_key(|(order, _, _)| *order);

        for (_, name, mut indices) in categories {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(&name)
                        .size(13.0)
                        .family(theme::semibold(ui.ctx()))
                        .color(theme::TEXT),
                );
                neutral_chip(ui, &plural(indices.len(), "release"));
            });
            ui.add_space(2.0);

            // `shown` is already in the chosen order, and an index is its
            // position in `shown`, so ascending index is the sorted order.
            indices.sort_unstable();
            for index in indices {
                self.search_grouped_row(ui, &shown[index], &releases[index], false, actions);
            }
            ui.add_space(4.0);
        }
    }

    /// A series: seasons as tabs, then complete-season packs versus single
    /// episodes, then resolution groups.
    pub(crate) fn search_series_categories(
        &mut self,
        ui: &mut egui::Ui,
        work: &Work,
        shown: &[SearchHit],
        actions: &mut SearchActions,
    ) {
        // Seasons in order, as positions into `work.members`.
        let mut seasons: Vec<(u32, Vec<usize>)> = Vec::new();
        for (position, member) in work.members.iter().enumerate() {
            let season = member.entry.release.season.unwrap_or(0);
            match seasons.iter_mut().find(|(existing, _)| *existing == season) {
                Some((_, positions)) => positions.push(position),
                None => seasons.push((season, vec![position])),
            }
        }
        seasons.sort_by_key(|(season, _)| *season);

        let key = work.key.clone();
        let state = self
            .search_series_views
            .get(&key)
            .cloned()
            .unwrap_or_default();
        // Default to a real season, not the "Specials" bucket.
        let mut chosen_season = state.season.or_else(|| {
            seasons
                .iter()
                .map(|(season, _)| *season)
                .find(|season| *season > 0)
                .or_else(|| seasons.first().map(|(season, _)| *season))
        });

        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("Seasons")
                    .size(11.0)
                    .color(theme::TEXT_DIM),
            );
            for (season, _) in &seasons {
                if ui
                    .selectable_label(Some(*season) == chosen_season, season_label(*season))
                    .clicked()
                {
                    chosen_season = Some(*season);
                }
            }
        });

        let Some(season) = chosen_season else {
            return;
        };
        let positions: Vec<usize> = seasons
            .iter()
            .find(|(existing, _)| *existing == season)
            .map(|(_, positions)| positions.clone())
            .unwrap_or_default();

        // A release with no episode number is a whole-season pack.
        let packs: Vec<usize> = positions
            .iter()
            .copied()
            .filter(|&position| is_complete_season(&work.members[position]))
            .collect();
        let episodes: Vec<usize> = positions
            .iter()
            .copied()
            .filter(|&position| !is_complete_season(&work.members[position]))
            .collect();

        let mut chosen_kind = state.kind;
        if chosen_kind == SeriesKind::Complete && packs.is_empty() {
            chosen_kind = SeriesKind::Episodes;
        }
        if chosen_kind == SeriesKind::Episodes && episodes.is_empty() {
            chosen_kind = SeriesKind::Complete;
        }

        // Only offer the split when both kinds are present.
        if !packs.is_empty() && !episodes.is_empty() {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Show")
                        .size(11.0)
                        .color(theme::TEXT_DIM),
                );
                if ui
                    .selectable_label(
                        chosen_kind == SeriesKind::Complete,
                        format!("Complete season · {}", packs.len()),
                    )
                    .clicked()
                {
                    chosen_kind = SeriesKind::Complete;
                }
                if ui
                    .selectable_label(
                        chosen_kind == SeriesKind::Episodes,
                        format!("Episodes · {}", episodes.len()),
                    )
                    .clicked()
                {
                    chosen_kind = SeriesKind::Episodes;
                }
            });
        }

        let selected = match chosen_kind {
            SeriesKind::Complete => packs,
            SeriesKind::Episodes => episodes,
        };

        // The resolutions this selection actually has.
        let mut resolutions: Vec<String> = Vec::new();
        for &position in &selected {
            let label = member_resolution(&work.members[position]);
            if !resolutions.contains(&label) {
                resolutions.push(label);
            }
        }
        resolutions.sort_by_key(|label| std::cmp::Reverse(resolution_rank(label)));

        let mut chosen_resolution = state.resolution.clone();
        if chosen_resolution
            .as_ref()
            .is_some_and(|resolution| !resolutions.contains(resolution))
        {
            chosen_resolution = None;
        }
        if resolutions.len() > 1 {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new("Quality")
                        .size(11.0)
                        .color(theme::TEXT_DIM),
                );
                if ui
                    .selectable_label(chosen_resolution.is_none(), "All")
                    .clicked()
                {
                    chosen_resolution = None;
                }
                for resolution in &resolutions {
                    let selected = chosen_resolution.as_deref() == Some(resolution.as_str());
                    if ui.selectable_label(selected, resolution).clicked() {
                        chosen_resolution = Some(resolution.clone());
                    }
                }
            });
        }

        // Remember the choices for the next frame.
        self.search_series_views.insert(
            key,
            SeriesView {
                season: Some(season),
                kind: chosen_kind,
                resolution: chosen_resolution.clone(),
            },
        );

        // Group the selected releases under a resolution header.
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for &position in &selected {
            let label = member_resolution(&work.members[position]);
            if chosen_resolution
                .as_ref()
                .is_some_and(|chosen| chosen != &label)
            {
                continue;
            }
            match groups.iter_mut().find(|(existing, _)| *existing == label) {
                Some((_, list)) => list.push(position),
                None => groups.push((label, vec![position])),
            }
        }
        groups.sort_by_key(|(label, _)| std::cmp::Reverse(resolution_rank(label)));

        if groups.is_empty() {
            ui.label(
                egui::RichText::new("No releases match this filter.")
                    .size(11.0)
                    .color(theme::TEXT_DIM),
            );
            return;
        }

        for (label, mut positions) in groups {
            ui.label(
                egui::RichText::new(&label)
                    .size(12.0)
                    .strong()
                    .color(theme::TEXT_DIM),
            );
            // `shown` is already in the chosen order, so ascending index is the
            // sorted order.
            positions.sort_by_key(|&position| work.members[position].torrent_id());
            for position in positions {
                let member = &work.members[position];
                let index = member.torrent_id();
                self.search_grouped_row(ui, &shown[index], &member.entry.release, true, actions);
            }
            ui.add_space(4.0);
        }
    }

    /// One torrent inside a category: a compact line, like a row of ext.to's
    /// table, with the quality, swarm and the actions.
    pub(crate) fn search_grouped_row(
        &self,
        ui: &mut egui::Ui,
        hit: &SearchHit,
        release: &Release,
        is_series: bool,
        actions: &mut SearchActions,
    ) {
        egui::Frame::NONE
            .fill(theme::SURFACE_RAISED)
            .inner_margin(egui::Margin::symmetric(10, 6))
            .corner_radius(theme::CARD_RADIUS)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let mut line = Vec::new();
                    if is_series {
                        if let Some(label) = episode_label(release) {
                            line.push(label);
                        }
                    }
                    let mut chips = release.attributes.summary();
                    chips.extend(release.attributes.languages.iter().cloned());
                    if !chips.is_empty() {
                        line.push(chips.join(" \u{00b7} "));
                    }

                    let mut facts = Vec::new();
                    if let Some(size) = hit.size_bytes {
                        facts.push(fmt::human_bytes(size));
                    }
                    if let Some(seeders) = hit.seeders {
                        facts.push(format!("{seeders} seeders"));
                    }
                    if let Some(leechers) = hit.leechers {
                        facts.push(format!("{leechers} leechers"));
                    }
                    if let Some(published) = hit.published_at {
                        facts.push(age_label(published));
                    }
                    facts.push(hit.source.clone());

                    ui.vertical(|ui| {
                        if !line.is_empty() {
                            ui.label(
                                egui::RichText::new(line.join("   \u{2022}   "))
                                    .size(11.5)
                                    .color(theme::TEXT),
                            );
                        }
                        ui.label(
                            egui::RichText::new(facts.join("   \u{2022}   "))
                                .size(10.5)
                                .color(theme::TEXT_DIM),
                        );
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let pending = self.search_actions.get(&hit.key()).copied();
                        let destination = self.cast_device_name();
                        search_actions_ui(ui, hit, pending, actions, destination.as_deref());
                    });
                });
            });
        ui.add_space(4.0);
    }
}

/// The add/copy/open a search row can ask for, applied once at the end of the
/// frame so the borrow of the row is released.
#[derive(Default)]
pub(crate) struct SearchActions {
    add: Option<SearchHit>,
    play: Option<SearchHit>,
    copy: Option<String>,
    open: Option<String>,
}

/// Which kind of release a series section is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SeriesKind {
    #[default]
    Complete,
    Episodes,
}

/// Per-series choices in the search list: which season, which kind, and which
/// resolution is selected. Kept per work so each card remembers its own.
#[derive(Debug, Clone, Default)]
pub(crate) struct SeriesView {
    pub(crate) season: Option<u32>,
    pub(crate) kind: SeriesKind,
    pub(crate) resolution: Option<String>,
}

/// The Play / Add / Open / Copy buttons shared by the flat and grouped rows.
pub(crate) fn search_actions_ui(
    ui: &mut egui::Ui,
    hit: &SearchHit,
    pending: Option<SearchActionPhase>,
    actions: &mut SearchActions,
    destination: Option<&str>,
) {
    let can_add = hit.is_usable() || hit.detail_url.is_some();
    if let Some(phase) = pending {
        search_action_indicator(ui, phase);
    }
    let play_label = if destination.is_some() {
        "Play on TV"
    } else {
        "Play"
    };
    let play_hover = match destination {
        Some(device) => format!("Add this result and play its best file on {device}"),
        None => "Add this result and start streaming its best file".to_string(),
    };
    if ui
        .add_enabled(
            can_add && pending.is_none(),
            egui::Button::new(play_label),
        )
        .on_disabled_hover_text("This result has no magnet or .torrent URL")
        .on_hover_text(play_hover)
        .clicked()
    {
        actions.play = Some(hit.clone());
    }
    if ui
        .add_enabled(can_add && pending.is_none(), egui::Button::new("Add"))
        .on_disabled_hover_text("This result has no magnet or .torrent URL")
        .on_hover_text("Add this result; the magnet is fetched from its page")
        .clicked()
    {
        actions.add = Some(hit.clone());
    }
    if let Some(url) = &hit.detail_url {
        if ui.button("Open").clicked() {
            actions.open = Some(url.clone());
        }
    }
    if let Some(magnet) = &hit.magnet {
        if ui
            .button("Copy")
            .on_hover_text("Copy the magnet link")
            .clicked()
        {
            actions.copy = Some(magnet.clone());
        }
    }
}

pub(crate) fn search_action_indicator(ui: &mut egui::Ui, phase: SearchActionPhase) {
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(
            egui::RichText::new(phase.label())
                .size(10.5)
                .color(theme::ACCENT),
        );
    });
}

/// Draw only the search cards that are actually on screen.
///
/// egui lays out every widget in a `ScrollArea` even when it is scrolled out of
/// view, so a result list of hundreds costs hundreds of layouts — and hundreds
/// of poster decodes — on every frame. This draws the visible cards and reserves
/// the last measured height of the rest; each card is measured the first time it
/// comes into view.
///
/// `heights` is kept by the caller (and survives between frames) so that a card
/// scrolled off screen is still accounted for.
/// A labelled text field, for the expanded filter row.
pub(crate) fn labelled_field(ui: &mut egui::Ui, label: &str, value: &mut String, width: f32) {
    ui.vertical(|ui| {
        ui.label(egui::RichText::new(label).size(10.5).color(theme::TEXT_DIM));
        ui.add_sized([width, 24.0], egui::TextEdit::singleline(value));
    });
}

pub(crate) fn virtual_list(
    ui: &mut egui::Ui,
    len: usize,
    heights: &mut Vec<f32>,
    mut render: impl FnMut(&mut egui::Ui, usize),
) {
    if heights.len() < len {
        heights.resize(len, SEARCH_CARD_HEIGHT);
    }
    egui::ScrollArea::vertical().show_viewport(ui, |ui, viewport| {
        // The viewport is in content coordinates, with zero at the top; the
        // content is laid out from `origin` downwards, so the cursor has to be
        // made relative to it before comparing.
        let origin = ui.max_rect().top();
        for (index, slot) in heights.iter_mut().take(len).enumerate() {
            let height = *slot;
            let top = ui.cursor().top() - origin;
            let bottom = top + height;
            let visible = bottom >= viewport.top() && top <= viewport.bottom();
            if visible {
                let before = ui.cursor().top();
                render(ui, index);
                let measured = ui.cursor().top() - before;
                if measured > 0.0 {
                    *slot = measured;
                }
            } else {
                ui.add_space(height);
            }
            ui.add_space(SEARCH_CARD_GAP);
        }
    });
}

/// How relevant a work is to the query, highest first.
///
/// The title match dominates, so an exact hit still wins. Below that, a title
/// whose metadata was fetched beats one that matched nothing, and a title with
/// many torrents beats a one-release oddity — the two signals that the user most
/// likely meant this one.
pub(crate) fn work_score(work: &Work, query: &str) -> i64 {
    let matched = i64::from(work_relevance(&work.title, query));
    let has_metadata = i64::from(work.metadata.is_some());
    let torrents = (work.torrent_count() as i64).min(50);
    matched * 1000 + has_metadata * 100 + torrents * 4
}

/// How well a work's title matches the search query, so the closest result
/// sorts first: exact (3), contains the whole query (2), contains every query
/// word (1), otherwise 0.
pub(crate) fn work_relevance(title: &str, query: &str) -> u8 {
    let title = normalize_words(title);
    let query = normalize_words(query);
    if query.is_empty() {
        return 0;
    }
    if title == query {
        return 3;
    }
    if title.contains(&query) {
        return 2;
    }
    let words: HashSet<&str> = title.split_whitespace().collect();
    if query.split_whitespace().all(|word| words.contains(word)) {
        return 1;
    }
    0
}

/// Lowercase, punctuation to spaces, single-spaced: `The Lighthouse (2019)`
/// becomes `the lighthouse 2019`.
pub(crate) fn normalize_words(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The numeric resolution a label names, 0 when unrecognised.
pub(crate) fn resolution_rank(label: &str) -> u16 {
    let value = label.to_ascii_lowercase();
    if value.contains("2160") || value.contains("4k") {
        2160
    } else if value.contains("1440") {
        1440
    } else if value.contains("1080") {
        1080
    } else if value.contains("720") {
        720
    } else if value.contains("576") {
        576
    } else if value.contains("480") {
        480
    } else {
        0
    }
}

/// The category a film torrent belongs to, as `(sort key, label)`. Higher
/// resolution sorts first.
pub(crate) fn resolution_category(resolution: Option<&str>) -> (i64, String) {
    let rank = resolution_rank(resolution.unwrap_or(""));
    let name = match rank {
        2160 => "2160p (4K)",
        1440 => "1440p",
        1080 => "1080p",
        720 => "720p",
        576 => "576p",
        480 => "480p",
        _ => "Other",
    };
    (-i64::from(rank), name.to_string())
}

/// The resolution label for a series release, or a placeholder.
pub(crate) fn member_resolution(member: &WorkMember) -> String {
    member
        .entry
        .release
        .attributes
        .resolution
        .clone()
        .unwrap_or_else(|| "Unknown".to_string())
}

/// Whether a release is a whole-season pack rather than a single episode.
pub(crate) fn is_complete_season(member: &WorkMember) -> bool {
    member
        .entry
        .release
        .episodes
        .iter()
        .find(|episode| !episode.extra)
        .and_then(|episode| episode.episode)
        .is_none()
}

/// `Season 1`, or `Specials` for the season-zero bucket.
pub(crate) fn season_label(season: u32) -> String {
    if season == 0 {
        "Specials".to_string()
    } else {
        format!("Season {season}")
    }
}

/// `S01E02`, `Season 1 complete`, or `None` when the name says neither.
pub(crate) fn episode_label(release: &Release) -> Option<String> {
    let episode = release.episodes.iter().find(|episode| !episode.extra)?;
    match (release.season.or(episode.season), episode.episode) {
        (Some(season), Some(number)) => Some(format!("S{season:02}E{number:02}")),
        (Some(season), None) => Some(format!("Season {season} complete")),
        (None, Some(number)) => Some(format!("E{number:02}")),
        (None, None) => None,
    }
}

/// A rating worth showing: providers report 0 for a title nobody has voted on.
pub(crate) fn display_rating(metadata: &Metadata) -> Option<f32> {
    let rating = metadata.rating?;
    (rating > 0.0 && metadata.vote_count.unwrap_or(0) > 0).then_some(rating)
}

/// `6y` / `3mo` / `5d` for a publication time, so a row reads like ext.to's.
pub(crate) fn age_label(published_at: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(published_at);
    let days = (now - published_at).max(0) / 86_400;
    if days >= 365 {
        format!("{}y", days / 365)
    } else if days >= 30 {
        format!("{}mo", days / 30)
    } else if days >= 1 {
        format!("{days}d")
    } else {
        "today".to_string()
    }
}

/// How search results are ordered for display.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SearchSort {
    /// Keep the order the backends returned: best swarm first.
    Relevance,
    Seeders,
    Size,
    Newest,
}

impl SearchSort {
    pub(crate) fn label(self) -> &'static str {
        match self {
            SearchSort::Relevance => "Best match",
            SearchSort::Seeders => "Most seeders",
            SearchSort::Size => "Largest",
            SearchSort::Newest => "Newest",
        }
    }

    /// Order in place. Unknown values sort last, and ties keep backend order
    /// (`sort_by` is stable).
    pub(crate) fn apply(self, hits: &mut [SearchHit]) {
        match self {
            SearchSort::Relevance => {}
            SearchSort::Seeders => {
                hits.sort_by_key(|hit| std::cmp::Reverse(hit.seeders.unwrap_or(0)));
            }
            SearchSort::Size => {
                hits.sort_by_key(|hit| std::cmp::Reverse(hit.size_bytes.unwrap_or(0)));
            }
            SearchSort::Newest => {
                hits.sort_by_key(|hit| std::cmp::Reverse(hit.published_at.unwrap_or(0)));
            }
        }
    }
}

/// One line summarising a search, including sources that failed.
pub(crate) fn describe_search(results: &reel_catalog::SearchResults) -> Option<String> {
    describe_search_summary(results.hits.len(), &results.failures)
}

pub(crate) fn describe_search_summary(hit_count: usize, failures: &[(String, String)]) -> Option<String> {
    let mut parts = Vec::new();
    parts.push(plural(hit_count, "result"));
    for (source, message) in failures {
        parts.push(format!(
            "{source} failed: {}",
            reel_core::title::truncate(message, 80)
        ));
    }
    Some(parts.join("   \u{2022}   "))
}

