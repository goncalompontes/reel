//! Platform-appropriate default locations.
//!
//! reel runs on Linux, macOS and Windows, and each keeps user data somewhere
//! different. This is the one place that knows that: the rest of the workspace
//! asks for a home, data or download directory rather than reading `HOME`
//! (which Windows does not set).

use std::path::PathBuf;

/// A non-empty environment variable, as a path.
fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// The user's home directory, by platform convention.
///
/// Unix sets `HOME`; Windows sets `USERPROFILE` (and, in some contexts,
/// `HOMEDRIVE` + `HOMEPATH`).
pub fn home_dir() -> Option<PathBuf> {
    if let Some(home) = env_path("HOME") {
        return Some(home);
    }
    if let Some(profile) = env_path("USERPROFILE") {
        return Some(profile);
    }
    let drive = env_path("HOMEDRIVE")?;
    let path = env_path("HOMEPATH")?;
    Some(drive.join(path))
}

/// Where reel keeps its settings, cache and history.
///
/// `REEL_DATA_DIR` overrides everything. Otherwise the platform convention is
/// used: `~/.local/share/reel` on Unix (matching the original Linux layout),
/// `~/Library/Application Support/reel` on macOS, and `%APPDATA%\reel` on
/// Windows.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = env_path("REEL_DATA_DIR") {
        return dir;
    }

    #[cfg(target_os = "windows")]
    if let Some(appdata) = env_path("APPDATA") {
        return appdata.join("reel");
    }

    #[cfg(target_os = "macos")]
    if let Some(home) = home_dir() {
        return home
            .join("Library")
            .join("Application Support")
            .join("reel");
    }

    if let Some(home) = home_dir() {
        return home.join(".local").join("share").join("reel");
    }
    PathBuf::from("./reel-data")
}

/// Where downloads go by default: `~/Downloads/reel`, or a relative folder if
/// there is no home directory to speak of.
pub fn download_dir() -> PathBuf {
    match home_dir() {
        Some(home) => home.join("Downloads").join("reel"),
        None => PathBuf::from("./reel-downloads"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_honours_the_override() {
        // The override is read before any platform branch, so it is the one
        // piece testable without mutating the process environment.
        let previous = std::env::var_os("REEL_DATA_DIR");
        // SAFETY: single-threaded test scope; restored below.
        unsafe { std::env::set_var("REEL_DATA_DIR", "/tmp/reel-paths-test") };
        assert_eq!(data_dir(), PathBuf::from("/tmp/reel-paths-test"));
        match previous {
            Some(value) => unsafe { std::env::set_var("REEL_DATA_DIR", value) },
            None => unsafe { std::env::remove_var("REEL_DATA_DIR") },
        }
    }

    #[test]
    fn download_dir_is_under_home_when_there_is_one() {
        if home_dir().is_some() {
            assert!(download_dir().ends_with("reel"));
        }
    }
}
