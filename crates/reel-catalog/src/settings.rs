//! Settings that have to survive a restart.
//!
//! The API key is the reason this exists. An environment variable is fine for a
//! terminal, but a desktop app is normally started by a launcher, and a launcher
//! does not read shell rc files — so `REEL_TMDB_API_KEY` in `.zshrc` is simply
//! absent for anyone clicking an icon. That is the same trap as a launcher entry
//! that depends on `PATH`, and the fix is the same: read it from disk.
//!
//! The environment still wins when it is set, so scripts and CI are unaffected.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const API_KEY_ENV: &str = "REEL_TMDB_API_KEY";
const FILE_NAME: &str = "settings.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogSettings {
    /// TMDB key: a v3 API key or a v4 read access token. Both work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmdb_api_key: Option<String>,
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

        // The key is a credential: create it private rather than writing it
        // world-readable and narrowing afterwards.
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
    fn stored_key(&self) -> Option<String> {
        self.tmdb_api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_string)
    }

    /// The key to actually use: the environment overrides the stored one.
    pub fn api_key(&self) -> Option<String> {
        api_key_from_env().or_else(|| self.stored_key())
    }

    /// Where the key in use came from, for the settings page.
    pub fn key_source(&self) -> KeySource {
        if api_key_from_env().is_some() {
            KeySource::Environment
        } else if self.stored_key().is_some() {
            KeySource::Stored
        } else {
            KeySource::None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    None,
    /// From the settings file.
    Stored,
    /// From an environment variable, which wins.
    Environment,
}

impl KeySource {
    pub fn describe(&self) -> &'static str {
        match self {
            KeySource::None => "not set",
            KeySource::Stored => "set in settings",
            KeySource::Environment => "set in the environment (overrides settings)",
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
        };
        settings.save(&dir).unwrap();

        assert_eq!(CatalogSettings::load(&dir).tmdb_api_key.as_deref(), Some("abc123"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_key_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("mode");
        CatalogSettings {
            tmdb_api_key: Some("secret".into()),
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
    fn the_environment_wins_over_the_stored_key() {
        // SAFETY: single-threaded within this test; the variable is removed again.
        unsafe { std::env::set_var(API_KEY_ENV, "from-env") };
        let stored = CatalogSettings {
            tmdb_api_key: Some("from-disk".into()),
        };
        assert_eq!(stored.api_key().as_deref(), Some("from-env"));
        assert_eq!(stored.key_source(), KeySource::Environment);

        unsafe { std::env::remove_var(API_KEY_ENV) };
        assert_eq!(stored.api_key().as_deref(), Some("from-disk"));
        assert_eq!(stored.key_source(), KeySource::Stored);

        let empty = CatalogSettings::default();
        assert_eq!(empty.api_key(), None);
        assert_eq!(empty.key_source(), KeySource::None);
    }

    #[test]
    fn a_blank_stored_key_counts_as_absent() {
        let settings = CatalogSettings {
            tmdb_api_key: Some("   ".into()),
        };
        unsafe { std::env::remove_var(API_KEY_ENV) };
        assert_eq!(settings.api_key(), None);
        assert_eq!(settings.key_source(), KeySource::None);
    }
}
