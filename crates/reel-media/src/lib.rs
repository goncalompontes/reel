//! `reel-media` — inspection and conversion for casting.
//!
//! DLNA receivers play a narrow slice of torrent content, so before casting we
//! have to answer "can this TV play this file, and if not, what would it take?"
//!
//! 1. [`Probe::from_json`] reads the [`Probe`] ffprobe produces.
//! 2. [`plan`] turns a probe plus [`PlanOptions`] into a [`MediaPlan`]:
//!    serve as-is, rewrap, re-encode, or give up.
//! 3. [`Tools`] locates ffmpeg/ffprobe (override → bundled → `PATH`) and runs
//!    the conversion; [`convert`] builds the command lines.
//! 4. [`ConversionCache`] keeps converted output keyed by source identity.
//!
//! The decision logic, the command lines and the cache policy are pure and
//! unit-tested; only [`Tools`] actually spawns processes.

pub mod cache;
pub mod convert;
pub mod error;
pub mod plan;
pub mod probe;
pub mod tools;

pub use cache::{CacheKey, ConversionCache};
pub use error::{MediaError, Result};
pub use plan::{MediaPlan, PlanOptions, TranscodeMode, plan};
pub use probe::{Audio, Probe, Video};
pub use tools::Tools;
