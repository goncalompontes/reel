//! `reel-desktop` — the native app.
//!
//! Everything here is immediate-mode egui rendered by wgpu directly to the
//! window: there is no webview and no browser. Video comes from
//! [`reel_player`] as RGBA frames uploaded into an egui texture, and the
//! torrent engine plus its HTTP streaming API run in-process.

pub mod backend;
pub mod player;
pub mod testing;
pub mod theme;
pub mod ui;

pub use backend::{
    Backend, BackendCapabilities, BackendEvent, EngineBackend, FakeBackend, PlayerCapability,
};
pub use player::{PlaybackInfo, PlaybackStats, PlayerController};
pub use ui::{App, Screen};

/// Apply the app's theme to an egui context.
pub fn apply_theme(ctx: &egui::Context) {
    theme::apply(ctx);
}
