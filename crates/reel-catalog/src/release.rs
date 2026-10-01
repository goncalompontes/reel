//! Working out what a torrent actually *is*, from the names inside it.
//!
//! # Why this is a best-effort problem
//!
//! There is no standard for release names. The "scene" rules are a set of
//! conventions that most releases follow most of the time, and everything else
//! is ad-hoc. A parser therefore cannot be *correct*; it can only be useful and
//! honest about when it is guessing.
//!
//! Parsing is delegated to [`hunch`], a pure-Rust descendant of `guessit`, after
//! comparing its output against the shapes this app meets (see the tests below
//! and `docs/ARCHITECTURE.md`). Writing that by hand would mean re-deriving a
//! decade of accumulated edge cases: `S01E02`, `1x02`, `Season 1 Complete`,
//! `S01E01E02`, anime's absolute numbering, daily shows, and the tags that must
//! *not* be mistaken for any of them.
//!
//! What this module adds on top is the torrent-level judgement: a torrent is a
//! *set* of files, so the decision "series or film" belongs to the whole set
//! rather than to any one name, and the files have to be lined up with episode
//! numbers so the interface can say "E03" instead of listing paths.

use hunch::MediaType;
use reel_core::media::is_video_file;
use serde::{Deserialize, Serialize};

/// How much to trust the parse.
///
/// Deliberately not `hunch`'s enum, which is `#[non_exhaustive]` and would
/// otherwise leak into this crate's public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    /// Strong anchors found; the title and numbering are very likely right.
    High,
    /// Plausible, with some ambiguity.
    Medium,
    /// Little or nothing was recognised. Treat the title as a guess.
    Low,
}

impl Trust {
    fn from_hunch(confidence: hunch::Confidence) -> Self {
        match confidence {
            hunch::Confidence::High => Trust::High,
            hunch::Confidence::Medium => Trust::Medium,
            _ => Trust::Low,
        }
    }
}

/// What kind of thing a torrent holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    /// A film, or a single video with no episode numbering.
    Movie,
    /// A series, whether one episode or a whole season.
    Series,
    /// Not enough to say. The interface falls back to a plain file list.
    #[default]
    Unknown,
}

/// Technical attributes, shown as extra information.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseAttributes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edition: Option<String>,
}

impl ReleaseAttributes {
    /// The short list a UI wants to show, e.g. `["1080p", "BluRay", "x264"]`.
    pub fn summary(&self) -> Vec<String> {
        [
            self.resolution.as_deref(),
            self.source.as_deref(),
            self.video_codec.as_deref(),
            self.audio_codec.as_deref(),
            self.edition.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect()
    }

    fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Coarse quality rank, `(resolution, source)`. Higher is better.
    ///
    /// Used to order the versions of one work, so "Play" starts the best copy
    /// the library holds. Codec and audio deliberately do not rank: H.265 is
    /// not better than H.264 for a player, and an unusual codec is a reason to
    /// offer a choice, not to prefer one.
    pub fn quality_rank(&self) -> (u8, u8) {
        (
            resolution_rank(self.resolution.as_deref()),
            source_rank(self.source.as_deref()),
        )
    }

    /// A short human label, e.g. `1080p · Blu-ray · x264`. Falls back to a
    /// placeholder when nothing was recognised.
    pub fn quality_label(&self) -> String {
        let parts = self.summary();
        if parts.is_empty() {
            "quality unknown".to_string()
        } else {
            parts.join(" \u{00b7} ")
        }
    }
}

/// Numeric rank of a resolution string. `2160p` beats `1080p`, and anything
/// unrecognised sits below a known low resolution rather than above it.
fn resolution_rank(resolution: Option<&str>) -> u8 {
    let Some(value) = resolution else {
        return 0;
    };
    let lower = value.to_ascii_lowercase();
    if lower.contains("4320") || lower.contains("8k") {
        return 6;
    }
    if lower.contains("2160") || lower.contains("4k") {
        return 5;
    }
    if lower.contains("1440") {
        return 4;
    }
    if lower.contains("1080") {
        return 3;
    }
    if lower.contains("720") {
        return 2;
    }
    if lower.contains("576") || lower.contains("480") || lower.contains("sd") {
        return 1;
    }
    0
}

/// Numeric rank of a source string. Physical media beats a web rip beats TV
/// capture, which is the order people actually pick when several are offered.
fn source_rank(source: Option<&str>) -> u8 {
    let Some(value) = source else {
        return 0;
    };
    let lower = value.to_ascii_lowercase();
    if lower.contains("remux") {
        return 4;
    }
    if lower.contains("blu") || lower.contains("bdrip") || lower.contains("brrip") {
        return 3;
    }
    if lower.contains("web") {
        return 2;
    }
    if lower.contains("hdtv") || lower.contains("dvd") || lower.contains("dvdrip") {
        return 1;
    }
    0
}

/// A file that looks like an episode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpisodeFile {
    /// Index of this file within the torrent.
    pub file_id: usize,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<u32>,
    /// Air date, `YYYY-MM-DD`, for shows that number by date rather than by
    /// episode. Matched against the provider's episode list afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub air_date: Option<String>,
    pub length: u64,
    /// Bonus material, opening/ending, a preview: worth listing, but not an
    /// episode, so it must not be numbered alongside them.
    #[serde(default)]
    pub extra: bool,
}

