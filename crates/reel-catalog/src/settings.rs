//! Settings that have to survive a restart.
//!
//! This file is the **canonical** way to configure the app. Environment
//! variables are a terminal convenience, but a desktop app is normally started
//! by a launcher, and a launcher does not read shell rc files — so
//! `REEL_TMDB_API_KEY` in `.zshrc` is simply absent for anyone clicking an
//! icon. That is the same trap as a launcher entry that depends on `PATH`, and
//! the fix is the same: read it from disk.
//!
//! The stored value therefore wins. The environment is only consulted as a
//! first-run fallback when nothing has been saved yet, so scripts keep working
//! but saving in the app always takes effect.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const API_KEY_ENV: &str = "REEL_TMDB_API_KEY";
const FILE_NAME: &str = "settings.json";

/// Everything the app remembers between runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogSettings {
    /// TMDB key: a v3 API key or a v4 read access token. Both work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmdb_api_key: Option<String>,

    /// Where torrent data is written. `None` means the platform default
    /// (`~/Downloads/reel`). Takes effect on the next start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_dir: Option<String>,

    /// Fetch files only while they are being watched, rather than downloading
    /// everything in the background. Streaming is the primary way to use the
    /// app, so this is on by default.
    #[serde(default = "default_true")]
    pub stream_only: bool,

    /// Turn subtitles on automatically when a file has them.
    #[serde(default = "default_true")]
    pub subtitles_enabled: bool,

    /// Register the bundled search sources (the Internet Archive).
    #[serde(default = "default_true")]
    pub enable_bundled_sources: bool,

    /// Merge torrents that are the same film or show into one library title.
    #[serde(default = "default_true")]
    pub merge_works: bool,

    /// Preferred subtitle language, e.g. `en`. `None` lets mpv choose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle_language: Option<String>,

    /// Initial player volume, 0–100.
    #[serde(default = "default_volume")]
    pub default_volume: f32,

    /// While streaming, stop fetching once this much is buffered ahead of
    /// playback, and resume when it drops. `0` disables the limit. librqbit
    /// only prioritises a window; it will otherwise fetch the whole file.
    #[serde(default = "default_stream_buffer_mb")]
    pub stream_buffer_mb: u32,
}

fn default_true() -> bool {
    true
}

fn default_volume() -> f32 {
    100.0
}

fn default_stream_buffer_mb() -> u32 {
    256
}

impl Default for CatalogSettings {
    fn default() -> Self {
        Self {
            tmdb_api_key: None,
            download_dir: None,
            stream_only: default_true(),
            subtitles_enabled: default_true(),
            enable_bundled_sources: default_true(),
            merge_works: default_true(),
            subtitle_language: None,
            default_volume: default_volume(),
            stream_buffer_mb: default_stream_buffer_mb(),
        }
    }
}

impl CatalogSettings {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }

    /// Read settings, or default ones. An unreadable file is not an error: the
    /// app must still start.
    pub fn load(data_dir: &Path) -> Self {
        let path = Self::path(data_dir);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "ignoring unreadable settings");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, data_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let bytes = serde_json::to_vec_pretty(self)?;

        // The file can hold a credential: create it private rather than writing
        // it world-readable and narrowing afterwards.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)?;
            file.write_all(&bytes)?;
            file.flush()?;
        }
        #[cfg(not(unix))]
        std::fs::write(&path, bytes)?;

        Ok(())
    }

    /// The stored key, with whitespace and blanks treated as absent.
    pub fn stored_key(&self) -> Option<String> {
        self.tmdb_api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_string)
    }

    /// The key to actually use.
    ///
    /// A stored key is canonical and always wins. The environment is only used
    /// when nothing has been saved, so a shell variable can bootstrap a run
    /// without preventing the user from changing the key in Settings later.
    pub fn api_key(&self) -> Option<String> {
        self.stored_key().or_else(api_key_from_env)
    }

    /// Where the key in use came from, for the settings page.
    pub fn key_source(&self) -> KeySource {
        if self.stored_key().is_some() {
            KeySource::Stored
        } else if api_key_from_env().is_some() {
            KeySource::Environment
        } else {
            KeySource::None
        }
    }

    /// The configured download directory, if the user set one.
    pub fn download_dir_path(&self) -> Option<PathBuf> {
        self.download_dir
            .as_deref()
            .map(str::trim)
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
    }

    /// Clamp fields whose range is not enforced by the type system.
    pub fn normalise(&mut self) {
        self.default_volume = self.default_volume.clamp(0.0, 130.0);
        self.subtitle_language = self
            .subtitle_language
            .take()
            .map(|lang| lang.trim().to_string())
            .filter(|lang| !lang.is_empty());
        self.download_dir = self
            .download_dir
            .take()
            .map(|dir| dir.trim().to_string())
            .filter(|dir| !dir.is_empty());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    None,
    /// From the settings file, which is canonical.
    Stored,
    /// From an environment variable, used only when nothing is stored.
    Environment,
}

