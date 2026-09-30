//! The TMDB client.
//!
//! Deliberately a thin, explicit HTTP client rather than a wrapper crate:
//! request building and response parsing are separated so the parsing is
//! testable against fixtures, and the base URLs are configurable so the whole
//! client can be pointed at a stub server in tests (and at a mirror or proxy in
//! the app).

use std::time::Duration;

use serde_json::Value;

use crate::error::CatalogError;
use crate::matching::LookupQuery;
use crate::model::{Artwork, ArtworkRef, ArtworkSize, Candidate, Metadata};

pub const DEFAULT_BASE_URL: &str = "https://api.themoviedb.org/3";
pub const DEFAULT_IMAGE_BASE: &str = "https://image.tmdb.org/t/p";

const PROVIDER: &str = "tmdb";

/// How to authenticate. TMDB v3 uses an `api_key` query parameter, v4 uses a
/// bearer token; both are in circulation, so we accept either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    Query(String),
    Bearer(String),
}

impl Auth {
    /// v4 tokens are JWTs and start with the base64 of `{"`.
    pub fn detect(key: &str) -> Self {
        let key = key.trim();
        if key.starts_with("eyJ") {
            Auth::Bearer(key.to_string())
        } else {
            Auth::Query(key.to_string())
        }
    }

    pub fn is_configured(&self) -> bool {
        match self {
            Auth::Query(key) | Auth::Bearer(key) => !key.trim().is_empty(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TmdbClient {
    http: reqwest::Client,
    base_url: String,
    image_base: String,
    auth: Auth,
    language: String,
}

impl TmdbClient {
    /// Build a client for an API key. Fails when the key is empty, so a
    /// half-configured app cannot silently make unauthenticated requests.
    pub fn new(api_key: impl Into<String>) -> Result<Self, CatalogError> {
        let auth = Auth::detect(&api_key.into());
        if !auth.is_configured() {
            return Err(CatalogError::NotConfigured);
        }

        let http = reqwest::Client::builder()
            .user_agent(concat!("reel/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
            .build()?;

        Ok(Self {
            http,
            base_url: DEFAULT_BASE_URL.to_string(),
            image_base: DEFAULT_IMAGE_BASE.to_string(),
            auth,
            language: "en-US".to_string(),
        })
    }

    /// Point at a different API host. Used by tests, and useful behind a proxy
    /// or a self-hosted mirror.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    pub fn with_image_base(mut self, image_base: impl Into<String>) -> Self {
        self.image_base = image_base.into().trim_end_matches('/').to_string();
        self
    }

    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = language.into();
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The underlying HTTP client, for artwork downloads from the image host
    /// (which is a different host from the API).
    pub fn raw_http(&self) -> &reqwest::Client {
        &self.http
    }

    /// URL of a poster, at the size the UI needs.
    pub fn poster_url(&self, path: &str, size: ArtworkSize) -> String {
        let size = match size {
            ArtworkSize::Card => "w342",
            ArtworkSize::Hero => "w780",
        };
        self.image_url(size, path)
    }

    /// URL of a backdrop. Backdrops are wider, so the size steps up.
    pub fn backdrop_url(&self, path: &str, size: ArtworkSize) -> String {
        let size = match size {
            ArtworkSize::Card => "w780",
            ArtworkSize::Hero => "w1280",
        };
        self.image_url(size, path)
    }

    fn image_url(&self, size: &str, path: &str) -> String {
        let path = path.trim_start_matches('/');
        format!("{}/{size}/{path}", self.image_base)
    }

    fn request(&self, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/{}", self.base_url, path.trim_start_matches('/'));
        let request = self.http.get(url);
        match &self.auth {
            Auth::Query(key) => request.query(&[("api_key", key.as_str())]),
            Auth::Bearer(token) => request.bearer_auth(token),
        }
        .query(&[("language", self.language.as_str())])
    }

    async fn get_json(
        &self,
        path: &str,
        extra: &[(&str, String)],
    ) -> Result<Value, CatalogError> {
        let response = self.request(path).query(extra).send().await?;
        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            // TMDB reports failures as {"status_message": "...", ...}.
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| {
                    v.get("status_message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| body.chars().take(200).collect());
            return Err(CatalogError::Api {
                status: status.as_u16(),
                message,
            });
        }

        serde_json::from_str(&body)
            .map_err(|e| CatalogError::Decode(format!("{e}: {}", body.chars().take(200).collect::<String>())))
    }

    /// Search for a film by name, optionally constrained to a year.
    pub async fn search_movies(
        &self,
        query: &str,
        year: Option<u16>,
    ) -> Result<Vec<Candidate>, CatalogError> {
        let mut extra: Vec<(&str, String)> = vec![("query", query.to_string())];
        if let Some(year) = year {
            extra.push(("year", year.to_string()));
        }
        let body = self.get_json("search/movie", &extra).await?;
        parse_search(&body)
    }

    /// Full details for one film.
    pub async fn movie(&self, id: u64) -> Result<Metadata, CatalogError> {
        let body = self.get_json(&format!("movie/{id}"), &[]).await?;
        parse_movie(&body)
    }

    /// What is popular this week, for a discovery row.
    pub async fn trending(&self) -> Result<Vec<Candidate>, CatalogError> {
        let body = self.get_json("trending/movie/week", &[]).await?;
        parse_search(&body)
    }

    /// Search, choose the best candidate, then fetch its details.
    ///
    /// Returns `Ok(None)` when nothing matched well enough — a miss is a better
    /// outcome than a confident wrong match.
    pub async fn resolve(&self, query: &LookupQuery) -> Result<Option<Metadata>, CatalogError> {
        let candidates = self.search_movies(&query.title, query.year).await?;
        let Some((chosen, score)) = crate::matching::best(query, &candidates) else {
            tracing::debug!(
                title = %query.title,
                ?query.year,
                considered = candidates.len(),
                "no metadata candidate scored high enough"
            );
            return Ok(None);
        };

        tracing::debug!(
            title = %query.title,
            matched = %chosen.title,
            ?score,
            "matched metadata candidate"
        );

        let id: u64 = chosen
            .source_id
            .parse()
            .map_err(|_| CatalogError::Decode(format!("non-numeric tmdb id {}", chosen.source_id)))?;
        let mut metadata = self.movie(id).await?;
        metadata.popularity = chosen.popularity;
        Ok(Some(metadata))
    }
}

/// `1999-03-30` -> `1999`.
pub fn parse_year(date: Option<&str>) -> Option<u16> {
    let date = date?;
    let year: u16 = date.get(0..4)?.parse().ok()?;
    // TMDB uses empty strings and placeholder dates for unreleased films.
    if !(1870..=2200).contains(&year) {
        return None;
    }
    Some(year)
}

fn parse_candidate(value: &Value) -> Option<Candidate> {
    let source_id = value.get("id")?.as_u64()?.to_string();
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| value.get("original_title").and_then(Value::as_str))?
        .to_string();

    Some(Candidate {
        source: PROVIDER.to_string(),
        source_id,
        title,
        original_title: value
            .get("original_title")
            .and_then(Value::as_str)
            .map(str::to_string),
        year: parse_year(value.get("release_date").and_then(Value::as_str)),
        overview: non_empty(value.get("overview").and_then(Value::as_str)),
        rating: value.get("vote_average").and_then(Value::as_f64).map(|v| v as f32),
        vote_count: value
            .get("vote_count")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        popularity: value.get("popularity").and_then(Value::as_f64).map(|v| v as f32),
        poster_path: non_empty(value.get("poster_path").and_then(Value::as_str)),
        backdrop_path: non_empty(value.get("backdrop_path").and_then(Value::as_str)),
    })
}

/// Parse a `search/movie` or `trending/movie` body.
pub fn parse_search(body: &Value) -> Result<Vec<Candidate>, CatalogError> {
    let results = body
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| CatalogError::Decode("response has no `results` array".into()))?;
    Ok(results.iter().filter_map(parse_candidate).collect())
}

/// Parse a `movie/{id}` body.
pub fn parse_movie(body: &Value) -> Result<Metadata, CatalogError> {
    let source_id = body
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| CatalogError::Decode("movie has no `id`".into()))?
        .to_string();

    let title = body
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| body.get("original_title").and_then(Value::as_str))
        .ok_or_else(|| CatalogError::Decode("movie has no title".into()))?
        .to_string();

    let genres = body
        .get("genres")
        .and_then(Value::as_array)
        .map(|genres| {
            genres
                .iter()
                .filter_map(|g| g.get("name").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut artwork = Artwork::default();
    if let Some(path) = non_empty(body.get("poster_path").and_then(Value::as_str)) {
        artwork.poster = Some(ArtworkRef::new(path));
    }
    if let Some(path) = non_empty(body.get("backdrop_path").and_then(Value::as_str)) {
        artwork.backdrop = Some(ArtworkRef::new(path));
    }

    Ok(Metadata {
        source: PROVIDER.to_string(),
        source_id,
        title,
        original_title: body
            .get("original_title")
            .and_then(Value::as_str)
            .map(str::to_string),
        year: parse_year(body.get("release_date").and_then(Value::as_str)),
        overview: non_empty(body.get("overview").and_then(Value::as_str)),
        tagline: non_empty(body.get("tagline").and_then(Value::as_str)),
        genres,
        runtime_minutes: body
            .get("runtime")
            .and_then(Value::as_u64)
            .map(|v| v as u32)
            .filter(|v| *v > 0),
        rating: body.get("vote_average").and_then(Value::as_f64).map(|v| v as f32),
        vote_count: body
            .get("vote_count")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        popularity: body.get("popularity").and_then(Value::as_f64).map(|v| v as f32),
        artwork,
    })
}

/// TMDB uses `""` rather than `null` for "not provided".
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed but structurally faithful `search/movie` response.
    const SEARCH: &str = r#"{
      "page": 1,
      "results": [
        {
          "adult": false,
          "backdrop_path": "/fNG7i7RqMErkcqhohV2a6cV1Ehy.jpg",
          "genre_ids": [28, 878],
          "id": 603,
          "original_language": "en",
          "original_title": "The Matrix",
          "overview": "Set in the 22nd century, The Matrix tells the story of a computer hacker.",
          "popularity": 42.5678,
          "poster_path": "/f89U3ADr1oiB1s9GkdPOEpXUk5H.jpg",
          "release_date": "1999-03-30",
          "title": "The Matrix",
          "video": false,
          "vote_average": 8.219,
          "vote_count": 25417
        },
        {
          "adult": false,
          "backdrop_path": "/icmmSD4vTTDKOq2vvdulafOGw93.jpg",
          "genre_ids": [28, 12, 878],
          "id": 604,
          "original_title": "The Matrix Reloaded",
          "overview": "Six months after the events of the first film.",
          "popularity": 30.1234,
          "poster_path": "/9TGHDvWrqKBzwDxDodHYXEmOE6J.jpg",
          "release_date": "2003-05-15",
          "title": "The Matrix Reloaded",
          "video": false,
          "vote_average": 7.0,
          "vote_count": 11000
        }
      ],
      "total_pages": 1,
      "total_results": 2
    }"#;

    /// A trimmed but structurally faithful `movie/{id}` response.
    const MOVIE: &str = r#"{
      "adult": false,
      "backdrop_path": "/fNG7i7RqMErkcqhohV2a6cV1Ehy.jpg",
      "budget": 63000000,
      "genres": [{"id": 28, "name": "Action"}, {"id": 878, "name": "Science Fiction"}],
      "id": 603,
      "imdb_id": "tt0133093",
      "original_title": "The Matrix",
      "overview": "Set in the 22nd century, The Matrix tells the story of a computer hacker.",
      "popularity": 42.5678,
      "poster_path": "/f89U3ADr1oiB1s9GkdPOEpXUk5H.jpg",
      "release_date": "1999-03-30",
      "runtime": 136,
      "status": "Released",
      "tagline": "Welcome to the Real World.",
      "title": "The Matrix",
      "video": false,
      "vote_average": 8.219,
      "vote_count": 25417
    }"#;

    #[test]
    fn parses_a_search_response() {
        let body: Value = serde_json::from_str(SEARCH).unwrap();
        let candidates = parse_search(&body).unwrap();
        assert_eq!(candidates.len(), 2);

        let first = &candidates[0];
        assert_eq!(first.source, "tmdb");
        assert_eq!(first.source_id, "603");
        assert_eq!(first.title, "The Matrix");
        assert_eq!(first.year, Some(1999));
        assert_eq!(first.poster_path.as_deref(), Some("/f89U3ADr1oiB1s9GkdPOEpXUk5H.jpg"));
        assert!(first.vote_count.unwrap() > 25_000);
    }

    #[test]
    fn parses_movie_details() {
        let body: Value = serde_json::from_str(MOVIE).unwrap();
        let metadata = parse_movie(&body).unwrap();

        assert_eq!(metadata.display_title(), "The Matrix (1999)");
        assert_eq!(metadata.genres, vec!["Action", "Science Fiction"]);
        assert_eq!(metadata.runtime_label().as_deref(), Some("2h 16m"));
        assert_eq!(metadata.tagline.as_deref(), Some("Welcome to the Real World."));
        assert!(metadata.artwork.poster.is_some());
        assert!(metadata.artwork.backdrop.is_some());
    }

    #[test]
    fn tolerates_missing_and_empty_fields() {
        // TMDB sends "" for absent text and can omit runtime entirely.
        let body: Value = serde_json::from_str(
            r#"{"id": 1, "title": "Untitled", "overview": "", "release_date": "",
                "tagline": "", "poster_path": null, "genre_ids": []}"#,
        )
        .unwrap();

        let metadata = parse_movie(&body).unwrap();
        assert_eq!(metadata.year, None);
        assert_eq!(metadata.overview, None);
        assert_eq!(metadata.tagline, None);
        assert_eq!(metadata.runtime_minutes, None);
        assert!(metadata.artwork.is_empty());
    }

    #[test]
    fn a_response_without_results_is_an_error() {
        let body: Value = serde_json::from_str(r#"{"status_code":34,"status_message":"nope"}"#).unwrap();
        assert!(matches!(parse_search(&body), Err(CatalogError::Decode(_))));
    }

    #[test]
    fn malformed_entries_are_skipped_rather_than_failing_the_page() {
        let body: Value = serde_json::from_str(
            r#"{"results":[{"no_id":true},{"id":7,"title":"Fine","release_date":"2001-01-01"}]}"#,
        )
        .unwrap();
        let candidates = parse_search(&body).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].title, "Fine");
    }

