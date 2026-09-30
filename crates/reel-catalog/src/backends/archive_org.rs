//! A [`SearchBackend`] for the Internet Archive.
//!
//! The Archive publishes public-domain and Creative Commons film, and every
//! item it hosts has a BitTorrent file at a predictable URL. That makes it a
//! genuinely useful source *and* a clean template: one HTTP request for search,
//! no scraping, no anti-bot games, no credentials.
//!
//! The shape of a real backend is all here:
//!
//! * building a query safely from user text,
//! * issuing one request through a client with a timeout and a user agent,
//! * parsing a response whose field types are not consistent,
//! * mapping results onto [`SearchHit`], including the URL the engine will fetch,
//! * reporting failures as [`SearchError`] rather than panicking,
//! * separating the pure parts (query, parsing) so they can be tested without a
//!   network, with one `#[ignore]`d test that does use it.

use serde_json::Value;

use crate::error::CatalogError;
use crate::search::{BoxFuture, SearchBackend, SearchError, SearchHit, SearchQuery};
use std::time::Duration;

/// The Internet Archive's search endpoint.
pub const DEFAULT_SEARCH_URL: &str = "https://archive.org/advancedsearch.php";

/// Where items are downloaded from, and where their `.torrent` files live.
pub const DEFAULT_DOWNLOAD_BASE: &str = "https://archive.org/download";

/// Name shown next to results.
pub const SOURCE_NAME: &str = "archive.org";

pub struct ArchiveOrgBackend {
    http: reqwest::Client,
    search_url: String,
    download_base: String,
    /// How many results to ask for.
    rows: usize,
    /// Whether the backend is usable. Present so the trait's "needs
    /// configuration" path is exercised by a real backend.
    enabled: bool,
}

impl Default for ArchiveOrgBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ArchiveOrgBackend {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("reel/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(25))
            .build()
            .unwrap_or_default();

        Self {
            http,
            search_url: DEFAULT_SEARCH_URL.to_string(),
            download_base: DEFAULT_DOWNLOAD_BASE.to_string(),
            rows: 20,
            enabled: true,
        }
    }

    /// Point at a different host. Used by tests, and useful behind a proxy or a
    /// mirror.
    pub fn with_search_url(mut self, url: impl Into<String>) -> Self {
        self.search_url = url.into();
        self
    }

    pub fn with_download_base(mut self, base: impl Into<String>) -> Self {
        self.download_base = base.into().trim_end_matches('/').to_string();
        self
    }

    pub fn with_rows(mut self, rows: usize) -> Self {
        self.rows = rows.clamp(1, 100);
        self
    }

    /// Switch the backend off, so the UI can show it as configured-but-disabled.
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// The `.torrent` URL for an Archive item.
    pub fn torrent_url(&self, identifier: &str) -> String {
        format!("{}/{identifier}/{identifier}_archive.torrent", self.download_base)
    }

    /// The page a person would open in a browser.
    pub fn detail_url(&self, identifier: &str) -> String {
        format!("https://archive.org/details/{identifier}")
    }
}

/// Build the Archive's Lucene-ish query for a user's text.
///
/// Constrained to film, and to items that actually carry a BitTorrent file: an
/// item without one would produce a result we cannot add, which is worse than
/// no result. Quotes are stripped so a stray one cannot break the query.
pub fn build_query(text: &str) -> String {
    let text = text.replace('"', " ").replace(['(', ')'], " ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("mediatype:movies AND format:\"Archive BitTorrent\" AND title:({text})")
}

/// Parse an `advancedsearch.php` response.
pub fn parse_search(body: &Value, backend: &ArchiveOrgBackend) -> Result<Vec<SearchHit>, SearchError> {
    let docs = body
        .pointer("/response/docs")
        .and_then(|docs| docs.as_array())
        .ok_or_else(|| SearchError::Backend {
            backend: SOURCE_NAME.to_string(),
            message: "response has no /response/docs array".to_string(),
        })?;

    Ok(docs
        .iter()
        .filter_map(|doc| parse_doc(doc, backend))
        .collect())
}

fn parse_doc(doc: &Value, backend: &ArchiveOrgBackend) -> Option<SearchHit> {
    let identifier = first_string(doc.get("identifier"))?;
    if identifier.is_empty() {
        return None;
    }

    let title = first_string(doc.get("title")).unwrap_or_else(|| identifier.clone());

    let mut hit = SearchHit::new(title, SOURCE_NAME);
    hit.year = first_year(doc.get("year"));
    hit.torrent_url = Some(backend.torrent_url(&identifier));
    // Archive items are not seeds in a swarm we can count before connecting, so
    // downloads stand in as the only popularity signal.
    hit.popularity = first_u64(doc.get("downloads"));
    hit.detail = Some(format!("archive.org \u{2022} {identifier}"));
    Some(hit)
}

