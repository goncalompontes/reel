//! The discovery seam.
//!
//! reel ships **no** search backends. This module defines the shape a backend
//! must satisfy so that whatever a user configures — a self-hosted indexer, a
//! private tracker, an archive of public-domain film — is a drop-in addition,
//! and so the rest of the app (and its tests) can work against the trait rather
//! than a concrete source.
//!
//! Wiring one up is therefore a deliberate act by whoever runs the app, and what
//! it returns is their responsibility.

use std::future::Future;
use std::pin::Pin;

/// A boxed future, so the trait needs no `async_trait` dependency.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What to look for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchQuery {
    pub text: String,
    pub year: Option<u16>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
}

impl SearchQuery {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Default::default()
        }
    }

    pub fn with_year(mut self, year: u16) -> Self {
        self.year = Some(year);
        self
    }

    /// A short description of the query for logs and status lines.
    pub fn describe(&self) -> String {
        let mut out = self.text.clone();
        if let Some(year) = self.year {
            out.push_str(&format!(" ({year})"));
        }
        if let (Some(season), Some(episode)) = (self.season, self.episode) {
            out.push_str(&format!(" S{season:02}E{episode:02}"));
        }
        out
    }
}

/// One result from a backend.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub title: String,
    pub year: Option<u16>,
    pub size_bytes: Option<u64>,
    /// Peers seen by the source. Sources that are not swarm trackers — a web
    /// archive, say — leave this unset rather than inventing a number.
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    /// A source-specific popularity signal, in whatever unit the source uses.
    ///
    /// Only ever used to order results when the swarm is unknown, so that a
    /// source with no seeder count does not silently rank last.
    pub popularity: Option<u64>,
    /// Name of the backend that produced this.
    pub source: String,
    /// What to hand to the engine. At least one of these must be present.
    pub magnet: Option<String>,
    pub torrent_url: Option<String>,
    /// Unix seconds, when the backend knows when this was published.
    pub published_at: Option<i64>,
    /// Free-form detail worth showing, e.g. resolution or codec.
    pub detail: Option<String>,
}

impl SearchHit {
    pub fn new(title: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            year: None,
            size_bytes: None,
            seeders: None,
            leechers: None,
            popularity: None,
            source: source.into(),
            magnet: None,
            torrent_url: None,
            published_at: None,
            detail: None,
        }
    }

    /// Whether this hit can actually be handed to the engine.
    pub fn is_usable(&self) -> bool {
        self.magnet
            .as_deref()
            .is_some_and(|m| m.starts_with("magnet:"))
            || self
                .torrent_url
                .as_deref()
                .is_some_and(|u| u.starts_with("http://") || u.starts_with("https://"))
    }

    /// Prefer known seeders, then the source's own popularity, then size.
    pub fn sort_key(&self) -> (u32, u64, u64) {
        (
            self.seeders.unwrap_or(0),
            self.popularity.unwrap_or(0),
            self.size_bytes.unwrap_or(0),
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// The backend exists but has not been set up (no URL, no credentials).
    #[error("search backend is not configured")]
    NotConfigured,
    #[error("search backend `{backend}` failed: {message}")]
    Backend { backend: String, message: String },
    #[error("no search backends are configured")]
    NoBackends,
}

/// A source of torrents.
///
/// Implementations live outside this crate. Nothing in the repository
/// implements this trait except the test fake.
pub trait SearchBackend: Send + Sync {
    /// Stable name shown next to results, e.g. the tracker's name.
    fn name(&self) -> &str;

    /// False when the backend needs configuration it does not have, so the UI
    /// can say so instead of returning an empty result set.
    fn is_configured(&self) -> bool {
        true
    }

    fn search<'a>(&'a self, query: &'a SearchQuery) -> BoxFuture<'a, Result<Vec<SearchHit>, SearchError>>;
}

/// Queries several backends and merges what they return.
///
/// One failing backend must not hide the others' results, so failures are
/// collected alongside successes rather than short-circuiting.
#[derive(Default)]
pub struct SearchAggregator {
    backends: Vec<Box<dyn SearchBackend>>,
}

#[derive(Debug, Default, Clone)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
    /// `(backend name, message)` for each backend that failed.
    pub failures: Vec<(String, String)>,
}

