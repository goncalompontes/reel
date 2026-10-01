//! Grouping torrents into *works*: one film, or one series.
//!
//! A library added from several sources holds more than one torrent for the
//! same thing: a 2160p and a 1080p copy of a film; season 1 from one release
//! and season 2 from another; two packs that overlap on a few episodes. The
//! catalog should present that as a single title, with the copies offered as a
//! choice, not as several unrelated cards.
//!
//! # How identity is decided
//!
//! Strongest signal first:
//!
//! 1. Two torrents that both have metadata and share `(source, source_id)` are
//!    the same work. A provider id is exact.
//! 2. Otherwise, a normalised title, a compatible year and the same
//!    [`MediaKind`] have to agree. `kind` is part of the key on purpose, so the
//!    *Fargo* film and the *Fargo* series never merge.
//!
//! A metadata-less torrent is folded into a metadata group only when exactly
//! one group matches and the years do not conflict. That is the same
//! "prefer no merge over a wrong merge" rule the metadata matcher uses: a
//! slightly split library is better than one that claims two different films
//! are the same.
//!
//! Everything here is pure and unit-tested.

use std::collections::HashMap;

use crate::matching::normalize;
use crate::model::{CatalogEntry, EpisodeInfo, Metadata, WatchProgress, FINISHED_FRACTION};
use crate::release::{MediaKind, ReleaseAttributes};

/// One torrent's contribution to a work.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkMember {
    pub entry: CatalogEntry,
    /// The file the engine would play for a film, when it named one. The
    /// catalog cannot work this out on its own: `sample.mkv` and the feature
    /// have the same extension.
    pub primary_file_id: Option<usize>,
}

impl WorkMember {
    pub fn new(entry: CatalogEntry, primary_file_id: Option<usize>) -> Self {
        Self {
            entry,
            primary_file_id,
        }
    }

    pub fn torrent_id(&self) -> usize {
        self.entry.torrent_id
    }

    pub fn attributes(&self) -> &ReleaseAttributes {
        &self.entry.release.attributes
    }

    /// The feature file: the engine's primary, else the largest non-extra
    /// video. `None` when the torrent holds no video.
    pub fn feature_file_id(&self) -> Option<usize> {
        self.primary_file_id.or_else(|| {
            self.entry
                .release
                .episodes
                .iter()
                .filter(|episode| !episode.extra)
                .max_by_key(|episode| episode.length)
                .map(|episode| episode.file_id)
        })
    }

    pub fn feature_size(&self) -> u64 {
        let Some(id) = self.feature_file_id() else {
            return 0;
        };
        self.entry
            .release
            .episodes
            .iter()
            .find(|episode| episode.file_id == id)
            .map(|episode| episode.length)
            .unwrap_or(0)
    }

    /// Ordering key for "best copy", descending resolution then source then
    /// size, with the torrent id as a stable tie-break.
    fn quality(&self) -> (u8, u8, u64) {
        let (resolution, source) = self.attributes().quality_rank();
        (resolution, source, self.feature_size())
    }
}

/// One selectable copy of a film.
#[derive(Debug, Clone, PartialEq)]
pub struct Version {
    pub torrent_id: usize,
    pub file_id: usize,
    pub attributes: ReleaseAttributes,
    pub size: u64,
    /// The release name, so two same-quality copies are still distinguishable.
    pub title: String,
}

/// One copy of an episode.
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeVariant {
    pub torrent_id: usize,
    pub file_id: usize,
    pub attributes: ReleaseAttributes,
    pub size: u64,
}

/// One episode, however many copies of it the library holds.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkEpisode {
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub air_date: Option<String>,
    /// Copies of this episode, best first.
    pub variants: Vec<EpisodeVariant>,
}

impl WorkEpisode {
    /// The copy Play uses when the user has not chosen one.
    pub fn preferred(&self) -> Option<&EpisodeVariant> {
        self.variants.first()
    }

    pub fn code(&self) -> String {
        match (self.season, self.episode) {
            (Some(season), Some(number)) => format!("S{season:02}E{number:02}"),
            (_, Some(number)) => format!("Episode {number}"),
            (Some(season), None) => format!("S{season:02} (unnumbered)"),
            (None, None) => self
                .air_date
                .clone()
                .unwrap_or_else(|| "Unnumbered".to_string()),
        }
    }
}

