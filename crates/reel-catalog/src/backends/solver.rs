//! Finding and starting the Cloudflare solver that ships with reel.
//!
//! ext.to sits behind Cloudflare's *interactive* challenge, which no plain HTTP
//! client and no headless browser can pass: it needs a real stealth browser.
//! Rather than compile a browser into reel, the installer places
//! [FlareSolverr](https://github.com/FlareSolverr/FlareSolverr) alongside the
//! binary — `<prefix>/lib/reel/solver` — with a `run.sh` that starts it and a
//! `url` file naming the localhost address it listens on.
//!
//! This module locates that install, starts it if nothing is answering yet, and
//! returns its URL. The solver is a small local server, so it is left running
//! (like any other FlareSolverr): the next run reuses it.
//!
//! `REEL_SOLVER` overrides the `run.sh` path, for development.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Points at the solver's `run.sh`. The `url` file is looked up beside it.
const SCRIPT_ENV: &str = "REEL_SOLVER";

/// How long to wait for a freshly started solver to answer.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// The `run.sh` the installer placed next to reel, if it is there.
fn script_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(SCRIPT_ENV) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let exe = std::env::current_exe().ok()?;
    // <prefix>/bin/reel -> <prefix>/lib/reel/solver/run.sh
    let prefix = exe.parent()?.parent()?;
    let script = prefix.join("lib").join("reel").join("solver").join("run.sh");
    script.is_file().then_some(script)
}

/// Whether a bundled solver is installed, so ext.to can be listed as usable.
pub fn bundled_available() -> bool {
    script_path().is_some()
}

/// The URL the bundled solver listens on, from the `url` file beside `run.sh`.
fn bundled_url() -> Option<String> {
    let script = script_path()?;
    let url = std::fs::read_to_string(script.parent()?.join("url")).ok()?;
    let url = url.trim().trim_end_matches('/');
    (!url.is_empty()).then(|| url.to_string())
}

/// Ensure the bundled solver is running, returning its URL.
///
/// Returns `None` when there is no bundled solver, or it could not be started,
/// so the caller can fall back to a cookie or report the challenge.
pub async fn ensure_running() -> Option<String> {
    let url = bundled_url()?;
    if reachable(&url).await {
        return Some(url);
    }

    let script = script_path()?;
    // One start at a time: two searches racing would spawn two solvers, and the
    // second would fail to bind the port.
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = LOCK.lock().await;

    // Another caller may have started it while we waited for the lock.
    if reachable(&url).await {
        return Some(url);
    }
    if !spawn(&script) {
        return None;
    }

    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if reachable(&url).await {
            return Some(url);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    tracing::warn!(%url, "the bundled solver did not start in time");
    None
}

/// A quick TCP check that something is listening at `url`'s host and port.
async fn reachable(url: &str) -> bool {
    let Some(authority) = url.strip_prefix("http://").and_then(|rest| rest.split('/').next())
    else {
        return false;
    };
    matches!(
        tokio::time::timeout(
            Duration::from_millis(300),
            tokio::net::TcpStream::connect(authority),
        )
        .await,
        Ok(Ok(_))
    )
}

/// Start `run.sh` detached, logging beside it. Returns whether it spawned.
#[cfg(unix)]
fn spawn(script: &Path) -> bool {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut command = Command::new("sh");
    command.arg(script).stdin(Stdio::null());
    match script.parent().and_then(|dir| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("solver.log"))
            .ok()
    }) {
        Some(log) => {
            if let Ok(err) = log.try_clone() {
                command.stdout(log).stderr(err);
            } else {
                command.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }
        None => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    // Its own process group, so it is not disturbed by the app's terminal.
    command.process_group(0);
    match command.spawn() {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(%error, "could not start the bundled solver");
            false
        }
    }
}

#[cfg(not(unix))]
fn spawn(_script: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_bundled_solver_is_a_clean_none() {
        // In the test binary there is no <prefix>/lib/reel/solver, and the
        // override is not set, so the backend must simply report "not here".
        if std::env::var_os(SCRIPT_ENV).is_none() {
            assert!(!bundled_available());
            assert_eq!(bundled_url(), None);
        }
    }

    #[tokio::test]
    async fn reachability_rejects_a_dead_port() {
        // Nothing listens on port 1.
        assert!(!reachable("http://127.0.0.1:1").await);
        assert!(!reachable("not a url").await);
    }
}