    #[test]
    fn year_parsing_rejects_nonsense() {
        assert_eq!(parse_year(Some("1999-03-30")), Some(1999));
        assert_eq!(parse_year(Some("")), None);
        assert_eq!(parse_year(None), None);
        assert_eq!(parse_year(Some("not a date")), None);
        assert_eq!(parse_year(Some("0001-01-01")), None);
    }

    #[test]
    fn auth_style_is_detected_from_the_key() {
        assert_eq!(Auth::detect("abc123"), Auth::Query("abc123".into()));
        assert_eq!(
            Auth::detect("eyJhbGciOiJIUzI1NiJ9.payload.sig"),
            Auth::Bearer("eyJhbGciOiJIUzI1NiJ9.payload.sig".into())
        );
        assert!(!Auth::detect("   ").is_configured());
        assert_eq!(Auth::detect("  spaced  "), Auth::Query("spaced".into()));
    }

    #[test]
    fn an_empty_key_is_refused() {
        assert!(matches!(TmdbClient::new(""), Err(CatalogError::NotConfigured)));
        assert!(matches!(TmdbClient::new("  "), Err(CatalogError::NotConfigured)));
        assert!(TmdbClient::new("abc").is_ok());
    }

    #[test]
    fn artwork_urls_use_the_right_sizes() {
        let client = TmdbClient::new("test").unwrap();
        assert_eq!(
            client.poster_url("/abc.jpg", ArtworkSize::Card),
            "https://image.tmdb.org/t/p/w342/abc.jpg"
        );
        assert_eq!(
            client.poster_url("/abc.jpg", ArtworkSize::Hero),
            "https://image.tmdb.org/t/p/w780/abc.jpg"
        );
        assert_eq!(
            client.backdrop_url("abc.jpg", ArtworkSize::Hero),
            "https://image.tmdb.org/t/p/w1280/abc.jpg"
        );

        let custom = TmdbClient::new("test")
            .unwrap()
            .with_image_base("http://localhost:1/img/");
        assert_eq!(
            custom.poster_url("/x.jpg", ArtworkSize::Card),
            "http://localhost:1/img/w342/x.jpg"
        );
    }
}
