//! Poster/card artwork painting and the per-card hit box.

use super::*;

// ---------------------------------------------------------------- artwork

impl App {
    /// Paint a poster or backdrop, falling back to generated artwork.
    ///
    /// The fallback is deliberate, not a placeholder: without an API key there
    /// are no real posters, and a stable colour per title keeps the grid
    /// readable and obviously intentional.
    pub(crate) fn paint_art(
        ui: &egui::Ui,
        rect: Rect,
        reference: Option<&ArtworkRef>,
        seed: &str,
        kind: ArtworkKind,
        radius: CornerRadius,
    ) {
        if let Some(uri) = reference.and_then(|artwork| artwork.local_uri()) {
            ui.painter()
                .rect_filled(rect, radius, theme::SURFACE_RAISED);
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
            reel_core::title::initials(seed),
            egui::FontId::proportional(rect.height() * 0.22),
            Color32::from_white_alpha(38),
        );
    }

    /// A poster tile: artwork, title, versions and a resume bar.
    /// A poster tile: artwork, a gradient scrim, title/quality, and a resume
    /// bar. Lifts slightly on hover, the way a card does on a streaming app.
    pub(crate) fn card(&mut self, ui: &mut egui::Ui, work: &Work, width: f32, height: f32) -> CardHit {
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
        // The title is painted, so expose the card as one labelled button for
        // AccessKit (and the tests).
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), work.heading())
        });
        let painter = ui.painter().clone();

        // Hover lift: a few pixels out on every side, animated.
        let lift = ui
            .ctx()
            .animate_bool_with_time(response.id, response.hovered(), 0.12);
        let art = rect.expand(3.0 * lift);

        Self::paint_art(
            ui,
            art,
            work.poster(),
            &work.title,
            ArtworkKind::Poster,
            theme::CARD_RADIUS,
        );

        // A gradient scrim so the title reads over bright artwork, faded at the
        // top rather than a hard band.
        let mut scrim = art;
        scrim.set_top(art.bottom() - art.height() * 0.56);
        theme::paint_scrim(
            &painter,
            scrim,
            theme::CARD_RADIUS,
            if response.hovered() { 235 } else { 210 },
        );

        // A quality pill, top-left, when the release names say something useful.
        if let Some(version) = work.versions().first() {
            let quality = version.attributes.quality_label();
            if !quality.is_empty() && quality != "quality unknown" {
                Self::paint_pill(
                    ui,
                    art.left_top() + Vec2::new(8.0, 8.0),
                    &quality,
                    theme::TEXT,
                    Color32::from_black_alpha(150),
                );
            }
        }

        // Title and subtitle painted left-aligned (a `ui.put` Label would
        // centre itself in its box).
        painter.text(
            egui::pos2(art.left() + 10.0, art.bottom() - 30.0),
            egui::Align2::LEFT_BOTTOM,
            reel_core::title::truncate(&work.title, 22),
            egui::FontId::new(13.5, theme::semibold(ui.ctx())),
            Color32::WHITE,
        );

        // Second line: a resume hint, or what the title actually contains.
        let subtitle = if let Some(position) = work.resume_position() {
            format!("{}  resume {}", icons::PLAY, fmt::human_duration(position))
        } else if work.is_series() {
            let episodes = work.episodes();
            let choices = episodes
                .iter()
                .filter(|episode| episode.variants.len() > 1)
                .count();
            if choices > 0 {
                format!(
                    "{} episodes \u{00b7} {choices} with choices",
                    episodes.len()
                )
            } else {
                plural(episodes.len(), "episode")
            }
        } else {
            match work.versions().first() {
                Some(version) => {
                    let label = version.attributes.quality_label();
                    let quality = if label == "quality unknown" {
                        String::new()
                    } else {
                        label
                    };
                    let count = work.versions().len();
                    match (quality.is_empty(), count > 1) {
                        (false, true) => format!("{quality} \u{00b7} {count} versions"),
                        (false, false) => quality,
                        (true, _) => work
                            .year
                            .map(|year| year.to_string())
                            .unwrap_or_else(|| reel_core::title::truncate(&work.key, 10)),
                    }
                }
                None => work
                    .year
                    .map(|year| year.to_string())
                    .unwrap_or_else(|| reel_core::title::truncate(&work.key, 10)),
            }
        };
        painter.text(
            egui::pos2(art.left() + 10.0, art.bottom() - 10.0),
            egui::Align2::LEFT_BOTTOM,
            subtitle,
            egui::FontId::proportional(10.5),
            if work.resume_position().is_some() {
                theme::ACCENT
            } else {
                theme::TEXT_DIM
            },
        );

        // Download progress along the bottom edge, from the best member.
        let lead = self.lead_item(work);
        if let Some(item) = lead.as_ref() {
            if item.torrent.stats.percent < 99.5 {
                let bar = Rect::from_min_size(
                    egui::pos2(art.left(), art.bottom() - 4.0),
                    Vec2::new(art.width(), 4.0),
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
                    egui::pos2(art.right() - 14.0, art.top() + 14.0),
                    8.0,
                    Color32::from_black_alpha(180),
                );
                painter.text(
                    egui::pos2(art.right() - 14.0, art.top() + 14.0),
                    egui::Align2::CENTER_CENTER,
                    work.torrent_count().to_string(),
                    egui::FontId::proportional(11.0),
                    Color32::WHITE,
                );
            }

            let dot_x = if work.torrent_count() > 1 {
                art.right() - 30.0
            } else {
                art.right() - 14.0
            };
            painter.circle_filled(
                egui::pos2(dot_x, art.top() + 14.0),
                5.0,
                theme::state_color(&item.torrent.stats.state),
            );
        }

        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            painter.rect_stroke(
                art,
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

    /// Paint a small rounded pill of text, returning the rect it occupied.
    pub(crate) fn paint_pill(
        ui: &egui::Ui,
        pos: egui::Pos2,
        text: &str,
        fg: Color32,
        bg: Color32,
    ) -> Rect {
        let painter = ui.painter();
        let galley = painter.layout_no_wrap(text.to_owned(), egui::FontId::proportional(10.0), fg);
        let pad = Vec2::new(7.0, 3.0);
        let rect = Rect::from_min_size(pos, galley.size() + pad * 2.0);
        painter.rect_filled(rect, theme::CHIP_RADIUS, bg);
        painter.galley(pos + pad, galley, fg);
        rect
    }
}

pub(crate) struct CardHit {
    pub(crate) clicked: bool,
    pub(crate) double_clicked: bool,
}

