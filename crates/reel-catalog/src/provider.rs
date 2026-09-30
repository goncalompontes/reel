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

        let Some(mut metadata) = self.client.resolve(&query).await? else {
            return Ok(None);
        };

        self.ensure_artwork(&mut metadata).await;
        let _ = self.cache.write_metadata(&metadata);
        let _ = self
            .cache
            .write_metadata_at("index", &index_key, &metadata);

        Ok(Some(metadata))
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
fn query_index_key(query: &LookupQuery) -> String {
    let title = crate::matching::normalize(&query.title).replace(' ', "-");
    match query.year {
        Some(year) => format!("{title}-{year}"),
        None => title,
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
        assert_eq!(query_index_key(&LookupQuery::new("The Matrix", None)), "matrix");
    }
}
