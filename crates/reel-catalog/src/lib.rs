//! `reel-catalog` — metadata, artwork and watch history.
//!
//! This is the layer that turns a torrent into something that looks like a
//! catalogue entry: what the film is called, what it is about, who made it, and
//! a poster to click on. It also remembers how far through it you got.
//!
//! # Boundaries
//!
//! * It knows nothing about torrents or HTTP streaming — it takes a title and a
//!   year and answers with [`model::Metadata`].
//! * It ships **no search backends**. [`search::SearchBackend`] exists so a
//!   source can be plugged in by whoever runs the app, and the crate's own tests
//!   use a fake.
//! * The metadata source is behind [`provider::MetadataProvider`], so the app
//!   runs without any API key (generated artwork, no synopsis) and tests run
//!   without a network.
//!
//! # A note on matching
//!
//! Picking the right entry is the part of a catalogue that fails most visibly:
//! searching for *The Matrix* returns *The Matrix Reloaded*, or a remake steals
//! the match from the original. [`matching`] is written so the failure mode is
//! "no match" rather than "confidently wrong match", and it is exhaustively
//! unit-tested.

pub mod cache;
pub mod error;
pub mod history;
pub mod matching;
pub mod model;
pub mod provider;
pub mod rows;
pub mod search;
pub mod tmdb;

pub use cache::CatalogCache;
pub use error::CatalogError;
pub use history::WatchHistory;
pub use matching::{LookupQuery, MatchScore};
pub use model::{
    Artwork, ArtworkKind, ArtworkRef, ArtworkSize, Candidate, CatalogEntry, Metadata, WatchProgress,
};
pub use provider::{MetadataProvider, NullProvider, StaticProvider, TmdbProvider};
pub use rows::{Row, RowKind, build_rows};
pub use search::{SearchAggregator, SearchError, SearchHit, SearchQuery, SearchResults};
pub use tmdb::TmdbClient;

/// Where the cache and watch history live by default, under a data directory.
pub fn default_data_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("REEL_DATA_DIR") {
        return std::path::PathBuf::from(dir);
    }
    if let Ok(home) = std::env::var("HOME") {
        return std::path::PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("reel");
    }
    std::path::PathBuf::from("./reel-data")
}

/// Build a provider from an optional API key.
///
/// With no key the app still runs, just without real metadata.
pub fn provider_from_key(
    api_key: Option<&str>,
    cache: std::sync::Arc<CatalogCache>,
) -> std::sync::Arc<dyn MetadataProvider> {
    match api_key.map(str::trim).filter(|key| !key.is_empty()) {
        Some(key) => match TmdbClient::new(key) {
            Ok(client) => std::sync::Arc::new(TmdbProvider::new(client, cache)),
            Err(e) => {
                tracing::warn!(error = %e, "could not build the metadata provider");
                std::sync::Arc::new(NullProvider)
            }
        },
        None => std::sync::Arc::new(NullProvider),
    }
}

/// Enrich a batch of library entries, one at a time.
///
/// Sequential on purpose: a fresh library can be hundreds of titles, and
/// providers rate-limit. A caller that wants parallelism can drive
/// [`MetadataProvider::lookup`] itself.
pub async fn enrich_entries(
    provider: &dyn MetadataProvider,
    entries: &[CatalogEntry],
) -> Vec<Option<Metadata>> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let query = LookupQuery::new(entry.display_title.clone(), entry.year);
        match provider.lookup(query).await {
            Ok(metadata) => out.push(metadata),
            Err(e) => {
                tracing::debug!(title = %entry.display_title, error = %e, "metadata lookup failed");
                out.push(None);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_or_blank_key_yields_the_null_provider() {
        let cache = std::sync::Arc::new(CatalogCache::new("/tmp/reel-catalog-test"));
        assert_eq!(provider_from_key(None, cache.clone()).name(), "none");
        assert_eq!(provider_from_key(Some("  "), cache.clone()).name(), "none");
        assert_eq!(
            provider_from_key(Some("a-real-looking-key"), cache).name(),
            "tmdb"
        );
    }

    #[test]
    fn the_data_directory_can_be_overridden() {
        // SAFETY: single-threaded test process, and the value is restored below.
        unsafe { std::env::set_var("REEL_DATA_DIR", "/tmp/reel-explicit") };
        assert_eq!(default_data_dir(), std::path::PathBuf::from("/tmp/reel-explicit"));
        unsafe { std::env::remove_var("REEL_DATA_DIR") };
        // With no override it lands somewhere plausible, not empty.
        assert!(!default_data_dir().as_os_str().is_empty());
    }

    #[tokio::test]
    async fn enriching_nothing_returns_nothing() {
        let provider = NullProvider;
        assert!(enrich_entries(&provider, &[]).await.is_empty());
    }

    #[tokio::test]
    async fn a_provider_failure_becomes_a_missing_metadata_not_a_panic() {
        // NullProvider errors on every lookup; the batch must still line up.
        let entries = vec![CatalogEntry {
            torrent_id: 1,
            info_hash: "x".into(),
            display_title: "Anything".into(),
            year: None,
            metadata: None,
            watch: None,
        }];
        let enriched = enrich_entries(&NullProvider, &entries).await;
        assert_eq!(enriched.len(), 1);
        assert!(enriched[0].is_none());
    }
}