/// A season's worth of episodes, merged across torrents.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkSeason {
    pub number: Option<u32>,
    pub episodes: Vec<WorkEpisode>,
}

/// Bonus material, listed but never numbered as an episode.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkExtra {
    pub torrent_id: usize,
    pub file_id: usize,
    pub path: String,
    pub size: u64,
}

/// A film or a series, however many torrents make it up.
#[derive(Debug, Clone, PartialEq)]
pub struct Work {
    pub key: String,
    pub kind: MediaKind,
    pub title: String,
    pub year: Option<u16>,
    pub metadata: Option<Metadata>,
    /// Members, best version first.
    pub members: Vec<WorkMember>,
}

impl Work {
    /// One torrent as a work of its own, for when merging is off.
    pub fn single(member: WorkMember) -> Self {
        let identity = Identity::of(&member.entry);
        Self {
            key: identity.key,
            kind: identity.kind,
            title: identity.title,
            year: identity.year,
            metadata: member.entry.metadata.clone(),
            members: vec![member],
        }
    }

    pub fn lead(&self) -> &WorkMember {
        // `members` is never empty: a work is only built from one.
        &self.members[0]
    }

    /// The torrent id the UI keys this work by.
    pub fn lead_torrent_id(&self) -> usize {
        self.lead().torrent_id()
    }

    pub fn torrent_count(&self) -> usize {
        self.members.len()
    }

    pub fn is_series(&self) -> bool {
        self.kind == MediaKind::Series
    }

    /// The most recent watch position any member holds.
    pub fn watch(&self) -> Option<&WatchProgress> {
        self.members
            .iter()
            .filter_map(|member| member.entry.watch.as_ref())
            .max_by_key(|progress| progress.updated_at)
    }

    pub fn resume_position(&self) -> Option<f64> {
        self.watch()
            .filter(|progress| progress.is_resumable())
            .map(|progress| progress.position)
    }

    pub fn is_finished(&self) -> bool {
        self.watch()
            .and_then(|progress| progress.fraction())
            .is_some_and(|fraction| fraction > FINISHED_FRACTION)
    }

    pub fn poster(&self) -> Option<&crate::model::ArtworkRef> {
        self.metadata.as_ref()?.artwork.poster.as_ref()
    }

    pub fn backdrop(&self) -> Option<&crate::model::ArtworkRef> {
        self.metadata.as_ref()?.artwork.backdrop.as_ref()
    }

    pub fn heading(&self) -> String {
        match self.year {
            Some(year) => format!("{} ({year})", self.title),
            None => self.title.clone(),
        }
    }

    /// The copies of a film. Empty for a series.
    pub fn versions(&self) -> Vec<Version> {
        if self.is_series() {
            return Vec::new();
        }
        self.members
            .iter()
            .filter_map(|member| {
                let file_id = member.feature_file_id()?;
                Some(Version {
                    torrent_id: member.torrent_id(),
                    file_id,
                    attributes: member.attributes().clone(),
                    size: member.feature_size(),
                    title: member.entry.heading(),
                })
            })
            .collect()
    }

    /// Every episode the library holds for this series, merged and ordered.
    pub fn episodes(&self) -> Vec<WorkEpisode> {
        let mut order: Vec<EpisodeKey> = Vec::new();
        let mut variants: HashMap<EpisodeKey, Vec<EpisodeVariant>> = HashMap::new();

        for member in &self.members {
            for file in member.entry.release.episodes.iter().filter(|file| !file.extra) {
                let key = EpisodeKey::of(member, file);
                if !variants.contains_key(&key) {
                    order.push(key.clone());
                }
                variants.entry(key).or_default().push(EpisodeVariant {
                    torrent_id: member.torrent_id(),
                    file_id: file.file_id,
                    attributes: member.attributes().clone(),
                    size: file.length,
                });
            }
        }

        let mut episodes: Vec<WorkEpisode> = order
            .into_iter()
            .map(|key| {
                let mut copies = variants.remove(&key).unwrap_or_default();
                copies.sort_by(compare_variants);
                WorkEpisode {
                    season: key.season,
                    episode: key.episode,
                    air_date: key.air_date,
                    variants: copies,
                }
            })
            .collect();

        // A dated show has no season or episode number in the file name; the
        // provider's episode list is the only place one can come from.
        for episode in &mut episodes {
            if episode.season.is_none() && episode.episode.is_none() {
                if let Some(info) = self.info_by_date(episode.air_date.as_deref()) {
                    episode.season = Some(info.season);
                    episode.episode = Some(info.number);
                }
            }
        }

        episodes.sort_by(compare_episodes);
        episodes
    }