/// Archive fields come back as either a scalar or a one-element array,
/// depending on the field and the item.
fn first_string(value: Option<&Value>) -> Option<String> {
    fn one(item: &Value) -> Option<String> {
        match item {
            Value::String(text) => Some(text.trim().to_string()),
            // Fields such as `year` come back as numbers.
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        }
    }

    match value? {
        Value::Array(items) => items.iter().find_map(one),
        scalar => one(scalar),
    }
}

fn first_u64(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        Value::Array(items) => items.iter().find_map(|item| first_u64(Some(item))),
        _ => None,
    }
}

/// `1922`, `"1922"` or `"1922-03-04"` all mean 1922.
fn first_year(value: Option<&Value>) -> Option<u16> {
    let raw = first_string(value)?;
    let digits: String = raw.chars().take_while(char::is_ascii_digit).take(4).collect();
    let year: u16 = digits.parse().ok()?;
    (1870..=2200).contains(&year).then_some(year)
}

fn parse_error(error: CatalogError) -> SearchError {
    SearchError::Backend {
        backend: SOURCE_NAME.to_string(),
        message: error.to_string(),
    }
}

impl SearchBackend for ArchiveOrgBackend {
    fn name(&self) -> &str {
        SOURCE_NAME
    }

    fn is_configured(&self) -> bool {
        self.enabled
    }