impl KeySource {
    pub fn describe(&self) -> &'static str {
        match self {
            KeySource::None => "not set",
            KeySource::Stored => "set in settings",
            KeySource::Environment => "set in the environment (fallback; save a key to override)",
        }
    }
}

fn api_key_from_env() -> Option<String> {
    std::env::var(API_KEY_ENV)
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("reel-settings-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn settings_round_trip() {
        let dir = scratch("roundtrip");
        assert!(CatalogSettings::load(&dir).tmdb_api_key.is_none());

        let settings = CatalogSettings {
            tmdb_api_key: Some("abc123".into()),
            download_dir: Some("/tmp/downloads".into()),
            stream_only: false,
            ..Default::default()
        };
        settings.save(&dir).unwrap();

        let loaded = CatalogSettings::load(&dir);
        assert_eq!(loaded.tmdb_api_key.as_deref(), Some("abc123"));
        assert_eq!(loaded.download_dir.as_deref(), Some("/tmp/downloads"));
        assert!(!loaded.stream_only);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn defaults_are_streaming_first() {
        let settings = CatalogSettings::default();
        assert!(settings.stream_only, "streaming is the primary way to use the app");
        assert!(settings.subtitles_enabled);
        assert_eq!(settings.default_volume, 100.0);
    }

    #[cfg(unix)]
    #[test]
    fn the_settings_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("mode");
        CatalogSettings {
            tmdb_api_key: Some("secret".into()),
            ..Default::default()
        }
        .save(&dir)
        .unwrap();

        let mode = std::fs::metadata(CatalogSettings::path(&dir))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "a credential must not be world readable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_settings_file_does_not_stop_startup() {
        let dir = scratch("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(CatalogSettings::path(&dir), b"not json").unwrap();

        assert!(CatalogSettings::load(&dir).tmdb_api_key.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_stored_key_wins_over_the_environment() {
        // SAFETY: single-threaded within this test; the variable is removed again.
        unsafe { std::env::set_var(API_KEY_ENV, "from-env") };

        // Nothing stored: the environment bootstraps a run.
        let empty = CatalogSettings::default();
        assert_eq!(empty.api_key().as_deref(), Some("from-env"));
        assert_eq!(empty.key_source(), KeySource::Environment);

        // Once saved, the stored key is canonical and the environment no longer
        // overrides it. This is the whole point: Settings, not the shell.
        let stored = CatalogSettings {
            tmdb_api_key: Some("from-disk".into()),
            ..Default::default()
        };
        assert_eq!(stored.api_key().as_deref(), Some("from-disk"));
        assert_eq!(stored.key_source(), KeySource::Stored);

        unsafe { std::env::remove_var(API_KEY_ENV) };
        assert_eq!(stored.api_key().as_deref(), Some("from-disk"));
        assert_eq!(empty.api_key(), None);
        assert_eq!(empty.key_source(), KeySource::None);
    }

    #[test]
    fn a_blank_stored_key_counts_as_absent() {
        let settings = CatalogSettings {
            tmdb_api_key: Some("   ".into()),
            ..Default::default()
        };
        unsafe { std::env::remove_var(API_KEY_ENV) };
        assert_eq!(settings.api_key(), None);
        assert_eq!(settings.key_source(), KeySource::None);
    }

    #[test]
    fn normalise_trims_and_clamps() {
        let mut settings = CatalogSettings {
            download_dir: Some("  /tmp/x  ".into()),
            subtitle_language: Some("  en ".into()),
            default_volume: 999.0,
            ..Default::default()
        };
        settings.normalise();
        assert_eq!(settings.download_dir.as_deref(), Some("/tmp/x"));
        assert_eq!(settings.subtitle_language.as_deref(), Some("en"));
        assert_eq!(settings.default_volume, 130.0);
    }
}