/// A file to be considered, as the caller sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInput {
    pub file_id: usize,
    pub path: String,
    pub length: u64,
}

impl FileInput {
    pub fn new(file_id: usize, path: impl Into<String>, length: u64) -> Self {
        Self {
            file_id,
            path: path.into(),
            length,
        }
    }

    /// Just the file name: directory names in a torrent are usually the release
    /// folder, which says more about the release than about this file.
    fn file_name(&self) -> &str {
        self.path
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&self.path)
    }
}

/// What a torrent turned out to be.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Release {
    pub kind: MediaKind,
    /// Title as parsed. Empty when nothing sensible was found.
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u16>,
    /// The season the files belong to, when they agree on one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season: Option<u32>,
    /// Video files, with whatever numbering could be recovered.
    pub episodes: Vec<EpisodeFile>,
    pub attributes: ReleaseAttributes,
    pub trust: Trust,
    /// A season's worth of files with no per-file numbering: a pack.
    #[serde(default)]
    pub is_season_pack: bool,
}

impl Default for Release {
    fn default() -> Self {
        Self {
            kind: MediaKind::Unknown,
            title: String::new(),
            year: None,
            season: None,
            episodes: Vec::new(),
            attributes: ReleaseAttributes::default(),
            trust: Trust::Low,
            is_season_pack: false,
        }
    }
}

impl Release {
    /// Every season the torrent holds, ascending.
    pub fn seasons(&self) -> Vec<u32> {
        let mut seasons: Vec<u32> = self
            .episodes
            .iter()
            .filter(|e| !e.extra)
            .filter_map(|e| e.season)
            .collect();
        seasons.sort_unstable();
        seasons.dedup();
        seasons
    }

    /// Air dates present, ascending. Present means the torrent is of a show
    /// that numbers by date.
    pub fn air_dates(&self) -> Vec<String> {
        let mut dates: Vec<String> = self
            .episodes
            .iter()
            .filter_map(|e| e.air_date.clone())
            .collect();
        dates.sort();
        dates.dedup();
        dates
    }

    /// A show numbered by air date rather than by season and episode.
    pub fn is_dated(&self) -> bool {
        self.kind == MediaKind::Series
            && self.episodes.iter().any(|e| e.air_date.is_some())
            && self.episode_numbers().is_empty()
    }

    /// Episode numbers found, in order, ignoring extras.
    pub fn episode_numbers(&self) -> Vec<u32> {
        let mut numbers: Vec<u32> = self
            .episodes
            .iter()
            .filter(|e| !e.extra)
            .filter_map(|e| e.episode)
            .collect();
        numbers.sort_unstable();
        numbers.dedup();
        numbers
    }

