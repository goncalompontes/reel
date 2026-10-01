//! The metadata provider seam, plus the TMDB implementation.
//!
//! A provider answers "what film is this?" and knows where its artwork lives.
//! Keeping it behind a trait means the desktop app can be driven by a
//! fixture-backed provider in tests, and means another source (a local NFO
//! reader, a different API) needs no changes above this line.

use std::sync::Arc;

use crate::cache::CatalogCache;
use crate::error::CatalogError;
use crate::matching::LookupQuery;
use crate::model::{ArtworkKind, ArtworkSize, Candidate, Metadata};
use crate::release::MediaKind;
use crate::search::BoxFuture;
use crate::tmdb::TmdbClient;

pub trait MetadataProvider: Send + Sync {
    /// Short name, shown in settings.
    fn name(&self) -> &str;

    /// False when the provider has no credentials and cannot be used.
    fn is_configured(&self) -> bool;

    /// Resolve a title, downloading artwork into the cache as a side effect.
    fn lookup<'a>(
        &'a self,
        query: LookupQuery,
    ) -> BoxFuture<'a, Result<Option<Metadata>, CatalogError>>;

    /// Absolute URL for an artwork path, at the requested size.
    fn artwork_url(&self, remote_path: &str, kind: ArtworkKind, size: ArtworkSize) -> String;
}

/// A provider that knows nothing. Used when no API key is configured so the app
/// still runs, with generated artwork instead of posters.
pub struct NullProvider;

impl MetadataProvider for NullProvider {
    fn name(&self) -> &str {
        "none"
    }

    fn is_configured(&self) -> bool {
        false
    }

    fn lookup<'a>(
        &'a self,
        _query: LookupQuery,
    ) -> BoxFuture<'a, Result<Option<Metadata>, CatalogError>> {
        Box::pin(async { Err(CatalogError::NotConfigured) })
    }

    fn artwork_url(&self, _remote_path: &str, _kind: ArtworkKind, _size: ArtworkSize) -> String {
        String::new()
    }
}

/// TMDB, with a disk cache in front of both the API and the images.
pub struct TmdbProvider {
    client: TmdbClient,
    cache: Arc<CatalogCache>,
}

impl TmdbProvider {
    pub fn new(client: TmdbClient, cache: Arc<CatalogCache>) -> Self {
        Self { client, cache }
    }

    pub fn client(&self) -> &TmdbClient {
        &self.client
    }

    pub fn cache(&self) -> &CatalogCache {
        &self.cache
    }

    /// Look up metadata, preferring the cache and falling back to the API.
    ///
    /// Artwork downloads are best-effort: being offline degrades the result to
    /// generated artwork rather than failing the whole lookup.
    pub async fn enrich(&self, query: LookupQuery) -> Result<Option<Metadata>, CatalogError> {
        // The cache is keyed by provider id, but we are looking up by title, so
        // a resolved "title+year -> id" mapping is what makes cache hits
        // possible. That mapping is itself cached.
        let index_key = query_index_key(&query);
        if let Some(mut cached) = self.cache.read_metadata("index", &index_key) {
            // Refresh the local artwork paths: the cache directory can move.
            self.attach_cached_artwork(&mut cached);
            return Ok(Some(cached));
        }

        let found = match query.kind {
            MediaKind::Series => self.resolve_series(&query).await?,
            // A film, or a torrent too ambiguous to call: the film catalogue is
            // the better guess, and matching will reject a bad one anyway.
            _ => self.client.resolve(&query).await?,
        };

        let Some(mut metadata) = found else {
            return Ok(None);
        };

        self.ensure_artwork(&mut metadata).await;
        self.ensure_stills(&mut metadata, &query).await;
        let _ = self.cache.write_metadata(&metadata);
        let _ = self
            .cache
            .write_metadata_at("index", &index_key, &metadata);

        Ok(Some(metadata))
    }

