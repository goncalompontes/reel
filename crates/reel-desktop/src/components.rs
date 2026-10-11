//! The design system: the type and spacing scales, and the small widgets every
//! screen shares.
//!
//! Views should not invent their own sizes and colours. Keeping headings,
//! captions, label/value rows and empty states here is what makes the app read
//! as one app rather than a set of separately-built pages.

use egui::{Response, RichText, Ui, Vec2};

use crate::theme;

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
    ui.label(RichText::new(text.into()).size(TEXT_DISPLAY).strong());
}

/// A section heading inside a view.
pub fn section_header(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).size(TEXT_TITLE).strong());
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

/// The one primary action of a view.
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Button::new(RichText::new(text).size(TEXT_HEADING)))
}

/// A centred placeholder for a list with nothing in it.
pub fn empty_state(ui: &mut Ui, title: &str, body: &str) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(title).size(20.0).strong());
        ui.add_space(SPACE_S);
        ui.label(RichText::new(body).color(theme::TEXT_DIM).size(TEXT_BODY));
    });
}

/// A coloured pill of text, for a state or a quality.
pub fn badge(ui: &mut Ui, text: &str, color: egui::Color32) {
    ui.label(RichText::new(text).size(TEXT_MICRO).color(color));
}