    /// Files that could not be numbered at all.
    pub fn unnumbered(&self) -> Vec<&EpisodeFile> {
        self.episodes
            .iter()
            .filter(|e| e.episode.is_none() && !e.extra)
            .collect()
    }

    pub fn extras(&self) -> Vec<&EpisodeFile> {
        self.episodes.iter().filter(|e| e.extra).collect()
    }

    /// Whether this looks like a series, film, or neither.
    pub fn kind(&self) -> MediaKind {
        self.kind
    }
}

/// Parse a single name, for a display title or a lookup query.
///
/// Returns `(title, year, kind)`; `title` falls back to the input with obvious
/// separators tidied when nothing was recognised.
pub fn parse_one(name: &str) -> (String, Option<u16>, MediaKind) {
    let parsed = hunch::hunch(name);
    let kind = match parsed.media_type() {
        Some(MediaType::Episode) => MediaKind::Series,
        Some(MediaType::Movie) => MediaKind::Movie,
        Some(MediaType::Extra) => MediaKind::Movie,
        _ => MediaKind::Unknown,
    };

    let title = parsed
        .title()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| tidy_fallback(name));

    let year = parsed.year().and_then(|y| u16::try_from(y).ok());

    (title, year, kind)
}

/// Analyse a torrent's files together.
///
/// The whole set matters: `Some.Show.S01E02.mkv` alone says "episode 2 of season
/// 1", but three such files differing only in the episode number say "this is a
/// series" in a way no single name can.
pub fn analyse(files: &[FileInput]) -> Release {
    let videos: Vec<&FileInput> = files
        .iter()
        .filter(|f| is_video_file(f.file_name()))
        .collect();

    if videos.is_empty() {
        return Release {
            kind: MediaKind::Unknown,
            title: String::new(),
            year: None,
            season: None,
            episodes: Vec::new(),
            attributes: ReleaseAttributes::default(),
            trust: Trust::Low,
            is_season_pack: false,
        };
    }

    // Sibling context helps: hunch can use the other names in a pack to
    // recognise numbering that a single name would leave ambiguous.
    let names: Vec<&str> = videos.iter().map(|f| f.path.as_str()).collect();

    let mut parsed: Vec<(usize, hunch::HunchResult)> = Vec::with_capacity(videos.len());
    for (index, file) in videos.iter().enumerate() {
        let result = if names.len() > 1 {
            hunch::hunch_with_context(&file.path, &names)
        } else {
            hunch::hunch(&file.path)
        };
        parsed.push((index, result));
    }

    let episodes: Vec<EpisodeFile> = parsed
        .iter()
        .map(|(index, result)| {
            let file = videos[*index];
            let season = result.season().and_then(|s| u32::try_from(s).ok());
            let episode = result.episode().and_then(|e| u32::try_from(e).ok());
            let is_episode = matches!(result.media_type(), Some(MediaType::Episode));

            // A show numbered by air date has no episode number to find, so the
            // date in the name is the only link to a provider's episode list.
            let air_date = (is_episode && episode.is_none())
                .then(|| extract_air_date(&file.path))
                .flatten();

            EpisodeFile {
                file_id: file.file_id,
                path: file.path.clone(),
                season,
                episode,
                air_date,
                length: file.length,
                extra: matches!(result.media_type(), Some(MediaType::Extra)),
            }
        })
        .collect();

    let kind = decide_kind(&parsed, &episodes, files.len());
    let season = dominant_season(&episodes);
    let title = dominant_title(&parsed).unwrap_or_default();
    let year = parsed
        .iter()
        .filter_map(|(_, r)| r.year())
        .next()
        .and_then(|y| u16::try_from(y).ok());
    let attributes = best_attributes(&parsed);
    let trust = parsed
        .iter()
        .map(|(_, r)| Trust::from_hunch(r.confidence()))
        .max_by_key(|trust| trust_rank(*trust))
        .unwrap_or(Trust::Low);

    let numbered = episodes.iter().filter(|e| e.episode.is_some()).count();
    let _ = &numbered;
    let is_season_pack = kind == MediaKind::Series
        && numbered == 0
        && episodes.iter().filter(|e| !e.extra).count() > 1;

    Release {
        kind,
        title,
        year,
        season,
        episodes,
        attributes,
        trust,
        is_season_pack,
    }
}