    /// Episodes grouped by season, for a per-season heading and download.
    pub fn seasons(&self) -> Vec<WorkSeason> {
        let mut seasons: Vec<WorkSeason> = Vec::new();
        for episode in self.episodes() {
            match seasons
                .iter_mut()
                .find(|season| season.number == episode.season)
            {
                Some(season) => season.episodes.push(episode),
                None => seasons.push(WorkSeason {
                    number: episode.season,
                    episodes: vec![episode],
                }),
            }
        }
        seasons
    }

    /// The provider's description of one episode, when it has one.
    pub fn episode_info(&self, episode: &WorkEpisode) -> Option<&EpisodeInfo> {
        let metadata = self.metadata.as_ref()?;
        if let (Some(season), Some(number)) = (episode.season, episode.episode) {
            if let Some(info) = metadata.episode(season, number) {
                return Some(info);
            }
        }
        self.info_by_date(episode.air_date.as_deref())
    }

    fn info_by_date(&self, date: Option<&str>) -> Option<&EpisodeInfo> {
        let date = date?;
        self.metadata
            .as_ref()?
            .episodes
            .iter()
            .find(|info| info.air_date.as_deref() == Some(date))
    }

    /// Bonus material from every member.
    pub fn extras(&self) -> Vec<WorkExtra> {
        let mut extras = Vec::new();
        for member in &self.members {
            for file in member.entry.release.extras() {
                extras.push(WorkExtra {
                    torrent_id: member.torrent_id(),
                    file_id: file.file_id,
                    path: file.path.clone(),
                    size: file.length,
                });
            }
        }
        extras
    }
}

/// Build the works a library holds.
///
/// Members are shared between works only if their identities merge; each is
/// used once.
pub fn build_works(members: &[WorkMember]) -> Vec<Work> {
    if members.is_empty() {
        return Vec::new();
    }

    let identities: Vec<Identity> = members.iter().map(|m| Identity::of(&m.entry)).collect();

    // Group by exact key first.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut group_id: Vec<&Identity> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (member, identity) in identities.iter().enumerate() {
        match index.get(identity.key.as_str()) {
            Some(&group) => groups[group].push(member),
            None => {
                index.insert(identity.key.as_str(), groups.len());
                groups.push(vec![member]);
                group_id.push(identity);
            }
        }
    }

    // Fold metadata-less groups into a metadata group when there is exactly
    // one plausible home. Never fold metadata into metadata (the key already
    // did that) and never pick between two candidates.
    let mut by_name: HashMap<(&str, MediaKind), Vec<usize>> = HashMap::new();
    for (group, identity) in group_id.iter().enumerate() {
        if identity.source_id.is_some() {
            by_name
                .entry((identity.normalized.as_str(), identity.kind))
                .or_default()
                .push(group);
        }
    }

    let mut folded: Vec<Option<usize>> = vec![None; groups.len()];
    let mut removed = vec![false; groups.len()];
    for (group, identity) in group_id.iter().enumerate() {
        if identity.source_id.is_some() {
            continue;
        }
        let Some(candidates) = by_name.get(&(identity.normalized.as_str(), identity.kind)) else {
            continue;
        };
        let compatible: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&candidate| years_compatible(group_id[candidate].year, identity.year))
            .collect();
        if let [only] = compatible.as_slice() {
            folded[group] = Some(*only);
        }
    }
    for (group, target) in folded.iter().enumerate() {
        if let Some(target) = target {
            if *target != group {
                let moved = std::mem::take(&mut groups[group]);
                groups[*target].extend(moved);
                removed[group] = true;
            }
        }
    }

    let mut works: Vec<Work> = groups
        .into_iter()
        .enumerate()
        .filter(|(group, _)| !removed[*group])
        .map(|(_, indices)| build_work(members, &indices))
        .collect();

    // Newest first by the highest member id, so "Recently added" is stable.
    works.sort_by_key(|work| {
        std::cmp::Reverse(
            work.members
                .iter()
                .map(WorkMember::torrent_id)
                .max()
                .unwrap_or(0),
        )
    });
    works
}