    /// A series needs three calls: find it, read it, then read the season the
    /// torrent actually holds.
    async fn resolve_series(&self, query: &LookupQuery) -> Result<Option<Metadata>, CatalogError> {
        let candidates = self.client.search_tv(&query.title, query.year).await?;
        let Some((chosen, score)) = crate::matching::best(query, &candidates) else {
            tracing::debug!(
                title = %query.title,
                ?query.year,
                considered = candidates.len(),
                "no series candidate scored high enough"
            );
            return Ok(None);
        };

        let id: u64 = chosen.source_id.parse().map_err(|_| {
            CatalogError::Decode(format!("non-numeric tmdb id {}", chosen.source_id))
        })?;

        tracing::debug!(
            title = %query.title,
            matched = %chosen.title,
            ?score,
            season = ?query.season,
            "matched series candidate"
        );

        let mut metadata = self.client.tv(id).await?;
        metadata.popularity = chosen.popularity;

        // A dated show has no season in its name, so the date is the only way
        // to find which season it belongs to.
        let mut seasons = query.seasons.clone();
        if seasons.is_empty() && !query.air_dates.is_empty() {
            let dates: Vec<&String> = query.air_dates.iter().collect();
            let mut guessed: Vec<u32> = dates
                .iter()
                .filter_map(|date| metadata.season_for_date(date))
                .collect();
            guessed.sort_unstable();
            guessed.dedup();
            if !guessed.is_empty() {
                tracing::debug!(?guessed, "located seasons by air date");
            }
            seasons = guessed;
        }

        // Without a season we know the show but not which episodes the torrent
        // holds; the interface falls back to listing files.
        for season in seasons {
            match self.client.tv_season(id, season).await {
                Ok(episodes) => metadata.episodes.extend(episodes),
                Err(e) => tracing::debug!(season, error = %e, "could not read the season"),
            }
        }

        Ok(Some(metadata))
    }