fn trust_rank(trust: Trust) -> u8 {
    match trust {
        Trust::High => 2,
        Trust::Medium => 1,
        Trust::Low => 0,
    }
}

/// Series or film?
///
/// Numbered episodes are the strongest signal. Failing that, a single video is a
/// film, and several videos with no numbering are far more likely to be a series
/// (an unnumbered pack) or a multi-part film than a film with several copies.
fn decide_kind(
    parsed: &[(usize, hunch::HunchResult)],
    episodes: &[EpisodeFile],
    total_files: usize,
) -> MediaKind {
    let numbered = episodes
        .iter()
        .filter(|e| e.episode.is_some() && !e.extra)
        .count();
    let episode_shaped = parsed
        .iter()
        .filter(|(_, r)| matches!(r.media_type(), Some(MediaType::Episode)))
        .count();

    // A dated file is a series even without a number: hunch only calls it an
    // episode when it has seen episode-shaped naming.
    let dated = episodes.iter().filter(|e| e.air_date.is_some()).count();
    if numbered == 0 && dated > 0 {
        return MediaKind::Series;
    }

    match (numbered, episode_shaped) {
        // One or more files with an episode number: a series.
        (n, _) if n > 0 => MediaKind::Series,
        // hunch recognised episode structure without a number: `S01` or
        // `Season 1 Complete`, which is a pack.
        (0, n) if n > 0 => MediaKind::Series,
        // Several video files and no numbering: a series by volume. A film
        // release with two videos is nearly always a sample alongside it.
        (0, 0) if episodes.iter().filter(|e| !e.extra).count() > 1 => {
            if looks_like_sample_plus_feature(episodes) {
                MediaKind::Movie
            } else {
                MediaKind::Series
            }
        }
        // A single video file carrying no episode markers is a film.
        (0, 0) => {
            if total_files > 0 {
                MediaKind::Movie
            } else {
                MediaKind::Unknown
            }
        }
        _ => MediaKind::Unknown,
    }
}

/// `Film.mkv` next to `Film.sample.mkv`: one real video and some extras.
fn looks_like_sample_plus_feature(episodes: &[EpisodeFile]) -> bool {
    let real: Vec<&EpisodeFile> = episodes.iter().filter(|e| !e.extra).collect();
    if real.len() != 2 {
        return false;
    }
    let sizes: Vec<u64> = real.iter().map(|e| e.length).collect();
    let (big, small) = (sizes[0].max(sizes[1]), sizes[0].min(sizes[1]));
    // A sample is an order of magnitude smaller than the feature.
    big > 0 && small * 8 < big
}

/// The season the files agree on, if they do.
fn dominant_season(episodes: &[EpisodeFile]) -> Option<u32> {
    let mut counts: Vec<(u32, usize)> = Vec::new();
    for season in episodes.iter().filter_map(|e| e.season) {
        match counts.iter_mut().find(|(s, _)| *s == season) {
            Some((_, count)) => *count += 1,
            None => counts.push((season, 1)),
        }
    }
    counts
        .into_iter()
        .max_by_key(|(season, count)| (*count, std::cmp::Reverse(*season)))
        .map(|(season, _)| season)
}