/// One work per torrent, for when merging is turned off.
pub fn separate_works(members: &[WorkMember]) -> Vec<Work> {
    let mut works: Vec<Work> = members.iter().cloned().map(Work::single).collect();
    works.sort_by_key(|work| std::cmp::Reverse(work.lead_torrent_id()));
    works
}

fn build_work(members: &[WorkMember], indices: &[usize]) -> Work {
    let mut chosen: Vec<WorkMember> = indices.iter().map(|&i| members[i].clone()).collect();
    // Best copy first: resolution, then source, then size, then id.
    chosen.sort_by(|a, b| b.quality().cmp(&a.quality()).then(a.torrent_id().cmp(&b.torrent_id())));

    let identity = Identity::of(&chosen[0].entry);
    let metadata = chosen
        .iter()
        .find_map(|member| member.entry.metadata.clone());

    Work {
        key: identity.key,
        kind: identity.kind,
        title: metadata
            .as_ref()
            .map(|m| m.title.clone())
            .unwrap_or(identity.title),
        year: metadata
            .as_ref()
            .and_then(|m| m.year)
            .or(identity.year),
        metadata,
        members: chosen,
    }
}

/// What makes two torrents the same work.
struct Identity {
    key: String,
    kind: MediaKind,
    title: String,
    year: Option<u16>,
    normalized: String,
    /// Present when identity came from a provider id.
    source_id: Option<String>,
}

impl Identity {
    fn of(entry: &CatalogEntry) -> Self {
        if let Some(metadata) = entry.metadata.as_ref() {
            let normalized = normalize(&metadata.title);
            return Self {
                key: format!("meta:{}:{}", metadata.source, metadata.source_id),
                kind: effective_kind(metadata.kind, entry),
                title: metadata.title.clone(),
                year: metadata.year.or(entry.year),
                normalized,
                source_id: Some(format!("{}:{}", metadata.source, metadata.source_id)),
            };
        }

        let title = if entry.release.title.trim().is_empty() {
            entry.display_title.clone()
        } else {
            entry.release.title.clone()
        };
        let year = entry.release.year.or(entry.year);
        let kind = entry.release.kind;
        Self {
            key: format!("name:{}|{}|{kind:?}", normalize(&title), year_str(year)),
            kind,
            normalized: normalize(&title),
            title,
            year,
            source_id: None,
        }
    }
}

/// Metadata kind wins, but an unknown one falls back to what the files say.
fn effective_kind(kind: MediaKind, entry: &CatalogEntry) -> MediaKind {
    if kind != MediaKind::Unknown {
        kind
    } else {
        entry.release.kind
    }
}

fn year_str(year: Option<u16>) -> String {
    year.map(|y| y.to_string()).unwrap_or_else(|| "*".to_string())
}

/// Years that could describe the same work. Unknown agrees with anything.
fn years_compatible(a: Option<u16>, b: Option<u16>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.abs_diff(b) <= 1,
        _ => true,
    }
}

/// How an episode is merged across members.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EpisodeKey {
    season: Option<u32>,
    episode: Option<u32>,
    air_date: Option<String>,
    /// Files that carry no number at all stay distinct, keyed by their torrent
    /// and file id, rather than all collapsing into one row.
    fallback: Option<(usize, usize)>,
}

impl EpisodeKey {
    fn of(member: &WorkMember, file: &crate::release::EpisodeFile) -> Self {
        if file.episode.is_some() {
            Self {
                season: file.season,
                episode: file.episode,
                air_date: None,
                fallback: None,
            }
        } else if file.air_date.is_some() {
            Self {
                season: None,
                episode: None,
                air_date: file.air_date.clone(),
                fallback: None,
            }
        } else {
            Self {
                season: file.season,
                episode: None,
                air_date: None,
                fallback: Some((member.torrent_id(), file.file_id)),
            }
        }
    }
}

fn variant_quality(variant: &EpisodeVariant) -> (u8, u8, u64) {
    let (resolution, source) = variant.attributes.quality_rank();
    (resolution, source, variant.size)
}

fn compare_variants(a: &EpisodeVariant, b: &EpisodeVariant) -> std::cmp::Ordering {
    variant_quality(b)
        .cmp(&variant_quality(a))
        .then(a.torrent_id.cmp(&b.torrent_id))
}