    /// Download thumbnails for the episodes the torrent holds, and only those:
    /// a season can be twenty-odd images and a torrent usually has a handful.
    async fn ensure_stills(&self, metadata: &mut Metadata, query: &LookupQuery) {
        if query.episodes.is_empty() {
            return;
        }
        let source = metadata.source.clone();
        let series_id = metadata.source_id.clone();

        // Only episodes the torrent holds get a thumbnail: a season can be
        // twenty-odd images and a torrent is usually a handful. A dated show
        // has no numbers yet, so its dates stand in for them.
        let wanted: Vec<(u32, u32, Option<String>)> = metadata
            .episodes
            .iter()
            .filter(|episode| {
                (query.seasons.is_empty() || query.seasons.contains(&episode.season))
                    && query.episodes.contains(&episode.number)
                    && (query.air_dates.is_empty()
                        || episode
                            .air_date
                            .as_ref()
                            .is_some_and(|date| query.air_dates.contains(date)))
            })
            .map(|episode| (episode.season, episode.number, episode.air_date.clone()))
            .collect();

        for (season, number, _air_date) in wanted {
            let Some(path) = metadata
                .episode(season, number)
                .and_then(|e| e.still.as_ref())
                .map(|still| still.remote_path.clone())
            else {
                continue;
            };

            let key = format!("{series_id}-s{season}e{number}");
            if let Some(existing) =
                self.cache
                    .cached_artwork(&source, &key, ArtworkKind::Poster, ArtworkSize::Card)
            {
                if let Some(still) = metadata
                    .episodes
                    .iter_mut()
                    .find(|e| e.season == season && e.number == number)
                    .and_then(|e| e.still.as_mut())
                {
                    still.local_path = Some(existing);
                }
                continue;
            }

            let url = self.client.still_url(&path, ArtworkSize::Card);
            match self.download(&url).await {
                Ok(bytes) => {
                    match self
                        .cache
                        .store_artwork(&source, &key, ArtworkKind::Poster, ArtworkSize::Card, &bytes)
                    {
                        Ok(saved) => {
                            if let Some(still) = metadata
                                .episodes
                                .iter_mut()
                                .find(|e| e.season == season && e.number == number)
                                .and_then(|e| e.still.as_mut())
                            {
                                still.local_path = Some(saved);
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "could not cache an episode still"),
                    }
                }
                Err(e) => tracing::debug!(url = %url, error = %e, "still download failed"),
            }
        }
    }

    /// Download the poster and backdrop if they are not already cached, and
    /// point the metadata at the local copies.
    async fn ensure_artwork(&self, metadata: &mut Metadata) {
        let source = metadata.source.clone();
        let id = metadata.source_id.clone();

        for (kind, size) in [
            (ArtworkKind::Poster, ArtworkSize::Card),
            (ArtworkKind::Backdrop, ArtworkSize::Hero),
        ] {
            let Some(reference) = metadata.artwork.get(kind) else {
                continue;
            };
            let remote_path = reference.remote_path.clone();

            if let Some(existing) =
                self.cache
                    .cached_artwork(&source, &id, kind, size)
            {
                if let Some(reference) = metadata.artwork.get_mut(kind) {
                    reference.local_path = Some(existing);
                }
                continue;
            }

            let url = self.client_url(&remote_path, kind, size);
            match self.download(&url).await {
                Ok(bytes) => match self.cache.store_artwork(&source, &id, kind, size, &bytes) {
                    Ok(path) => {
                        if let Some(reference) = metadata.artwork.get_mut(kind) {
                            reference.local_path = Some(path);
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "could not cache artwork"),
                },
                Err(e) => {
                    // Offline, or the image was removed upstream. Not fatal.
                    tracing::debug!(url = %url, error = %e, "artwork download failed");
                }
            }
        }
    }

    fn client_url(&self, remote_path: &str, kind: ArtworkKind, size: ArtworkSize) -> String {
        match kind {
            ArtworkKind::Poster => self.client.poster_url(remote_path, size),
            ArtworkKind::Backdrop => self.client.backdrop_url(remote_path, size),
        }
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>, CatalogError> {
        let response = self
            .client
            .raw_http()
            .get(url)
            .send()
            .await?
            .error_for_status()?;
        Ok(response.bytes().await?.to_vec())
    }

    /// Re-point artwork references at files that exist right now.
    fn attach_cached_artwork(&self, metadata: &mut Metadata) {
        let (source, id) = (metadata.source.clone(), metadata.source_id.clone());

        // Episode stills, if this is a series.
        for episode in metadata.episodes.iter_mut() {
            let key = format!("{id}-s{}e{}", episode.season, episode.number);
            let found = self
                .cache
                .cached_artwork(&source, &key, ArtworkKind::Poster, ArtworkSize::Card);
            if let Some(still) = episode.still.as_mut() {
                still.local_path = found;
            }
        }
        for (kind, size) in [
            (ArtworkKind::Poster, ArtworkSize::Card),
            (ArtworkKind::Backdrop, ArtworkSize::Hero),
        ] {
            let found = self.cache.cached_artwork(&source, &id, kind, size);
            if let Some(reference) = metadata.artwork.get_mut(kind) {
                reference.local_path = found;
            }
        }
    }
}

impl MetadataProvider for TmdbProvider {
    fn name(&self) -> &str {
        "tmdb"
    }

    fn is_configured(&self) -> bool {
        true
    }

    fn lookup<'a>(
        &'a self,
        query: LookupQuery,
    ) -> BoxFuture<'a, Result<Option<Metadata>, CatalogError>> {
        Box::pin(self.enrich(query))
    }

    fn artwork_url(&self, remote_path: &str, kind: ArtworkKind, size: ArtworkSize) -> String {
        self.client_url(remote_path, kind, size)
    }
}

/// A provider backed by fixed entries, for tests and offline demos.
///
/// It uses the same matching code as the real provider, so tests exercise the
/// scoring rather than a shortcut.
pub struct StaticProvider {
    entries: Vec<Metadata>,
}

impl StaticProvider {
    pub fn new(entries: Vec<Metadata>) -> Self {
        Self { entries }
    }

    fn as_candidates(&self) -> Vec<Candidate> {
        self.entries
            .iter()
            .map(|metadata| Candidate {
                source: metadata.source.clone(),
                source_id: metadata.source_id.clone(),
                title: metadata.title.clone(),
                original_title: metadata.original_title.clone(),
                year: metadata.year,
                overview: metadata.overview.clone(),
                rating: metadata.rating,
                vote_count: metadata.vote_count,
                popularity: metadata.popularity,
                poster_path: metadata
                    .artwork
                    .poster
                    .as_ref()
                    .map(|a| a.remote_path.clone()),
                backdrop_path: metadata
                    .artwork
                    .backdrop
                    .as_ref()
                    .map(|a| a.remote_path.clone()),
            })
            .collect()
    }
}

impl MetadataProvider for StaticProvider {
    fn name(&self) -> &str {
        "static"
    }

    fn is_configured(&self) -> bool {
        true
    }