/// The title the files agree on. Longer titles win ties, because a truncated
/// parse is more common than an invented word.
fn dominant_title(parsed: &[(usize, hunch::HunchResult)]) -> Option<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for (_, result) in parsed {
        let Some(title) = result.title().map(str::trim).filter(|t| !t.is_empty()) else {
            continue;
        };
        match counts.iter_mut().find(|(t, _)| t.eq_ignore_ascii_case(title)) {
            Some((_, count)) => *count += 1,
            None => counts.push((title.to_string(), 1)),
        }
    }
    counts
        .into_iter()
        .max_by_key(|(title, count)| (*count, title.len()))
        .map(|(title, _)| title)
}

/// Attributes from whichever file parsed with the most detail.
fn best_attributes(parsed: &[(usize, hunch::HunchResult)]) -> ReleaseAttributes {
    let mut best = ReleaseAttributes::default();

    for (_, result) in parsed {
        let candidate = ReleaseAttributes {
            resolution: clean(result.screen_size()),
            source: clean(result.source()),
            video_codec: clean(result.video_codec()),
            audio_codec: clean(result.audio_codec()),
            release_group: clean(result.release_group()),
            edition: clean(result.edition()),
        };
        // Prefer the file that told us the most, and otherwise keep the first.
        if best.is_empty() || score(&candidate) > score(&best) {
            best = candidate;
        }
    }
    best
}

fn score(attributes: &ReleaseAttributes) -> usize {
    attributes.summary().len()
}