fn compare_episodes(a: &WorkEpisode, b: &WorkEpisode) -> std::cmp::Ordering {
    a.season
        .unwrap_or(u32::MAX)
        .cmp(&b.season.unwrap_or(u32::MAX))
        .then(
            a.episode
                .unwrap_or(u32::MAX)
                .cmp(&b.episode.unwrap_or(u32::MAX)),
        )
        .then(a.air_date.cmp(&b.air_date))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release::{EpisodeFile, Release};

    fn entry(id: usize, name: &str, hash: &str, release: Release) -> CatalogEntry {
        CatalogEntry {
            torrent_id: id,
            info_hash: hash.to_string(),
            display_title: name.to_string(),
            year: None,
            metadata: None,
            watch: None,
            release,
            watch_by_file: Default::default(),
        }
    }

    fn movie(id: usize, name: &str, attributes: ReleaseAttributes) -> WorkMember {
        let release = Release {
            kind: MediaKind::Movie,
            title: "The Matrix".into(),
            year: Some(1999),
            episodes: vec![EpisodeFile {
                file_id: 0,
                path: "The.Matrix.1999.mkv".into(),
                season: None,
                episode: None,
                air_date: None,
                length: 4_000_000_000,
                extra: false,
            }],
            attributes,
            ..Default::default()
        };
        WorkMember::new(entry(id, name, &format!("hash{id}"), release), Some(0))
    }

    fn hd(source: &str) -> ReleaseAttributes {
        ReleaseAttributes {
            resolution: Some("1080p".into()),
            source: Some(source.into()),
            ..Default::default()
        }
    }

    fn uhd() -> ReleaseAttributes {
        ReleaseAttributes {
            resolution: Some("2160p".into()),
            source: Some("Blu-ray".into()),
            ..Default::default()
        }
    }

    fn series_file(
        member_id: usize,
        file_id: usize,
        season: u32,
        episode: u32,
        length: u64,
    ) -> WorkMember {
        let release = Release {
            kind: MediaKind::Series,
            title: "Some Show".into(),
            year: Some(2024),
            episodes: vec![EpisodeFile {
                file_id,
                path: format!("Some.Show.S{season:02}E{episode:02}.mkv"),
                season: Some(season),
                episode: Some(episode),
                air_date: None,
                length,
                extra: false,
            }],
            attributes: hd("WEB-DL"),
            ..Default::default()
        };
        WorkMember::new(
            entry(
                member_id,
                "Some.Show",
                &format!("show{member_id}"),
                release,
            ),
            Some(file_id),
        )
    }

    #[test]
    fn two_copies_of_a_film_become_one_work_with_two_versions() {
        let members = [movie(1, "Matrix 1080", hd("Blu-ray")), movie(2, "Matrix 4K", uhd())];
        let works = build_works(&members);

        assert_eq!(works.len(), 1);
        let work = &works[0];
        assert_eq!(work.torrent_count(), 2);
        let versions = work.versions();
        assert_eq!(versions.len(), 2);
        // Best first.
        assert_eq!(versions[0].torrent_id, 2, "2160p should lead");
        assert_eq!(versions[1].torrent_id, 1);
        assert_eq!(work.lead_torrent_id(), 2);
    }

    #[test]
    fn a_different_film_is_not_merged() {
        let other = WorkMember::new(
            entry(
                3,
                "Sintel",
                "sintel",
                Release {
                    kind: MediaKind::Movie,
                    title: "Sintel".into(),
                    ..Default::default()
                },
            ),
            None,
        );
        let members = [movie(1, "Matrix", hd("WEB-DL")), other];
        assert_eq!(build_works(&members).len(), 2);
    }

    #[test]
    fn a_film_and_a_series_of_the_same_name_do_not_merge() {
        let film = movie(1, "Fargo film", hd("Blu-ray"));
        let series = series_file(2, 0, 1, 1, 1_000);
        let mut series_release = series.entry.release.clone();
        series_release.title = "Fargo".into();
        let mut film_release = film.entry.release.clone();
        film_release.title = "Fargo".into();
        let members = [
            WorkMember::new(
                entry(1, "Fargo 1996", "f1", film_release),
                Some(0),
            ),
            WorkMember::new(
                entry(2, "Fargo S01", "f2", series_release),
                Some(0),
            ),
        ];
        assert_eq!(build_works(&members).len(), 2, "kind is part of identity");
    }

    #[test]
    fn metadata_ids_win_over_parsed_names() {
        // Two different release names that resolved to the same provider id.
        let mut a = movie(1, "The.Matrix.1999.1080p", hd("Blu-ray"));
        let mut b = movie(2, "Matrix.1999.REMASTERED.2160p", uhd());
        for member in [&mut a, &mut b] {
            member.entry.metadata = Some(Metadata {
                kind: MediaKind::Movie,
                source: "tmdb".into(),
                source_id: "603".into(),
                title: "The Matrix".into(),
                year: Some(1999),
                ..Default::default()
            });
        }
        let works = build_works(&[a, b]);
        assert_eq!(works.len(), 1);
        assert_eq!(works[0].title, "The Matrix");
        assert!(works[0].metadata.is_some());
    }

    #[test]
    fn distinct_metadata_ids_never_merge() {
        let mut a = movie(1, "Crash 2004", hd("DVD"));
        let mut b = movie(2, "Crash 1996", hd("DVD"));
        for (member, id) in [(&mut a, "100"), (&mut b, "200")] {
            member.entry.metadata = Some(Metadata {
                kind: MediaKind::Movie,
                source: "tmdb".into(),
                source_id: id.into(),
                title: "Crash".into(),
                year: None,
                ..Default::default()
            });
        }
        assert_eq!(build_works(&[a, b]).len(), 2);
    }

    #[test]
    fn an_unenriched_torrent_folds_into_the_enriched_one() {
        let enriched = {
            let mut member = movie(1, "The.Matrix.1999.1080p", hd("Blu-ray"));
            member.entry.metadata = Some(Metadata {
                kind: MediaKind::Movie,
                source: "tmdb".into(),
                source_id: "603".into(),
                title: "The Matrix".into(),
                year: Some(1999),
                ..Default::default()
            });
            member
        };
        let plain = movie(2, "The.Matrix.1999.2160p", uhd());
        let works = build_works(&[enriched, plain]);
        assert_eq!(works.len(), 1, "the un-enriched copy should join its film");
        assert_eq!(works[0].torrent_count(), 2);
    }

    #[test]
    fn seasons_from_different_torrents_merge() {
        let members = [
            series_file(1, 0, 1, 1, 1_000),
            series_file(1, 1, 1, 2, 1_000),
            series_file(2, 0, 2, 1, 1_000),
        ];
        let works = build_works(&members);
        assert_eq!(works.len(), 1, "same show, different seasons");
        let work = &works[0];
        let seasons = work.seasons();
        assert_eq!(seasons.len(), 2);
        assert_eq!(seasons[0].number, Some(1));
        assert_eq!(seasons[0].episodes.len(), 2);
        assert_eq!(seasons[1].number, Some(2));
    }

    #[test]
    fn duplicate_episodes_become_one_row_with_two_variants() {
        let standard = series_file(1, 0, 1, 1, 1_000);
        let mut high = series_file(2, 0, 1, 1, 2_000);
        high.entry.release.attributes = uhd();

        let works = build_works(&[standard, high]);
        assert_eq!(works.len(), 1);
        let episodes = works[0].episodes();
        assert_eq!(episodes.len(), 1, "S01E01 from two torrents is one episode");
        assert_eq!(episodes[0].variants.len(), 2);
        // Best copy first.
        assert_eq!(episodes[0].preferred().unwrap().torrent_id, 2);
    }

    #[test]
    fn separate_works_does_not_merge() {
        let members = [movie(1, "A", hd("Blu-ray")), movie(2, "B", uhd())];
        let works = separate_works(&members);
        assert_eq!(works.len(), 2);
        assert_eq!(works[0].lead_torrent_id(), 2, "newest first");
    }

    #[test]
    fn a_work_watch_position_is_the_most_recent_member() {
        use crate::model::WatchProgress;
        let mut a = movie(1, "A", hd("Blu-ray"));
        let mut b = movie(2, "B", uhd());
        a.entry.watch = Some(WatchProgress {
            position: 100.0,
            duration: Some(1000.0),
            updated_at: 10,
            file_name: None,
            title: None,
        });
        b.entry.watch = Some(WatchProgress {
            position: 400.0,
            duration: Some(1000.0),
            updated_at: 99,
            file_name: None,
            title: None,
        });
        let works = build_works(&[a, b]);
        assert_eq!(works[0].watch().unwrap().position, 400.0);
    }
}