    fn lookup<'a>(
        &'a self,
        query: LookupQuery,
    ) -> BoxFuture<'a, Result<Option<Metadata>, CatalogError>> {
        Box::pin(async move {
            let candidates = self.as_candidates();
            let Some((chosen, _)) = crate::matching::best(&query, &candidates) else {
                return Ok(None);
            };
            Ok(self
                .entries
                .iter()
                .find(|m| m.source_id == chosen.source_id)
                .cloned())
        })
    }

    fn artwork_url(&self, remote_path: &str, _kind: ArtworkKind, _size: ArtworkSize) -> String {
        remote_path.to_string()
    }
}

/// Cache key for a title lookup. A plain, filesystem-safe slug.
///
/// Includes the kind and season: the same title can be both a film and a series,
/// and two seasons of one series are different lookups.
fn query_index_key(query: &LookupQuery) -> String {
    let title = crate::matching::normalize(&query.title).replace(' ', "-");
    let kind = match query.kind {
        MediaKind::Series => "series-",
        MediaKind::Movie => "movie-",
        MediaKind::Unknown => "unknown-",
    };
    // Every season is part of the identity: a pack of seasons 1-3 is not the
    // same lookup as season 1 alone.
    let seasons = if query.seasons.is_empty() {
        String::new()
    } else {
        format!(
            "-s{}",
            query
                .seasons
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join("+")
        )
    };
    match query.year {
        Some(year) => format!("{kind}{title}-{year}{seasons}"),
        None => format!("{kind}{title}{seasons}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(id: &str, title: &str, year: u16) -> Metadata {
        Metadata {
            source: "test".into(),
            source_id: id.into(),
            title: title.into(),
            year: Some(year),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn the_static_provider_uses_the_real_matching_rules() {
        let provider = StaticProvider::new(vec![
            metadata("1", "The Matrix", 1999),
            metadata("2", "The Matrix Reloaded", 2003),
        ]);

        // Callers hand over torrent names; cleaning them is part of the query.
        let found = provider
            .lookup(LookupQuery::from_release_name(
                "The.Matrix.1999.1080p.BluRay.x264-GROUP",
            ))
            .await
            .unwrap()
            .expect("should match the original");
        assert_eq!(found.source_id, "1");

        // The sequel must not be returned for the original.
        let original = provider
            .lookup(LookupQuery::new("The Matrix", Some(1999)))
            .await
            .unwrap();
        assert_eq!(original.map(|m| m.source_id), Some("1".to_string()));

        let nothing = StaticProvider::new(vec![metadata("2", "The Matrix Reloaded", 2003)]);
        assert!(nothing
            .lookup(LookupQuery::new("The Matrix", Some(1999)))
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn the_null_provider_is_honest_about_being_empty() {
        let provider = NullProvider;
        assert!(!provider.is_configured());
        assert_eq!(provider.name(), "none");
    }

    #[test]
    fn index_keys_are_filesystem_safe() {
        let key = query_index_key(&LookupQuery::new("Amélie / Le Fabuleux", Some(2001)));
        assert!(!key.contains('/'), "{key}");
        assert!(!key.contains(' '), "{key}");
        assert!(key.ends_with("-2001"), "{key}");
    }

    #[test]
    fn index_keys_separate_the_things_that_are_actually_different() {
        let film = LookupQuery::new("Fargo", Some(1996));
        let series = LookupQuery {
            kind: MediaKind::Series,
            ..LookupQuery::new("Fargo", Some(2014))
        };
        // Same title, different medium: different cache entries, or one would
        // serve the other's metadata.
        assert_ne!(query_index_key(&film), query_index_key(&series));

        // Different seasons of one series are different lookups, and a pack of
        // several is different again from any one of them.
        let s1 = LookupQuery {
            kind: MediaKind::Series,
            season: Some(1),
            seasons: vec![1],
            ..LookupQuery::new("Some Show", None)
        };
        let s2 = LookupQuery {
            seasons: vec![2],
            ..s1.clone()
        };
        let pack = LookupQuery {
            seasons: vec![1, 2, 3],
            ..s1.clone()
        };
        assert_ne!(query_index_key(&s1), query_index_key(&s2));
        assert_ne!(query_index_key(&s1), query_index_key(&pack));
        assert!(query_index_key(&s1).contains("s1"), "{}", query_index_key(&s1));
        assert!(query_index_key(&pack).contains("s1+2+3"), "{}", query_index_key(&pack));
    }
}
