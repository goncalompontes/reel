//! `reel-core` — the engine behind the reel streaming client.
//!
//! This crate owns everything that is not UI:
//!
//! * a BitTorrent session (via [`librqbit`]) with sequential, on-demand access
//!   to individual files,
//! * a normalised view of torrents/files ([`model`]) that the HTTP layer, the
//!   desktop GUI and the CLI all share,
//! * media detection helpers ([`media`]) used to pick "the thing the user
//!   actually wants to watch" out of a torrent.
//!
//! It intentionally knows nothing about HTTP or about how playback happens, so
//! that the same engine can be embedded in a Tauri app, a CLI daemon, or a
//! test harness.

pub mod config;
pub mod create;
pub mod engine;
pub mod fmt;
pub mod library;
pub mod media;
pub mod model;
pub mod streaming;
pub mod title;

pub use config::EngineConfig;
pub use create::{CreatedTorrent, create_torrent_file};
pub use engine::{AddOptions, AddSource, BoxedByteStream, Engine, EngineError};
pub use library::{LibraryEntry, LibraryStore, StoredFile};
pub use streaming::{DEFAULT_MEMORY_BUDGET, StreamingStorageFactory};
pub use media::{is_media_file, is_video_file, mime_for_name, pick_primary_file};
pub use model::{
    AddOutcome, AddRequest, FileProbe, FileView, PeerView, StatsView, StreamTarget, TorrentView,
};