    fn search<'a>(
        &'a self,
        query: &'a SearchQuery,
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, SearchError>> {
        Box::pin(async move {
            let text = match query.year {
                // The Archive indexes the year as a field, so a bare year in
                // the text would only make the title match worse.
                Some(_) => query.text.clone(),
                None => query.text.clone(),
            };

            let response = self
                .http
                .get(&self.search_url)
                .query(&[
                    ("q", build_query(&text)),
                    ("fl[]", "identifier".to_string()),
                    ("fl[]", "title".to_string()),
                    ("fl[]", "year".to_string()),
                    ("fl[]", "downloads".to_string()),
                    ("sort[]", "downloads desc".to_string()),
                    ("rows", self.rows.to_string()),
                    ("page", "1".to_string()),
                    ("output", "json".to_string()),
                ])
                .send()
                .await
                .map_err(|e| parse_error(e.into()))?;

            let status = response.status();
            let body = response.text().await.map_err(|e| parse_error(e.into()))?;

            if !status.is_success() {
                return Err(SearchError::Backend {
                    backend: SOURCE_NAME.to_string(),
                    message: format!(
                        "HTTP {status}: {}",
                        body.chars().take(200).collect::<String>()
                    ),
                });
            }

            let body: Value = serde_json::from_str(&body).map_err(|e| {
                parse_error(CatalogError::Decode(format!(
                    "{e}: {}",
                    body.chars().take(200).collect::<String>()
                )))
            })?;

            let found = body
                .pointer("/response/docs")
                .and_then(|docs| docs.as_array())
                .map(Vec::len)
                .unwrap_or(0);
            tracing::debug!(query = %query.describe(), results = found, "archive.org search");

            parse_search(&body, self)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed but structurally faithful `advancedsearch.php` response,
    /// including the field-type inconsistencies the real API produces.
    const RESPONSE: &str = r#"{
      "responseHeader": {"status": 0, "QTime": 96},
      "response": {
        "numFound": 306,
        "start": 0,
        "docs": [
          {
            "identifier": "Nosferatu_most_complete_version_93_mins.",
            "title": "Nosferatu, eine Symphonie des Grauens (A Symphony of Horror)",
            "year": 1922,
            "downloads": 432789
          },
          {
            "identifier": "Nosferatu_DVD_quality",
            "title": "Nosferatu_DVD_quality",
            "year": "1922",
            "downloads": "256804"
          },
          {
            "identifier": "array-title-item",
            "title": ["The Cabinet of Dr. Caligari", "Alternate title"],
            "year": "1920-02-26",
            "downloads": 95000
          },
          {
            "title": "No identifier, so unusable"
          }
        ]
      }
    }"#;

    fn backend() -> ArchiveOrgBackend {
        ArchiveOrgBackend::new()
    }

    #[test]
    fn the_query_is_constrained_to_film_with_a_torrent() {
        let query = build_query("nosferatu");
        assert!(query.contains("mediatype:movies"), "{query}");
        assert!(query.contains("format:\"Archive BitTorrent\""), "{query}");
        assert!(query.contains("title:(nosferatu)"), "{query}");
    }

    #[test]
    fn the_query_cannot_be_broken_by_user_input() {
        // Quotes and brackets are stripped, so user text cannot close the title
        // clause early and add a clause of its own. The wrapper must survive
        // intact, whatever the user typed.
        let hostile = r#"nos"feratu) OR mediatype:audio OR ("#;
        let query = build_query(hostile);

        assert!(
            query.starts_with(r#"mediatype:movies AND format:"Archive BitTorrent" AND title:("#),
            "{query}"
        );
        assert!(query.ends_with(')'), "{query}");
        assert_eq!(query.matches('(').count(), 1, "{query}");
        assert_eq!(query.matches(')').count(), 1, "{query}");
        assert_eq!(query.matches('"').count(), 2, "only the format quotes: {query}");

        // An empty query still produces something well-formed.
        assert!(build_query("   ").contains("title:()"));
    }

    #[test]
    fn results_map_onto_hits_with_a_torrent_url() {
        let body: Value = serde_json::from_str(RESPONSE).unwrap();
        let hits = parse_search(&body, &backend()).unwrap();

        // The entry with no identifier is dropped.
        assert_eq!(hits.len(), 3);

        let first = &hits[0];
        assert_eq!(first.source, "archive.org");
        assert_eq!(first.year, Some(1922));
        assert_eq!(first.popularity, Some(432_789));
        assert_eq!(
            first.torrent_url.as_deref(),
            Some(
                "https://archive.org/download/Nosferatu_most_complete_version_93_mins./\
                 Nosferatu_most_complete_version_93_mins._archive.torrent"
            )
        );
        assert!(first.is_usable(), "a torrent URL makes a hit addable");
        assert!(first.detail.as_deref().unwrap().contains("archive.org"));
    }

    #[test]
    fn inconsistent_field_types_are_handled() {
        let body: Value = serde_json::from_str(RESPONSE).unwrap();
        let hits = parse_search(&body, &backend()).unwrap();

        // "year" as a string, and downloads as a string.
        assert_eq!(hits[1].year, Some(1922));
        assert_eq!(hits[1].popularity, Some(256_804));

        // A title that arrives as an array, and a full ISO date.
        assert_eq!(hits[2].title, "The Cabinet of Dr. Caligari");
        assert_eq!(hits[2].year, Some(1920));
    }

    #[test]
    fn a_missing_title_falls_back_to_the_identifier() {
        let body: Value = serde_json::from_str(
            r#"{"response":{"docs":[{"identifier":"untitled-item","downloads":1}]}}"#,
        )
        .unwrap();
        let hits = parse_search(&body, &backend()).unwrap();
        assert_eq!(hits[0].title, "untitled-item");
        assert_eq!(hits[0].year, None);
    }

    #[test]
    fn nonsense_years_are_dropped_rather_than_shown() {
        assert_eq!(first_year(Some(&Value::String("0001-01-01".into()))), None);
        assert_eq!(first_year(Some(&Value::String("circa 1922".into()))), None);
        assert_eq!(first_year(Some(&Value::String("1922".into()))), Some(1922));
        assert_eq!(first_year(Some(&Value::Number(1922.into()))), Some(1922));
        assert_eq!(first_year(None), None);
    }

    #[test]
    fn a_response_without_docs_is_an_error_not_an_empty_success() {
        let body: Value = serde_json::from_str(r#"{"error":"something went wrong"}"#).unwrap();
        let error = parse_search(&body, &backend()).expect_err("should fail");
        assert!(matches!(error, SearchError::Backend { .. }));
        assert!(error.to_string().contains("archive.org"));
    }

    #[test]
    fn urls_are_configurable_for_a_mirror() {
        let backend = ArchiveOrgBackend::new()
            .with_search_url("http://localhost:1/search")
            .with_download_base("http://localhost:1/dl/");
        assert_eq!(backend.search_url, "http://localhost:1/search");
        assert_eq!(
            backend.torrent_url("some-item"),
            "http://localhost:1/dl/some-item/some-item_archive.torrent"
        );
        assert_eq!(
            backend.detail_url("some-item"),
            "https://archive.org/details/some-item"
        );
    }

    #[test]
    fn a_backend_can_report_itself_as_disabled() {
        assert!(backend().is_configured());
        assert!(!backend().disabled().is_configured());
    }

    #[tokio::test]
    async fn a_disabled_backend_is_reported_by_the_aggregator() {
        use crate::search::SearchAggregator;

        let aggregator = SearchAggregator::new(vec![Box::new(backend().disabled())]);
        let error = aggregator
            .search(&SearchQuery::new("anything"))
            .await
            .expect_err("a disabled backend cannot answer");
        assert!(matches!(error, SearchError::NotConfigured));
    }

    /// The parts the stub cannot check: that the query the Archive actually
    /// receives is one it accepts, and that real results parse.
    #[tokio::test]
    #[ignore = "requires network access"]
    async fn the_real_archive_answers_as_expected() {
        let hits = backend()
            .search(&SearchQuery::new("nosferatu"))
            .await
            .expect("the archive should answer");

        assert!(!hits.is_empty(), "expected results for a well-known film");
        assert!(
            hits.iter().all(|hit| hit.is_usable()),
            "every hit should carry a torrent URL"
        );

        // The most-downloaded version should be near the top.
        let top = &hits[0];
        println!("top hit: {:?} ({:?}) {}", top.title, top.year, top.torrent_url.as_deref().unwrap_or(""));
        assert!(top.year.is_none_or(|year| year <= 1930), "{top:?}");

        // And the torrent URL it produced must actually resolve.
        let response = reqwest::Client::new()
            .head(top.torrent_url.as_ref().unwrap())
            .send()
            .await
            .expect("HEAD should succeed");
        assert!(
            response.status().is_success(),
            "torrent URL returned {}",
            response.status()
        );
    }
}
