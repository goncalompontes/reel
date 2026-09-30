//! Fallback backend: hand the stream URL to a separate player process.
//!
//! This is what you get when libmpv is missing or too old. Playback quality is
//! excellent (it is the real mpv), but the video lives in the player's own
//! window, so reel cannot draw controls over it.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;

use crate::error::PlayerError;

pub(crate) struct ExternalPlayer {
    program: String,
    resolved: PathBuf,
    child: Mutex<Option<Child>>,
}

impl ExternalPlayer {
    pub(crate) fn new(program: String) -> Result<Self, PlayerError> {
        let resolved = resolve_program(&program).ok_or_else(|| {
            PlayerError::EmbeddedUnavailable(format!(
                "program `{program}` was not found on PATH"
            ))
        })?;
        Ok(Self {
            program,
            resolved,
            child: Mutex::new(None),
        })
    }

    pub(crate) fn play(&self, url: &str) -> Result<(), PlayerError> {
        self.kill_running();

        let child = Command::new(&self.resolved)
            .arg("--force-window=yes")
            .arg("--keep-open=yes")
            .arg(url)
            .spawn()
            .map_err(|e| {
                PlayerError::EmbeddedUnavailable(format!(
                    "could not start `{}`: {e}",
                    self.program
                ))
            })?;

        tracing::info!(program = %self.program, pid = child.id(), "handed playback to external player");
        *self.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
        Ok(())
    }

    pub(crate) fn stop(&self) -> Result<(), PlayerError> {
        self.kill_running();
        Ok(())
    }

    pub(crate) fn is_running(&self) -> bool {
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(_)) => {
                    *guard = None;
                    false
                }
                _ => true,
            },
            None => false,
        }
    }

    pub(crate) fn shutdown(&mut self) {
        self.kill_running();
    }

    fn kill_running(&self) {
        let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Minimal `which`: look for an executable file on PATH.
///
/// Deliberately not implemented by spawning `program --version`, because some
/// players open a window for that.
fn resolve_program(program: &str) -> Option<PathBuf> {
    let path = Path::new(program);

    // An explicit path wins.
    if path.components().count() > 1 {
        return path.is_file().then(|| path.to_path_buf());
    }

    let search = std::env::var_os("PATH")?;
    std::env::split_paths(&search)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_programs_on_path() {
        // `sh` exists on every unix; the Windows path is not exercised here.
        #[cfg(unix)]
        assert!(resolve_program("sh").is_some());
        assert!(resolve_program("definitely-not-a-real-program-xyz").is_none());
    }

    #[test]
    fn accepts_absolute_paths_only_when_they_exist() {
        // Has a separator, so it is treated as a path, not a PATH lookup.
        assert!(resolve_program("/nonexistent/player").is_none());
    }
}
