//! Turning torrent names into something that looks like a catalogue entry.
//!
//! A torrent name is a filename, not a title: `The.Matrix.1999.1080p.BluRay.
//! x264-GROUP` should read as *The Matrix (1999)*. This is a heuristic, and it
//! is deliberately conservative: when in doubt it keeps the original text
//! rather than mangling it.
//!
//! This is presentation only. Real metadata (posters, synopsis, cast) belongs
//! to the catalogue layer, which will come from a metadata provider.

/// Tokens that mark the end of the title in a typical release name.
const RELEASE_TAGS: &[&str] = &[
    "1080p", "720p", "480p", "2160p", "4k", "uhd", "hd", "sd", "fullhd", "fhd",
    "x264", "x265", "h264", "h265", "hevc", "avc", "xvid", "divx", "10bit", "8bit", "hdr",
    "hdr10", "sdr", "dolby", "dv", "webrip", "web", "webdl", "web-dl", "web-dl", "bluray",
    "blu-ray", "brrip", "bdrip", "dvdrip", "hdtv", "remux", "hdrip", "dvdscr", "cam", "ts",
    "aac", "ac3", "dts", "ddp5", "dd5", "dd+", "atmos", "truehd", "flac", "mp3", "5.1", "7.1",
    "proper", "repack", "internal", "extended", "unrated", "remastered", "multi", "dual",
    "subbed", "subs", "dubbed", "yify", "yts", "rarbg", "evo", "ion10", "psa", "galaxyrg",
];

/// A cleaned-up display title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanTitle {
    pub title: String,
    pub year: Option<u16>,
}

impl CleanTitle {
    /// `The Matrix (1999)`.
    pub fn display(&self) -> String {
        match self.year {
            Some(year) => format!("{} ({year})", self.title),
            None => self.title.clone(),
        }
    }
}

/// Best-effort title extraction. Never returns an empty title.
pub fn clean_title(raw: &str) -> CleanTitle {
    // Work on the last path component only.
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let stem = match base.rsplit_once('.') {
        // Only treat a dot as an extension separator when it is not the only dot.
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 4 && !stem.ends_with(' ') => stem,
        _ => base,
    };

    let mut title = stem.replace(['.', '_'], " ");

    // Keep bracketed year hints, drop other bracketed noise (e.g. [1080p]).
    title = strip_brackets(&title);

    let mut year = None;
    let mut kept: Vec<String> = Vec::new();

    for token in title.split_whitespace() {
        let lower = token
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '+' && c != '-')
            .to_ascii_lowercase();

        if is_release_tag(&lower) {
            break;
        }

        // A 4-digit year between 1900 and 2099, on its own or in parens.
        let digits = token.trim_matches(|c: char| !c.is_ascii_digit());
        if digits.len() == 4 {
            if let Ok(value) = digits.parse::<u16>() {
                if (1900..=2099).contains(&value) {
                    if year.is_none() {
                        year = Some(value);
                    }
                    continue;
                }
            }
        }

        // A "-GROUP" suffix attached to the last token.
        if kept.is_empty() && token.starts_with('-') {
            continue;
        }

        kept.push(token.to_string());
    }

    // Strip a trailing "-GROUP" release group from the final token.
    if let Some(last) = kept.last_mut() {
        if let Some((head, tail)) = last.rsplit_once('-') {
            if !head.is_empty()
                && !tail.is_empty()
                && tail.chars().all(|c| c.is_ascii_alphanumeric())
                && tail.chars().any(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                && !looks_like_word(tail)
            {
                *last = head.to_string();
            }
        }
    }

    let joined = kept.join(" ");
    let cleaned = tidy(&joined);

    if cleaned.is_empty() {
        // Nothing survived; fall back to a lightly tidied original.
        let fallback = tidy(&stem.replace(['.', '_'], " "));
        return CleanTitle {
            title: if fallback.is_empty() {
                stem.to_string()
            } else {
                fallback
            },
            year,
        };
    }

    CleanTitle {
        title: cleaned,
        year,
    }
}

fn is_release_tag(lower: &str) -> bool {
    RELEASE_TAGS.contains(&lower)
}

/// Lowercase words that are plausibly part of a title, so `Spider-Man` is not
/// mistaken for a release group.
fn looks_like_word(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "man" | "men" | "day" | "of" | "the" | "and" | "end" | "one" | "two" | "war" | "up"
    )
}

fn strip_brackets(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut depth = 0usize;
    for ch in input.chars() {
        match ch {
            '[' | '{' => depth += 1,
            ']' | '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Collapse whitespace and trim stray separators.
fn tidy(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c == '-' || c == '.' || c.is_whitespace())
        .to_string()
}

/// Uppercase initials, at most three characters.
pub fn initials(title: &str) -> String {
    let mut out = String::new();
    for word in title.split_whitespace() {
        if let Some(ch) = word.chars().find(|c| c.is_alphanumeric()) {
            out.extend(ch.to_uppercase());
            if out.chars().count() == 3 {
                break;
            }
        }
    }
    if out.is_empty() {
        "?".to_string()
    } else {
        out
    }
}

/// Shorten to `max` characters, adding an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typical_release_names() {
        assert_eq!(
            clean_title("The.Matrix.1999.1080p.BluRay.x264-GROUP"),
            CleanTitle { title: "The Matrix".into(), year: Some(1999) }
        );
        assert_eq!(
            clean_title("Big.Buck.Bunny.2008.720p.WEB-DL.AAC2.0.H.264"),
            CleanTitle { title: "Big Buck Bunny".into(), year: Some(2008) }
        );
        assert_eq!(
            clean_title("Sintel (2010) [1080p] [BluRay]"),
            CleanTitle { title: "Sintel".into(), year: Some(2010) }
        );
    }

    #[test]
    fn plain_names_survive() {
        assert_eq!(
            clean_title("test.mp4"),
            CleanTitle { title: "test".into(), year: None }
        );
        assert_eq!(
            clean_title("Holiday Video.mkv"),
            CleanTitle { title: "Holiday Video".into(), year: None }
        );
    }

    #[test]
    fn hyphens_in_titles_are_kept() {
        // "Man" is a word, so "-Man" must not be stripped as a release group.
        assert_eq!(clean_title("Spider-Man").title, "Spider-Man");
        assert_eq!(
            clean_title("Spider-Man.No.Way.Home.2021.2160p").title,
            "Spider-Man No Way Home"
        );
    }

    #[test]
    fn season_episodes_are_not_mistaken_for_tags() {
        assert_eq!(clean_title("Some.Show.S01E02.1080p").title, "Some Show S01E02");
    }

    #[test]
    fn never_empty() {
        assert_eq!(clean_title("1080p.mkv").title, "1080p");
        assert_eq!(clean_title(".hidden").title, "hidden");
    }

    #[test]
    fn initials_and_truncation() {
        assert_eq!(initials("The Matrix"), "TM");
        assert_eq!(initials("Big Buck Bunny Reloaded"), "BBB");
        assert_eq!(initials(""), "?");
        assert_eq!(truncate("abcdef", 4), "abc\u{2026}");
        assert_eq!(truncate("abc", 4), "abc");
    }
}
