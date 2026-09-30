//! The `reel` HTTP API and streaming server.
//!
//! Two surfaces:
//!
//! * `/api/*` — a JSON control plane (add/list/pause/inspect torrents) that the
//!   desktop GUI, the CLI, or a browser will talk to.
//! * `/stream/*` — byte-serving endpoints that honour HTTP `Range` requests so
//!   players can seek before the file is fully downloaded.
//!
//! The crate is transport only: all state lives in [`reel_core::Engine`], which
//! means a future Tauri app can mount this router in-process.

pub mod range;

mod error;
mod handlers;
mod router;

pub use error::HttpError;
pub use router::{
    AppState, bind, build_router, build_router_with_allowed_origins, serve, serve_router_with_shutdown,
    serve_with_shutdown, shutdown_signal,
};

/// Version string reported by `/api/health`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
