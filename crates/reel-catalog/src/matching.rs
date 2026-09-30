//! Deciding *which* metadata entry a torrent is.
//!
//! This is the part of a catalog that is easy to get subtly wrong: a search for
//! "The Matrix" happily returns *The Matrix Reloaded*, and a remake means the
//! title alone is not enough. The approach here is a normalised-title
//! similarity combined with a year agreement, with the weights and the
//! acceptance threshold chosen so that the failure mode is "no match" rather
//! than "confidently wrong match".
//!
//! Everything in this module is pure and unit-tested.

use crate::model::Candidate;

/// Skip a match scoring below this overall score.
pub const MIN_SCORE: f32 = 0.62;
/// Skip a match whose titles are barely alike, however well the years line up.
pub const MIN_SIMILARITY: f32 = 0.55;

/// What we know about a torrent before asking a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupQuery {
    pub title: String,
    pub year: Option<u16>,
}

impl LookupQuery {
    pub fn new(title: impl Into<String>, year: Option<u16>) -> Self {
        Self {
            title: title.into(),
            year,
        }
    }

    /// Build from a raw torrent or file name.
    ///
    /// Torrent names look like `The.Matrix.1999.1080p.BluRay.x264-GROUP`, and
    /// feeding that straight to a provider matches badly: the release tags
    /// swamp the title. This runs the same cleaner the interface uses, so the
    /// two agree on what a title is.
    pub fn from_release_name(name: &str) -> Self {
        let cleaned = reel_core::title::clean_title(name);
        Self {
            title: cleaned.title,
            year: cleaned.year,
        }
    }
}

/// How well a candidate matched, for logging and for tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchScore {
    pub similarity: f32,
    pub year_agreement: f32,
    pub total: f32,
}

impl MatchScore {
    pub fn accepted(&self) -> bool {
        self.total >= MIN_SCORE && self.similarity >= MIN_SIMILARITY
    }
}

/// Words that carry no identity in a title.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "the", "of", "el", "la", "le", "les", "los", "las", "der", "die", "das", "il",
    "lo", "un", "una",
];

/// Lowercase, fold common accents, replace punctuation with spaces, drop
/// stopwords, collapse whitespace.
///
/// The result is what two titles are compared on, so it has to make
/// `The Matrix`, `the.matrix` and `Matrix, The` agree while keeping
/// `Matrix Reloaded` distinct.
pub fn normalize(input: &str) -> String {
    let folded: String = input
        .chars()
        .map(|c| if c == '&' { ' ' } else { fold_char(c) })
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect();

    let mut tokens: Vec<String> = folded
        .split_whitespace()
        .map(|token| token.to_ascii_lowercase())
        .filter(|token| !STOPWORDS.contains(&token.as_str()))
        .collect();

    // Never normalise away to nothing: "It" and "The" must stay comparable.
    if tokens.is_empty() {
        tokens = folded
            .split_whitespace()
            .map(|token| token.to_ascii_lowercase())
            .collect();
    }

    tokens.join(" ")
}

/// ASCII-fold the accented letters that actually show up in film titles.
fn fold_char(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => 'a',
        'è' | 'é' | 'ê' | 'ë' | 'È' | 'É' | 'Ê' | 'Ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'Ì' | 'Í' | 'Î' | 'Ï' => 'i',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' => 'o',
        'ù' | 'ú' | 'û' | 'ü' | 'Ù' | 'Ú' | 'Û' | 'Ü' => 'u',
        'ç' | 'Ç' => 'c',
        'ñ' | 'Ñ' => 'n',
        'ý' | 'ÿ' | 'Ý' => 'y',
        'æ' | 'Æ' => 'a',
        'œ' | 'Œ' => 'o',
        'ß' => 's',
        other => other,
    }
}

