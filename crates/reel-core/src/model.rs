//! Wire-friendly views of the engine's state.
//!
//! These types are the contract between the engine and every consumer (HTTP
//! API, GUI, CLI). They are deliberately engine-agnostic: nothing here leaks
//! `librqbit` types, so the transport can evolve without touching the UI.

use serde::{Deserialize, Serialize};

/// Where a file can be streamed from, relative to the API root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamTarget {
    pub torrent_id: usize,
    pub file_id: usize,
    /// Human readable name, also used as the last path segment.
    pub name: String,
    pub mime: String,
    pub length: u64,
    /// Relative path, e.g. `/stream/1/0/Movie.mkv`.
    pub path: String,
    /// Absolute URL. Filled in by the HTTP layer when it knows its own base URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Minimal per-file description, used on the hot streaming path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileProbe {
    pub id: usize,
    pub name: String,
    pub path: String,
    pub length: u64,
    pub mime: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileView {
    pub id: usize,
    /// Path inside the torrent, `/`-separated.
    pub path: String,
    /// Last path component.
    pub name: String,
    pub length: u64,
    /// Bytes of this file already downloaded. Zero for files that are not
    /// being fetched, which is how a caller can tell one episode is being
    /// streamed and the rest are untouched.
    #[serde(default)]
    pub progress_bytes: u64,
    /// Whether the engine is downloading this file.
    pub included: bool,
    pub is_video: bool,
    pub is_audio: bool,
    pub is_subtitle: bool,
    pub stream: StreamTarget,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct PeerView {
    pub live: u32,
    pub connecting: u32,
    pub queued: u32,
    pub seen: u32,
    pub dead: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatsView {
    /// `initializing` | `live` | `paused` | `error`.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub total_bytes: u64,
    pub progress_bytes: u64,
    pub uploaded_bytes: u64,
    pub finished: bool,
    /// 0.0 – 100.0.
    pub percent: f64,
    pub download_bps: u64,
    pub upload_bps: u64,
    /// Seconds until done, when the engine can estimate it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_seconds: Option<u64>,
    pub peers: PeerView,
}

impl StatsView {
    /// Whether the engine reports this torrent as paused.
    pub fn is_paused(&self) -> bool {
        self.state == "paused"
    }

    /// Whether the engine is actively transferring (or trying to).
    pub fn is_live(&self) -> bool {
        self.state == "live"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentView {
    pub id: usize,
    pub info_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub output_folder: String,
    pub state: String,
    pub finished: bool,
    /// The file a player should open, when one could be determined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_file_id: Option<usize>,
    pub files: Vec<FileView>,
    pub stats: StatsView,
}

impl TorrentView {
    /// The primary file's view, if any.
    pub fn primary_file(&self) -> Option<&FileView> {
        let id = self.primary_file_id?;
        self.files.iter().find(|f| f.id == id)
    }

    /// Fill in absolute stream URLs for every file.
    pub fn with_base_url(&mut self, base_url: &str) {
        let base = base_url.trim_end_matches('/');
        for f in &mut self.files {
            f.stream.url = Some(format!("{base}{}", f.stream.path));
        }
    }
}

/// Percent-encode one path segment, leaving RFC 3986 "unreserved" characters
/// (and `/`-free punctuation that players tolerate) untouched.
///
/// Written by hand to keep the dependency list short: this is the only piece of
/// URL encoding the engine needs.
pub fn percent_encode_path_segment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'+' | b'@' | b',' | b'(' | b')' | b'='
                | b'&' | b'\'' | b'!' | b'$' | b'*' | b';' | b':');
        if keep {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// What the caller asked to add.
///
/// `source` is a `magnet:` URI, a bare 40-char info hash, or an HTTP(S) URL to
/// a `.torrent` file. Raw `.torrent` uploads bypass this and post the bytes
/// directly with `Content-Type: application/x-bittorrent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddRequest {
    pub source: String,
    /// Download only the playable file(s). Defaults to `true`.
    #[serde(default)]
    pub media_only: Option<bool>,
    /// Add without starting the download.
    #[serde(default)]
    pub paused: Option<bool>,
    /// Override the session's output folder for this torrent.
    #[serde(default)]
    pub output_folder: Option<String>,
    /// Let the engine use files that already exist on disk (needed to seed).
    #[serde(default)]
    pub allow_overwrite: Option<bool>,
    /// Peers to connect to immediately, e.g. `["127.0.0.1:51413"]`.
    #[serde(default)]
    pub initial_peers: Option<Vec<String>>,
    /// Hold the torrent until a file is chosen, when it holds more than one.
    #[serde(default)]
    pub pause_multi_file: Option<bool>,
    /// Upload cap in bytes per second.
    #[serde(default)]
    pub upload_limit_bps: Option<u32>,
    /// Download cap in bytes per second.
    #[serde(default)]
    pub download_limit_bps: Option<u32>,
}

/// Result of adding a torrent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddOutcome {
    pub torrent: TorrentView,
    /// `false` when the torrent was already known to the session.
    pub was_new: bool,
    /// `true` when the torrent was held back because it holds more than one
    /// playable file, so the user can choose what to fetch.
    #[serde(default)]
    pub paused_for_selection: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_spaces_and_slashes() {
        assert_eq!(percent_encode_path_segment("Movie.mkv"), "Movie.mkv");
        assert_eq!(
            percent_encode_path_segment("My Movie (2024).mkv"),
            "My%20Movie%20(2024).mkv"
        );
        assert_eq!(percent_encode_path_segment("a/b"), "a%2Fb");
        assert_eq!(percent_encode_path_segment("caf\u{e9}.srt"), "caf%C3%A9.srt");
    }
}
