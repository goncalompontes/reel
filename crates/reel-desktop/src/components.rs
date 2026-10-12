//! The design system: the type and spacing scales, and the small widgets every
//! screen shares.
//!
//! Views should not invent their own sizes and colours. Keeping headings,
//! captions, chips, buttons and empty states here is what makes the app read as
//! one app rather than a set of separately-built pages.

use egui::{Color32, Response, RichText, Ui, Vec2};

use crate::{icons, theme};

// --- type scale ---------------------------------------------------------

/// A page title: "Activity", "Search", a work's name.
pub const TEXT_DISPLAY: f32 = 22.0;
/// A section heading inside a view.
pub const TEXT_TITLE: f32 = 16.0;
/// A card or button heading.
pub const TEXT_HEADING: f32 = 15.0;
/// Primary body text.
pub const TEXT_BODY: f32 = 12.5;
/// Secondary body text: facts and metadata lines.
pub const TEXT_FACT: f32 = 12.0;
/// Small print: captions and hints.
pub const TEXT_CAPTION: f32 = 11.0;
/// The smallest print: chips and row sub-lines.
pub const TEXT_MICRO: f32 = 10.5;

// --- spacing scale ------------------------------------------------------

/// 4px — between a label and its value.
pub const SPACE_XS: f32 = 4.0;
/// 8px — between related controls.
pub const SPACE_S: f32 = 8.0;
/// 12px — between a heading and its body.
pub const SPACE_M: f32 = 12.0;
/// 16px — between sections.
pub const SPACE_L: f32 = 16.0;
/// 24px — around a major break.
pub const SPACE_XL: f32 = 24.0;

// --- typography widgets -------------------------------------------------

/// The title at the top of a view.
pub fn page_title(ui: &mut Ui, text: impl Into<String>) {
    ui.label(
        RichText::new(text.into())
            .size(TEXT_DISPLAY)
            .family(theme::semibold(ui.ctx())),
    );
}

/// A section heading inside a view.
pub fn section_header(ui: &mut Ui, text: impl Into<String>) {
    ui.label(
        RichText::new(text.into())
            .size(TEXT_TITLE)
            .family(theme::semibold(ui.ctx())),
    );
}

/// A heading with a trailing action on the same line, right-aligned.
pub fn section_header_with_action(
    ui: &mut Ui,
    text: impl Into<String>,
    action: &str,
) -> Option<Response> {
    let mut clicked = None;
    ui.horizontal(|ui| {
        section_header(ui, text);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let response = ui.add(
                egui::Button::new(RichText::new(format!("{action}  {}", icons::CARET_RIGHT)))
                    .frame(false)
                    .fill(Color32::TRANSPARENT),
            );
            if response.clicked() {
                clicked = Some(response);
            }
        });
    });
    clicked
}

/// Dim, small explanatory text under a heading.
pub fn caption(ui: &mut Ui, text: impl Into<String>) {
    ui.label(
        RichText::new(text.into())
            .size(TEXT_CAPTION)
            .color(theme::TEXT_DIM),
    );
}

/// A label/value row, e.g. `Provider   tmdb`.
pub fn row(ui: &mut Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            Vec2::new(150.0, 18.0),
            egui::Label::new(RichText::new(label).color(theme::TEXT_DIM)),
        );
        ui.label(RichText::new(value).monospace().size(TEXT_FACT));
    });
    ui.add_space(2.0);
}

/// A small stat: a dim caption above a value.
pub fn stat(ui: &mut Ui, label: &str, value: &str) {
    ui.vertical(|ui| {
        ui.label(RichText::new(label).size(10.0).color(theme::TEXT_DIM));
        ui.label(RichText::new(value).size(13.0));
    });
    ui.add_space(SPACE_S);
}

// --- buttons ------------------------------------------------------------

/// The one primary action of a view: an accent-filled button.
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).size(TEXT_HEADING))
            .fill(theme::ACCENT_DEEP)
            .corner_radius(theme::CONTROL_RADIUS),
    )
}

/// A secondary action: outlined rather than filled.
pub fn ghost_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(text).size(TEXT_FACT))
            .fill(Color32::TRANSPARENT)
            .stroke(egui::Stroke::new(1.0, theme::BORDER))
            .corner_radius(theme::CONTROL_RADIUS),
    )
}

/// A small icon-only button for row/toolbar actions.
pub fn icon_button(ui: &mut Ui, glyph: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(glyph).size(14.0))
            .frame(false)
            .fill(Color32::TRANSPARENT),
    )
}

// --- pills --------------------------------------------------------------

/// A rounded pill of text, for a quality, a kind or a rating.
pub fn chip(ui: &mut Ui, text: &str, color: Color32) -> Response {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.18))
        .corner_radius(theme::CHIP_RADIUS)
        .inner_margin(egui::Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text)
                    .size(TEXT_MICRO)
                    .family(theme::semibold(ui.ctx()))
                    .color(color),
            );
        })
        .response
}

/// A neutral state/quality chip in the dim text colour.
pub fn neutral_chip(ui: &mut Ui, text: &str) -> Response {
    chip(ui, text, theme::TEXT_DIM)
}

/// A pill of text in a translucent background of the same hue.
pub fn badge(ui: &mut Ui, text: &str, color: Color32) {
    chip(ui, text, color);
}

// --- misc ---------------------------------------------------------------

/// A centred placeholder for a list with nothing in it.
pub fn empty_state(ui: &mut Ui, title: &str, body: &str) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(title)
                .size(20.0)
                .family(theme::semibold(ui.ctx())),
        );
        ui.add_space(SPACE_S);
        ui.label(RichText::new(body).color(theme::TEXT_DIM).size(TEXT_BODY));
    });
}

/// A shimmering placeholder block, for content that has not arrived yet.
pub fn skeleton(ui: &mut Ui, size: Vec2, radius: egui::CornerRadius) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let time = ui.input(|input| input.time) as f32;
    let pulse = 0.5 + 0.5 * (time * 2.0).sin();
    let base = theme::SURFACE_RAISED;
    let alpha = (28.0 + 26.0 * pulse) as u8;
    let color =
        Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), alpha.saturating_add(120));
    ui.painter().rect_filled(rect, radius, color);
}
