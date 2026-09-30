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
    Backend, BackendCapabilities, BackendEvent, CatalogOptions, CatalogStatus, EngineBackend,
    FakeBackend, LibraryItem, PlayerCapability,
};
pub use player::{PlaybackInfo, PlaybackStats, PlayerController};
pub use ui::{App, RowLayout, Screen};

/// Install the dark theme and the image loaders, once, at startup.
pub fn install(ctx: &egui::Context) {
    theme::apply(ctx);
    // Decodes and caches poster images, including `file://` paths from the
    // catalog cache, off the UI thread.
    egui_extras::install_image_loaders(ctx);
}
