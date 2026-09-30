//! `reel-player` — native video playback backed by libmpv.
//!
//! The crate has no opinion about GUI toolkits. It exposes a [`Player`] that
//! decodes video and hands you RGBA frames; a UI uploads those as a texture.
//!
//! ```
//! # fn main() -> Result<(), reel_player::PlayerError> {
//! use reel_player::{Player, PlayerConfig};
//!
//! let player = Player::new(PlayerConfig::default())?;
//! player.set_target_size(Some((1280, 720)));
//! player.load("http://127.0.0.1:3030/stream/0/0/Movie.mkv")?;
//!
//! // Later, once per UI frame:
//! if let Some(snapshot) = player.frame_after(0) {
//!     let frame = &snapshot.frame;
//!     // upload frame.rgba as a `frame.width x frame.height` RGBA texture
//!     let _ = (frame.width, frame.height);
//! }
//! # Ok(())
//! # }
//! ```

mod embedded;
mod error;
mod external;
mod ffi;
mod player;

pub use error::PlayerError;
pub use player::{
    Backend, FrameSnapshot, Player, PlayerConfig, PlayerState, VideoFrame,
};

/// Check whether embedded (libmpv) playback is usable on this machine, and
/// explain why not when it is not.
pub fn embedded_available() -> Result<(), PlayerError> {
    ffi::MpvLib::load().map(|_| ())
}

/// The libmpv client API version reported at runtime, if libmpv loads.
pub fn mpv_api_version() -> Option<(u64, u64)> {
    let lib = ffi::MpvLib::load().ok()?;
    let version = unsafe { (lib.mpv_client_api_version)() };
    Some((version >> 16, version & 0xffff))
}