fn clean(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Pull an air date out of a name, as `YYYY-MM-DD`.
///
/// Only used for files that look like episodes but carry no season or episode
/// number, which is how daily shows are named. Requiring three numeric groups in
/// a row keeps a film's year and a resolution from being read as a date.
pub fn extract_air_date(name: &str) -> Option<String> {
    let chars: Vec<char> = name.chars().collect();
    let bytes = &chars;

    for start in 0..bytes.len() {
        if !bytes[start].is_ascii_digit() {
            continue;
        }
        // Four digits, a separator, then two groups.
        if start + 4 > bytes.len() || !bytes[start..start + 4].iter().all(char::is_ascii_digit) {
            continue;
        }
        let year: u32 = bytes[start..start + 4].iter().collect::<String>().parse().ok()?;
        if !(1900..=2200).contains(&year) {
            continue;
        }
        let mut cursor = start + 4;
        let mut groups = Vec::new();
        for _ in 0..2 {
            if cursor >= bytes.len() || !matches!(bytes[cursor], '.' | '-' | '_' | ' ') {
                break;
            }
            cursor += 1;
            let digits_start = cursor;
            while cursor < bytes.len() && bytes[cursor].is_ascii_digit() && cursor - digits_start < 2
            {
                cursor += 1;
            }
            if cursor == digits_start || bytes.get(cursor).is_some_and(|c| c.is_ascii_digit()) {
                break;
            }
            groups.push(bytes[digits_start..cursor].iter().collect::<String>());
        }

        if groups.len() == 2 {
            let month: u32 = groups[0].parse().ok()?;
            let day: u32 = groups[1].parse().ok()?;
            if (1..=12).contains(&month) && (1..=31).contains(&day) {
                return Some(format!("{year:04}-{month:02}-{day:02}"));
            }
        }
    }
    None
}

/// Last resort for a display title: drop the extension and tidy separators.
fn tidy_fallback(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 4 => stem,
        _ => base,
    };
    stem.replace(['.', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(id: usize, name: &str, length: u64) -> FileInput {
        FileInput::new(id, name, length)
    }

    fn series_release(files: &[FileInput]) -> Release {
        analyse(files)
    }

    #[test]
    fn a_film_is_a_film() {
        let release = series_release(&[video(0, "The.Matrix.1999.1080p.BluRay.x264-GROUP.mkv", 4_100_000_000)]);
        assert_eq!(release.kind, MediaKind::Movie);
        assert_eq!(release.title, "The Matrix");
        assert_eq!(release.year, Some(1999));
        assert!(release.episodes[0].episode.is_none());
        assert_eq!(release.trust, Trust::High);
        // hunch canonicalises tag spellings, which is what we want: the same
        // release is written "BluRay", "Blu-ray" and "BDRip" in the wild.
        assert_eq!(
            release.attributes.summary(),
            ["1080p", "Blu-ray", "H.264"]
        );
    }

    #[test]
    fn a_single_episode_is_a_series() {
        let release = series_release(&[video(0, "Breaking.Bad.S05E14.1080p.BluRay.x264-ROVERS.mkv", 2_000_000_000)]);
        assert_eq!(release.kind, MediaKind::Series);
        assert_eq!(release.title, "Breaking Bad");
        assert_eq!(release.season, Some(5));
        assert_eq!(release.episode_numbers(), [14]);
        assert!(!release.is_season_pack);
    }

    #[test]
    fn a_season_pack_is_numbered_per_file() {
        let files = [
            video(0, "Some.Show.S01E01.1080p.WEB.mkv", 1_500_000_000),
            video(1, "Some.Show.S01E02.1080p.WEB.mkv", 1_500_000_000),
            video(2, "Some.Show.S01E03.1080p.WEB.mkv", 1_500_000_000),
        ];
        let release = series_release(&files);
        assert_eq!(release.kind, MediaKind::Series);
        assert_eq!(release.title, "Some Show");
        assert_eq!(release.season, Some(1));
        assert_eq!(release.episode_numbers(), [1, 2, 3]);
        assert!(!release.is_season_pack);
        // Each episode maps back to the file that holds it.
        let by_number: Vec<(u32, usize)> = release
            .episodes
            .iter()
            .map(|e| (e.episode.unwrap(), e.file_id))
            .collect();
        assert_eq!(by_number, [(1, 0), (2, 1), (3, 2)]);
    }

    #[test]
    fn a_pack_with_no_per_file_numbering_is_still_a_series() {
        // "Season 1 Complete" gives a season but no episode numbers.
        let files = [
            video(0, "Some.Show.Season.1.Complete.720p.mkv", 1_200_000_000),
            video(1, "Some.Show.Season.1.Complete.720p.part2.mkv", 1_200_000_000),
        ];
        let release = series_release(&files);
        assert_eq!(release.kind, MediaKind::Series);
        assert!(release.is_season_pack, "{release:?}");
        assert_eq!(release.unnumbered().len(), 2);
    }

    #[test]
    fn non_video_files_are_ignored_entirely() {
        let files = [
            video(0, "Some.Show.S01E01.1080p.mkv", 1_500_000_000),
            FileInput::new(1, "Some.Show.S01E01.en.srt", 40_000),
            FileInput::new(2, "release.nfo", 2_000),
            FileInput::new(3, "cover.jpg", 100_000),
        ];
        let release = series_release(&files);
        assert_eq!(release.episodes.len(), 1);
        assert_eq!(release.episode_numbers(), [1]);
    }

    #[test]
    fn numbering_survives_alternative_shapes() {
        // 1x02
        let release = series_release(&[video(0, "Some.Show.1x02.HDTV.x264.mp4", 900_000_000)]);
        assert_eq!(release.kind, MediaKind::Series);
        assert_eq!(release.episode_numbers(), [2]);

        // Anime, absolute numbering, no season at all.
        let release = series_release(&[video(0, "[Group] Anime Show - 12 [1080p][HEVC].mkv", 1_100_000_000)]);
        assert_eq!(release.kind, MediaKind::Series);
        assert_eq!(release.title, "Anime Show");
        assert_eq!(release.episode_numbers(), [12]);
        assert_eq!(release.season, None);
    }

    #[test]
    fn sibling_context_recovers_numbering_a_single_name_cannot() {
        // "Disc1" alone parses as a film called "Some Show Disc1". Seeing the
        // three siblings together, the differing digit is enough to number
        // them. This is the reason files are parsed as a set rather than one at
        // a time.
        let files = [
            video(0, "Some.Show.Disc1.1080p.mkv", 3_000_000_000),
            video(1, "Some.Show.Disc2.1080p.mkv", 3_000_000_000),
            video(2, "Some.Show.Disc3.1080p.mkv", 3_000_000_000),
        ];
        let release = series_release(&files);
        assert_eq!(release.kind, MediaKind::Series);
        assert_eq!(release.episode_numbers(), [1, 2, 3]);
        assert!(release.unnumbered().is_empty());
    }

    #[test]
    fn several_videos_with_nothing_to_number_still_read_as_a_series() {
        // Volume is the only signal here, and a film with two videos is far
        // rarer than a series with two.
        let files = [
            video(0, "Some.Show.1080p.mkv", 3_000_000_000),
            video(1, "Some.Show.1080p.other.mkv", 3_000_000_000),
        ];
        let release = series_release(&files);
        assert_eq!(release.kind, MediaKind::Series);
        assert_eq!(release.unnumbered().len(), 2);
    }

    #[test]
    fn a_feature_plus_a_sample_stays_a_film() {
        let files = [
            video(0, "The.Matrix.1999.1080p.BluRay.x264-GROUP.mkv", 4_100_000_000),
            video(1, "The.Matrix.1999.1080p.BluRay.x264-GROUP.sample.mkv", 12_000_000),
        ];
        let release = series_release(&files);
        assert_eq!(
            release.kind,
            MediaKind::Movie,
            "a sample is not an episode: {release:?}"
        );
    }

    #[test]
    fn no_video_files_is_unknown_rather_than_a_guess() {
        let files = [FileInput::new(0, "disc.iso", 7_000_000_000)];
        let release = analyse(&files);
        assert_eq!(release.kind, MediaKind::Unknown);
        assert!(release.title.is_empty());
        assert_eq!(release.trust, Trust::Low);
    }

    #[test]
    fn extras_are_marked_and_kept_out_of_the_episode_numbers() {
        let files = [
            video(0, "Some.Show.S01E01.1080p.mkv", 1_500_000_000),
            video(1, "Some.Show.S01E01.Behind.The.Scenes.mkv", 90_000_000),
        ];
        let release = series_release(&files);
        // Whatever hunch decides about the second file, the numbered episode
        // list must not gain a phantom episode from it.
        assert_eq!(release.episode_numbers(), [1]);
    }

    #[test]
    fn parse_one_handles_a_bare_name() {
        assert_eq!(
            parse_one("The.Matrix.1999.1080p.BluRay.x264-GROUP"),
            ("The Matrix".to_string(), Some(1999), MediaKind::Movie)
        );
        assert_eq!(
            parse_one("Some.Show.S01E02.1080p.WEB"),
            ("Some Show".to_string(), None, MediaKind::Series)
        );
    }

    #[test]
    fn parse_one_never_returns_an_empty_title() {
        let (title, _, _) = parse_one("1080p.mkv");
        assert!(!title.is_empty(), "a display title is always needed");
        let (title, _, _) = parse_one("!!!");
        assert!(!title.is_empty());
    }

    #[test]
    fn the_title_is_the_one_the_files_agree_on() {
        // One odd file must not rename the whole show.
        let files = [
            video(0, "Some.Show.S01E01.1080p.mkv", 1_500_000_000),
            video(1, "Some.Show.S01E02.1080p.mkv", 1_500_000_000),
            video(2, "Some.Show.S01E03.1080p.mkv", 1_500_000_000),
        ];
        assert_eq!(analyse(&files).title, "Some Show");
    }

    #[test]
    fn a_daily_show_is_dated_rather_than_numbered() {
        let files = [
            video(0, "The.Daily.Show.2024.01.15.1080p.WEB.h264.mkv", 900_000_000),
            video(1, "The.Daily.Show.2024.01.16.1080p.WEB.h264.mkv", 900_000_000),
        ];
        let release = series_release(&files);

        assert_eq!(release.kind, MediaKind::Series);
        assert!(release.is_dated(), "{release:?}");
        assert_eq!(release.episode_numbers(), Vec::<u32>::new());
        assert_eq!(release.air_dates(), ["2024-01-15", "2024-01-16"]);
        assert_eq!(release.episodes[0].air_date.as_deref(), Some("2024-01-15"));
    }

    #[test]
    fn air_dates_are_only_read_when_they_are_plausible() {
        assert_eq!(
            extract_air_date("The.Daily.Show.2024.01.15.1080p").as_deref(),
            Some("2024-01-15")
        );
        assert_eq!(
            extract_air_date("Show.2024-01-15.720p").as_deref(),
            Some("2024-01-15")
        );
        // A film's year next to a resolution is not a date.
        assert_eq!(extract_air_date("The.Matrix.1999.1080p.BluRay"), None);
        assert_eq!(extract_air_date("Movie.2024.2160p.WEB"), None);
        // Out of range numbers are rejected rather than formatted.
        assert_eq!(extract_air_date("Show.2024.13.15"), None);
        assert_eq!(extract_air_date("Show.2024.01.45"), None);
        assert_eq!(extract_air_date("Show.99999.01.15"), None);
        assert_eq!(extract_air_date("no date here"), None);
    }

    #[test]
    fn a_film_is_not_dated_even_if_it_has_numbers() {
        let release = series_release(&[video(0, "Movie.2024.1080p.WEB.mkv", 900_000_000)]);
        assert_eq!(release.kind, MediaKind::Movie);
        assert!(!release.is_dated());
        assert!(release.episodes[0].air_date.is_none());
    }

    #[test]
    fn a_multi_season_pack_lists_every_season_it_holds() {
        let files = [
            video(0, "Some.Show.S01E01.1080p.mkv", 1_000_000_000),
            video(1, "Some.Show.S01E02.1080p.mkv", 1_000_000_000),
            video(2, "Some.Show.S02E01.1080p.mkv", 1_000_000_000),
            video(3, "Some.Show.S03E01.1080p.mkv", 1_000_000_000),
        ];
        let release = series_release(&files);
        assert_eq!(release.seasons(), [1, 2, 3]);
        // Each file keeps its own season rather than one standing for all.
        let by_season: Vec<(u32, u32, usize)> = release
            .episodes
            .iter()
            .map(|e| (e.season.unwrap(), e.episode.unwrap(), e.file_id))
            .collect();
        assert_eq!(by_season, [(1, 1, 0), (1, 2, 1), (2, 1, 2), (3, 1, 3)]);
    }

    #[test]
    fn an_empty_input_is_unknown() {
        let release = analyse(&[]);
        assert_eq!(release.kind, MediaKind::Unknown);
        assert!(release.episodes.is_empty());
    }

    #[test]
    fn quality_rank_prefers_resolution_then_source() {
        let uhd = ReleaseAttributes {
            resolution: Some("2160p".into()),
            source: Some("Blu-ray".into()),
            ..Default::default()
        };
        let hd_web = ReleaseAttributes {
            resolution: Some("1080p".into()),
            source: Some("WEB-DL".into()),
            ..Default::default()
        };
        let hd_blu = ReleaseAttributes {
            resolution: Some("1080p".into()),
            source: Some("Blu-ray".into()),
            ..Default::default()
        };
        assert!(uhd.quality_rank() > hd_blu.quality_rank());
        assert!(
            hd_blu.quality_rank() > hd_web.quality_rank(),
            "same resolution, better source wins"
        );
        assert_eq!(ReleaseAttributes::default().quality_rank(), (0, 0));
    }

    #[test]
    fn quality_label_falls_back_when_nothing_is_known() {
        assert_eq!(ReleaseAttributes::default().quality_label(), "quality unknown");
        let attributes = ReleaseAttributes {
            resolution: Some("1080p".into()),
            ..Default::default()
        };
        assert_eq!(attributes.quality_label(), "1080p");
    }
}
