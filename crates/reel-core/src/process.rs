//! Portable helpers for finding and invoking external programs.
//!
//! reel shells out to a few tools it does not link (ffmpeg, ffprobe, and — by
//! the player — mpv). "Is this runnable" differs by platform: Unix has an
//! executive bit, Windows just has files and an implicit `.exe` suffix.

use std::path::{Path, PathBuf};

/// Find an executable: an explicit path that exists, or a `PATH` lookup.
///
/// A value containing a path separator is treated as a path and never looked up
/// on `PATH`, so a caller can force a specific binary.
pub fn resolve_program(program: &str) -> Option<PathBuf> {
    resolve_program_in(program, std::env::var_os("PATH").as_deref())
}

/// Like [`resolve_program`], but against an explicit `PATH` value. An empty
/// search path still accepts an explicit-path program.
pub fn resolve_program_in(program: &str, search: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path = Path::new(program);

    if path.components().count() > 1 {
        return is_executable(path).then(|| path.to_path_buf());
    }

    let search = search?;
    std::env::split_paths(search)
        .flat_map(|dir| candidates(&dir, program))
        .find(|candidate| is_executable(candidate))
}

/// Candidate file names for `program` inside `dir`.
///
/// On Windows an extension-less name also tries `.exe`, matching how the shell
/// resolves it.
fn candidates(dir: &Path, program: &str) -> Vec<PathBuf> {
    let path = dir.join(program);
    #[cfg(windows)]
    {
        let mut out = vec![path];
        if Path::new(program).extension().is_none() {
            out.push(dir.join(format!("{program}.exe")));
        }
        out
    }
    #[cfg(not(windows))]
    {
        vec![path]
    }
}

/// Whether `path` is a file we can execute.
#[cfg(unix)]
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

/// Whether `path` is a file we can execute.
///
/// Windows has no executive bit; a regular file is considered runnable.
#[cfg(not(unix))]
pub fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_programs_on_path() {
        #[cfg(unix)]
        assert!(resolve_program("sh").is_some());
        assert!(resolve_program("definitely-not-a-real-program-xyz").is_none());
    }

    #[test]
    fn explicit_paths_are_used_only_when_they_exist() {
        assert!(resolve_program("/nonexistent/tool").is_none());
    }

    #[test]
    fn an_empty_search_path_finds_nothing() {
        assert!(resolve_program_in("sh", None).is_none());
        assert!(resolve_program_in("sh", Some(std::ffi::OsStr::new(""))).is_none());
    }
}
