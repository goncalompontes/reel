//! The app's icon vocabulary.
//!
//! One place that maps a concept ("play", "add", "search") to a Phosphor
//! glyph, so views never reach for a raw codepoint or an emoji. The font is
//! installed in [`crate::theme::apply`]; these are just the consts.

pub use egui_phosphor::regular::{
    ARROW_CLOCKWISE, ARROW_LEFT, ARROW_RIGHT, CARET_DOWN, CARET_LEFT, CARET_RIGHT, CHECK, CLOCK,
    COMPASS, COPY, DOTS_THREE, DOWNLOAD, FILM_STRIP, FUNNEL, GEAR, HOUSE, LIST, MAGNIFYING_GLASS,
    MONITOR_PLAY, PAUSE, PLAY, PLUS, SPINNER, SQUARES_FOUR, STAR, TELEVISION, X,
};