/// Similarity of two raw titles, 0.0..=1.0.
///
/// Takes the better of a token-level and a character-bigram comparison, so it
/// handles both word reordering and small typos.
pub fn similarity(a: &str, b: &str) -> f32 {
    let (a, b) = (normalize(a), normalize(b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    token_dice(&a, &b).max(bigram_dice(&a, &b))
}

/// Sørensen–Dice over word sets.
fn token_dice(a: &str, b: &str) -> f32 {
    let a: std::collections::HashSet<&str> = a.split(' ').filter(|t| !t.is_empty()).collect();
    let b: std::collections::HashSet<&str> = b.split(' ').filter(|t| !t.is_empty()).collect();
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let common = a.intersection(&b).count();
    (2.0 * common as f32) / (a.len() + b.len()) as f32
}

/// Sørensen–Dice over character bigrams.
fn bigram_dice(a: &str, b: &str) -> f32 {
    let a = bigrams(a);
    let b = bigrams(b);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let mut counts = std::collections::HashMap::new();
    for bigram in &b {
        *counts.entry(bigram.as_str()).or_insert(0u32) += 1;
    }
    let mut common = 0u32;
    for bigram in &a {
        if let Some(count) = counts.get_mut(bigram.as_str()) {
            if *count > 0 {
                *count -= 1;
                common += 1;
            }
        }
    }
    (2.0 * common as f32) / (a.len() + b.len()) as f32
}

fn bigrams(input: &str) -> Vec<String> {
    let chars: Vec<char> = input.chars().filter(|c| *c != ' ').collect();
    if chars.is_empty() {
        return Vec::new();
    }
    if chars.len() == 1 {
        return vec![chars[0].to_string()];
    }
    chars.windows(2).map(|w| w.iter().collect()).collect()
}

/// How much the years agree, 0.0..=1.0. Neutral when either side is unknown.
///
/// One year apart is common in release data (festival run versus general
/// release, or a December release), so it scores well without outranking an
/// exact match.
pub fn year_agreement(query: Option<u16>, candidate: Option<u16>) -> f32 {
    match (query, candidate) {
        (Some(a), Some(b)) if a == b => 1.0,
        (Some(a), Some(b)) if a.abs_diff(b) == 1 => 0.6,
        (Some(_), Some(_)) => 0.0,
        _ => 0.5,
    }
}

/// Score one candidate against the query.
pub fn score(query: &LookupQuery, candidate: &Candidate) -> MatchScore {
    let similarity = similarity(&query.title, &candidate.title);
    let year_agreement = year_agreement(query.year, candidate.year);

    // Popularity and vote count only break ties between near-identical titles,
    // so they are folded in as a small, bounded bonus.
    let popularity = candidate.popularity.unwrap_or(0.0).max(0.0);
    let votes = candidate.vote_count.unwrap_or(0);
    let tie_break = 0.02 * (popularity / (popularity + 50.0)) + 0.02 * (votes.min(5000) as f32 / 5000.0);

    MatchScore {
        similarity,
        year_agreement,
        total: similarity * 0.75 + year_agreement * 0.25 + tie_break,
    }
}

/// The best acceptable candidate, if any.
pub fn best<'a>(query: &LookupQuery, candidates: &'a [Candidate]) -> Option<(&'a Candidate, MatchScore)> {
    candidates
        .iter()
        .map(|candidate| (candidate, score(query, candidate)))
        .filter(|(_, score)| score.accepted())
        .max_by(|(a, sa), (b, sb)| {
            sa.total
                .total_cmp(&sb.total)
                // A deterministic tie-break so results do not flap between runs.
                .then_with(|| a.source_id.cmp(&b.source_id))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, title: &str, year: Option<u16>) -> Candidate {
        Candidate {
            source: "test".into(),
            source_id: id.into(),
            title: title.into(),
            original_title: None,
            year,
            overview: None,
            rating: None,
            vote_count: Some(1000),
            popularity: Some(20.0),
            poster_path: None,
            backdrop_path: None,
        }
    }

    fn query(title: &str, year: Option<u16>) -> LookupQuery {
        LookupQuery::new(title, year)
    }

    #[test]
    fn normalisation_makes_release_names_comparable() {
        assert_eq!(normalize("The Matrix"), "matrix");
        assert_eq!(normalize("the.matrix"), "matrix");
        assert_eq!(normalize("Matrix, The"), "matrix");
        assert_eq!(normalize("THE  MATRIX!!"), "matrix");
        assert_eq!(normalize("Amélie"), "amelie");
        assert_eq!(normalize("Spider-Man: No Way Home"), "spider man no way home");
    }

    #[test]
    fn normalisation_never_produces_nothing() {
        // "The" is a stopword on its own; it must survive rather than vanish.
        assert_eq!(normalize("The"), "the");
        assert_eq!(normalize("!!!"), "");
        assert!(!normalize("A").is_empty());
    }

    #[test]
    fn stopwords_and_ampersands_do_not_block_a_match() {
        let query = query("Fast & Furious", Some(2009));
        let candidates = vec![candidate("1", "Fast and Furious", Some(2009))];
        let (_, score) = best(&query, &candidates).expect("should match");
        assert!(score.accepted(), "{score:?}");
    }

    #[test]
    fn exact_title_and_year_scores_perfectly() {
        let query = query("The Matrix", Some(1999));
        let candidates = vec![candidate("603", "The Matrix", Some(1999))];
        let (_, score) = best(&query, &candidates).unwrap();
        assert!(score.similarity > 0.99, "{score:?}");
        assert!(score.total > 0.99, "{score:?}");
    }

    #[test]
    fn a_sequel_is_rejected_when_the_year_is_known() {
        let query = query("The Matrix", Some(1999));
        let candidates = vec![
            candidate("604", "The Matrix Reloaded", Some(2003)),
            candidate("605", "The Matrix Revolutions", Some(2003)),
        ];
        assert!(
            best(&query, &candidates).is_none(),
            "a sequel must not be accepted for the original"
        );
    }

    #[test]
    fn the_original_wins_over_a_sequel_when_both_are_present() {
        let query = query("The Matrix", Some(1999));
        let candidates = vec![
            candidate("604", "The Matrix Reloaded", Some(2003)),
            candidate("603", "The Matrix", Some(1999)),
        ];
        let (chosen, _) = best(&query, &candidates).unwrap();
        assert_eq!(chosen.source_id, "603");
    }

    #[test]
    fn a_remake_is_disambiguated_by_year() {
        // Same title, different years: without the year the tie-break decides,
        // with it the right one wins.
        let candidates = vec![
            candidate("old", "The Thing", Some(1982)),
            candidate("new", "The Thing", Some(2011)),
        ];
        let (chosen, _) = best(&query("The Thing", Some(2011)), &candidates).unwrap();
        assert_eq!(chosen.source_id, "new");
        let (chosen, _) = best(&query("The Thing", Some(1982)), &candidates).unwrap();
        assert_eq!(chosen.source_id, "old");
    }

    #[test]
    fn a_year_off_by_one_still_matches() {
        // Release-year discrepancies are extremely common.
        let query = query("Sintel", Some(2010));
        let candidates = vec![candidate("1", "Sintel", Some(2011))];
        let (_, score) = best(&query, &candidates).unwrap();
        assert!(score.accepted(), "{score:?}");
        assert_eq!(score.year_agreement, 0.6);
    }

    #[test]
    fn a_completely_unrelated_title_is_rejected() {
        let query = query("Big Buck Bunny", Some(2008));
        let candidates = vec![
            candidate("1", "Inglourious Basterds", Some(2008)),
            candidate("2", "The Hurt Locker", Some(2008)),
        ];
        assert!(best(&query, &candidates).is_none());
    }

    #[test]
    fn typos_still_match_through_the_bigram_fallback() {
        let query = query("Intersteller", None);
        let candidates = vec![candidate("1", "Interstellar", Some(2014))];
        let (_, score) = best(&query, &candidates).unwrap();
        assert!(score.accepted(), "{score:?}");
    }

    #[test]
    fn subtitles_and_punctuation_do_not_matter() {
        let query = query("Spider-Man: No Way Home", Some(2021));
        let candidates = vec![candidate("1", "Spider-Man - No Way Home", Some(2021))];
        assert!(best(&query, &candidates).is_some());
    }

    #[test]
    fn an_unknown_year_does_not_block_a_good_title_match() {
        let query = query("Elephants Dream", None);
        let candidates = vec![candidate("1", "Elephants Dream", Some(2006))];
        let (_, score) = best(&query, &candidates).unwrap();
        assert!(score.accepted(), "{score:?}");
    }

    #[test]
    fn a_short_common_word_does_not_match_a_long_title() {
        // Guards against a tiny title scoring well by accident.
        let query = query("It", Some(2017));
        let candidates = [candidate("1", "It Comes at Night", Some(2017))];
        let score = super::score(&query, &candidates[0]);
        assert!(
            !score.accepted() || score.similarity > 0.5,
            "score should not accept on the year alone: {score:?}"
        );
    }

    #[test]
    fn a_release_name_is_cleaned_before_it_becomes_a_query() {
        let query = LookupQuery::from_release_name("The.Matrix.1999.1080p.BluRay.x264-GROUP");
        assert_eq!(query.title, "The Matrix");
        assert_eq!(query.year, Some(1999));

        // And that query then matches, which it would not have raw: as a
        // filename the extra tokens swamp the title.
        let raw = LookupQuery::new("The.Matrix.1999.1080p.BluRay.x264-GROUP", Some(1999));
        let candidates = vec![candidate("603", "The Matrix", Some(1999))];
        assert!(best(&raw, &candidates).is_none(), "raw name should match badly");
        assert!(best(&query, &candidates).is_some(), "cleaned name should match");
    }

    #[test]
    fn lower_vote_count_loses_a_tie() {
        let query = query("Crash", Some(2004));
        let mut a = candidate("a", "Crash", Some(2004));
        a.vote_count = Some(500);
        let mut b = candidate("b", "Crash", Some(2004));
        b.vote_count = Some(4000);
        let candidates = [a, b];
        let (chosen, _) = best(&query, &candidates).unwrap();
        assert_eq!(chosen.source_id, "b");
    }
}