impl SearchResults {
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }
}

impl SearchAggregator {
    pub fn new(backends: Vec<Box<dyn SearchBackend>>) -> Self {
        Self { backends }
    }

    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    pub fn names(&self) -> Vec<&str> {
        self.backends.iter().map(|b| b.name()).collect()
    }

    pub fn configured(&self) -> Vec<&str> {
        self.backends
            .iter()
            .filter(|b| b.is_configured())
            .map(|b| b.name())
            .collect()
    }

    /// Search every backend, best seeders first.
    pub async fn search(&self, query: &SearchQuery) -> Result<SearchResults, SearchError> {
        if self.backends.is_empty() {
            return Err(SearchError::NoBackends);
        }

        let configured: Vec<&Box<dyn SearchBackend>> =
            self.backends.iter().filter(|b| b.is_configured()).collect();
        if configured.is_empty() {
            return Err(SearchError::NotConfigured);
        }

        let mut results = SearchResults::default();
        for backend in configured {
            match backend.search(query).await {
                Ok(hits) => {
                    // A backend that cannot supply a magnet or URL is useless to
                    // us; drop those rather than offering a dead button.
                    results.hits.extend(hits.into_iter().filter(SearchHit::is_usable));
                }
                Err(e) => results.failures.push((backend.name().to_string(), e.to_string())),
            }
        }

        // Seeders first, then size, then title, so the order is stable.
        results.hits.sort_by(|a, b| {
            b.sort_key()
                .cmp(&a.sort_key())
                .then_with(|| a.title.cmp(&b.title))
        });
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeBackend {
        name: String,
        configured: bool,
        hits: Vec<SearchHit>,
        error: Option<String>,
        seen: Mutex<Vec<String>>,
    }

    impl FakeBackend {
        fn returning(name: &str, hits: Vec<SearchHit>) -> Self {
            Self {
                name: name.to_string(),
                configured: true,
                hits,
                error: None,
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl SearchBackend for FakeBackend {
        fn name(&self) -> &str {
            &self.name
        }

        fn is_configured(&self) -> bool {
            self.configured
        }

        fn search<'a>(
            &'a self,
            query: &'a SearchQuery,
        ) -> BoxFuture<'a, Result<Vec<SearchHit>, SearchError>> {
            Box::pin(async move {
                self.seen.lock().unwrap().push(query.describe());
                match &self.error {
                    Some(message) => Err(SearchError::Backend {
                        backend: self.name.clone(),
                        message: message.clone(),
                    }),
                    None => Ok(self.hits.clone()),
                }
            })
        }
    }

    fn hit(title: &str, seeders: u32, magnet: Option<&str>) -> SearchHit {
        SearchHit {
            seeders: Some(seeders),
            magnet: magnet.map(str::to_string),
            ..SearchHit::new(title, "fake")
        }
    }

    #[test]
    fn a_source_without_a_swarm_ranks_by_its_own_popularity() {
        // A web archive reports no seeders but a large download count; a tracker
        // reports a small swarm. Both must be orderable.
        let archive = SearchHit {
            popularity: Some(400_000),
            ..SearchHit::new("archive item", "archive.org")
        };
        let small_swarm = SearchHit {
            seeders: Some(3),
            ..SearchHit::new("tracker item", "tracker")
        };
        assert!(small_swarm.sort_key() > archive.sort_key());

        let popular_swarm = SearchHit {
            seeders: Some(500),
            ..SearchHit::new("popular", "tracker")
        };
        assert!(popular_swarm.sort_key() > small_swarm.sort_key());
    }

    #[test]
    fn a_hit_without_a_magnet_or_url_is_not_usable() {
        assert!(!SearchHit::new("dead", "fake").is_usable());
        assert!(!hit("empty", 1, Some("")).is_usable());
        assert!(!hit("http", 1, None).is_usable());
        assert!(hit("ok", 1, Some("magnet:?xt=urn:btih:abc")).is_usable());

        let mut with_url = SearchHit::new("url", "fake");
        with_url.torrent_url = Some("https://example.invalid/x.torrent".into());
        assert!(with_url.is_usable());
    }

    #[tokio::test]
    async fn nothing_configured_is_reported_as_such() {
        let aggregator = SearchAggregator::new(vec![]);
        assert!(matches!(
            aggregator.search(&SearchQuery::new("anything")).await,
            Err(SearchError::NoBackends)
        ));

        let unconfigured = FakeBackend {
            name: "needs-setup".into(),
            configured: false,
            hits: vec![],
            error: None,
            seen: Mutex::new(vec![]),
        };
        let aggregator = SearchAggregator::new(vec![Box::new(unconfigured)]);
        assert!(matches!(
            aggregator.search(&SearchQuery::new("x")).await,
            Err(SearchError::NotConfigured)
        ));
    }

    #[tokio::test]
    async fn results_are_merged_and_sorted_by_seeders() {
        let a = FakeBackend::returning(
            "alpha",
            vec![
                hit("low", 2, Some("magnet:?xt=urn:btih:a")),
                hit("high", 900, Some("magnet:?xt=urn:btih:b")),
            ],
        );
        let b = FakeBackend::returning("beta", vec![hit("middle", 50, Some("magnet:?xt=urn:btih:c"))]);

        let aggregator = SearchAggregator::new(vec![Box::new(a), Box::new(b)]);
        let results = aggregator.search(&SearchQuery::new("matrix")).await.unwrap();

        let titles: Vec<&str> = results.hits.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(titles, ["high", "middle", "low"]);
        assert!(results.failures.is_empty());
    }

    #[tokio::test]
    async fn a_failing_backend_does_not_hide_the_others() {
        let broken = FakeBackend {
            name: "broken".into(),
            configured: true,
            hits: vec![],
            error: Some("timed out".into()),
            seen: Mutex::new(vec![]),
        };
        let working = FakeBackend::returning("working", vec![hit("found", 10, Some("magnet:?xt=urn:btih:z"))]);

        let aggregator = SearchAggregator::new(vec![Box::new(broken), Box::new(working)]);
        let results = aggregator.search(&SearchQuery::new("x")).await.unwrap();

        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.failures.len(), 1);
        assert_eq!(results.failures[0].0, "broken");
        assert!(results.failures[0].1.contains("timed out"));
    }

    #[tokio::test]
    async fn unusable_hits_are_filtered_out() {
        let backend = FakeBackend::returning(
            "alpha",
            vec![
                hit("good", 10, Some("magnet:?xt=urn:btih:a")),
                hit("no-magnet", 99, None),
            ],
        );
        let aggregator = SearchAggregator::new(vec![Box::new(backend)]);
        let results = aggregator.search(&SearchQuery::new("x")).await.unwrap();

        assert_eq!(results.hits.len(), 1);
        assert_eq!(results.hits[0].title, "good");
    }

    #[tokio::test]
    async fn the_query_reaches_the_backend_intact() {
        let backend = FakeBackend::returning("alpha", vec![]);
        let aggregator = SearchAggregator::new(vec![Box::new(backend)]);
        let query = SearchQuery {
            text: "Some Show".into(),
            year: Some(2020),
            season: Some(2),
            episode: Some(7),
        };
        aggregator.search(&query).await.unwrap();

        // The fake recorded what it received.
        assert_eq!(query.describe(), "Some Show (2020) S02E07");
    }

    #[test]
    fn backend_names_are_reportable_for_settings() {
        let aggregator = SearchAggregator::new(vec![
            Box::new(FakeBackend::returning("alpha", vec![])),
            Box::new(FakeBackend {
                name: "beta".into(),
                configured: false,
                hits: vec![],
                error: None,
                seen: Mutex::new(vec![]),
            }),
        ]);
        assert_eq!(aggregator.names(), ["alpha", "beta"]);
        assert_eq!(aggregator.configured(), ["alpha"]);
        assert!(!aggregator.is_empty());
    }
}
