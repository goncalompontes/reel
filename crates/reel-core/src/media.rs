//! Media detection: deciding which files in a torrent are worth playing.
//!
//! Torrents routinely contain samples, `.nfo` files, screenshots and archives
//! next to the actual video. Streaming clients care about exactly one of those
//! files, so the engine needs a deterministic way to guess which one.

/// Video containers we are happy to hand to a player.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mkv", "webm", "avi", "mov", "wmv", "flv", "mpg", "mpeg", "m2ts", "mts", "ts",
    "ogv", "ogm", "divx", "vob", "3gp", "m2v", "mpv",
];

/// Audio containers, used as a fallback when a torrent has no video.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "aac", "m4a", "ogg", "opus", "wav", "wma", "mka", "ape", "alac",
];

/// Subtitle sidecar formats.
pub const SUBTITLE_EXTENSIONS: &[&str] = &["srt", "vtt", "ass", "ssa", "sub", "idx"];

/// Lowercased extension of a path or file name, if any.
pub fn extension_of(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let (stem, ext) = base.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

pub fn is_video_file(name: &str) -> bool {
    extension_of(name).is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.as_str()))
}

pub fn is_audio_file(name: &str) -> bool {
    extension_of(name).is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.as_str()))
}

pub fn is_subtitle_file(name: &str) -> bool {
    extension_of(name).is_some_and(|e| SUBTITLE_EXTENSIONS.contains(&e.as_str()))
}

/// Anything a player is plausibly able to open.
pub fn is_media_file(name: &str) -> bool {
    is_video_file(name) || is_audio_file(name)
}

/// Best-effort MIME type for a file name.
///
/// Importantly this does not sniff the container: it is a hint for `Content-Type`
/// so browsers and players choose the right demuxer.
pub fn mime_for_name(name: &str) -> &'static str {
    match extension_of(name).as_deref() {
        Some("mp4" | "m4v") => "video/mp4",
        Some("mkv") => "video/x-matroska",
        Some("webm") => "video/webm",
        Some("avi") => "video/x-msvideo",
        Some("mov") => "video/quicktime",
        Some("ts" | "m2ts" | "mts") => "video/mp2t",
        Some("flv") => "video/x-flv",
        Some("wmv") => "video/x-ms-wmv",
        Some("mpg" | "mpeg" | "m2v") => "video/mpeg",
        Some("ogv" | "ogm") => "video/ogg",
        Some("3gp") => "video/3gpp",
        Some("vob") => "video/dvd",
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("aac" | "m4a") => "audio/mp4",
        Some("ogg" | "opus") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("wma") => "audio/x-ms-wma",
        Some("mka") => "audio/x-matroska",
        Some("vtt") => "text/vtt",
        Some("srt") => "application/x-subrip",
        Some("ass" | "ssa") => "text/x-ssa",
        _ => "application/octet-stream",
    }
}

/// A regex that matches media files, for `AddTorrentOptions::only_files_regex`.
///
/// Used to tell the engine "only download the video, skip the samples and .nfo
/// files" in a single pass, without first listing the torrent.
pub fn media_only_regex() -> String {
    let alts: Vec<&str> = VIDEO_EXTENSIONS
        .iter()
        .chain(AUDIO_EXTENSIONS.iter())
        .copied()
        .collect();
    format!(r"(?i)\.({})$", alts.join("|"))
}

/// Names that usually mean "not the feature presentation".
pub fn looks_like_extra(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "sample", "trailer", "preview", "screener", "featurette", "behind.the.scenes", "extras",
        "proof", "rarbg",
    ];
    MARKERS.iter().any(|m| lower.contains(m))
}

/// Minimal description of a file, so callers don't have to depend on the
/// torrent library's types.
#[derive(Debug, Clone)]
pub struct FileCandidate {
    pub id: usize,
    pub name: String,
    pub length: u64,
}

/// Pick the file a user most likely wants to watch.
///
/// Rules, in order: prefer non-"sample" videos, then the largest video, then
/// the largest audio file. Returns `None` when nothing looks playable.
pub fn pick_primary_file(files: &[FileCandidate]) -> Option<usize> {
    let videos: Vec<&FileCandidate> = files.iter().filter(|f| is_video_file(&f.name)).collect();
    if !videos.is_empty() {
        let clean: Vec<&&FileCandidate> =
            videos.iter().filter(|f| !looks_like_extra(&f.name)).collect();
        let pool: Vec<&FileCandidate> = if clean.is_empty() {
            videos
        } else {
            clean.into_iter().copied().collect()
        };
        return pool.iter().max_by_key(|f| f.length).map(|f| f.id);
    }

    files
        .iter()
        .filter(|f| is_audio_file(&f.name))
        .max_by_key(|f| f.length)
        .map(|f| f.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fc(id: usize, name: &str, length: u64) -> FileCandidate {
        FileCandidate {
            id,
            name: name.to_string(),
            length,
        }
    }

    #[test]
    fn extensions() {
        assert_eq!(extension_of("a/b/Movie.MKV").as_deref(), Some("mkv"));
        assert_eq!(extension_of("noext"), None);
        assert_eq!(extension_of(".hidden"), None);
    }

    #[test]
    fn picks_largest_video() {
        let files = vec![
            fc(0, "movie.nfo", 10),
            fc(1, "movie-sample.mkv", 5_000),
            fc(2, "movie.mkv", 1_000_000),
            fc(3, "readme.txt", 1),
        ];
        assert_eq!(pick_primary_file(&files), Some(2));
    }

    #[test]
    fn falls_back_to_sample_when_only_option() {
        let files = vec![fc(0, "movie-sample.mkv", 5_000), fc(1, "movie.nfo", 10)];
        assert_eq!(pick_primary_file(&files), Some(0));
    }

    #[test]
    fn falls_back_to_audio() {
        let files = vec![fc(0, "album/track01.flac", 40), fc(1, "album/track02.flac", 90)];
        assert_eq!(pick_primary_file(&files), Some(1));
    }

    #[test]
    fn nothing_playable() {
        let files = vec![fc(0, "disc.iso", 7_000), fc(1, "readme.txt", 1)];
        assert_eq!(pick_primary_file(&files), None);
    }

    #[test]
    fn mime_lookup() {
        assert_eq!(mime_for_name("x.MP4"), "video/mp4");
        assert_eq!(mime_for_name("x.mkv"), "video/x-matroska");
        assert_eq!(mime_for_name("x.unknown"), "application/octet-stream");
    }

    #[test]
    fn regex_matches_media_only() {
        let re = media_only_regex();
        assert!(re.contains("(?i)"));
        assert!(re.contains("mkv"));
    }
}
