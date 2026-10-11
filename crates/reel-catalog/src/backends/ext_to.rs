//! A [`SearchBackend`] that scrapes ext.to.
//!
//! ext.to is a torrent index with no API: a search is an HTML page at
//! `/browse/?q=<query>`, one table row per torrent. The site aggregates other
//! indexes, so its rows are the useful part.
//!
//! # The Cloudflare caveat
//!
//! ext.to sits behind a Cloudflare *interactive* challenge: the first request
//! from an unfamiliar client gets a JavaScript interstitial, and neither a plain
//! HTTP client nor a headless browser can clear it.
//!
//! That is handled by [FlareSolverr](https://github.com/FlareSolverr/FlareSolverr),
//! a local server that drives a real Chrome to clear the challenge. reel does not
//! compile a browser into itself: the installer places FlareSolverr beside the
//! binary (`<prefix>/lib/reel/solver`), and [`super::solver`] starts it on demand
//! and reuses it. A fresh install therefore works with no Docker and no manual
//! setup.
//!
//! Two overrides remain, for anyone who wants them:
//!
//! * **A clearance cookie you already have** (`cf_clearance`, saved in
//!   Settings), with the matching user agent.
//! * **A solver URL** you run yourself, which is used instead of the bundled one.
//!
//! When a challenge cannot be cleared, [`parse_search`] reports it as a
//! [`SearchError`] with instructions rather than quietly returning nothing.
//!
//! # What is parsed
//!
//! Search results live in `table.search-table`, one `<tr>` per torrent. The
//! first cell holds the title link (with `<span>`-wrapped search terms) to the
//! torrent's page, and the value cells are `<span class="add-block">Label</span>`
//! followed by a value span, which is how size, age, seeds and leechs are read.
//!
//! # Searching
//!
//! The site paginates and matches the query anywhere in a name, so a broad
//! query like `friends` is mostly *other* shows and one page is far from all
//! the torrents a popular title has. A search therefore fetches a couple of
//! pages of the query, then finds the title the results agree on
//! ([`anchor_query`]) and fetches that title's own releases directly. See
//! [`ExtToBackend::search`].
//!
//! The magnet link is not in the listing. It is fetched on demand from the
//! torrent's page through a token-signed AJAX call
//! (`/ajax/getTorrentMagnet.php`): the page carries `window.pageToken` and
//! `window.csrfToken`, and the request signs
//! `SHA256("<torrent id>|<timestamp>|<page token>")`. [`ExtToBackend::magnet_for`]
//! does this when a row is added, so a row shows as "Add" even though the
//! listing itself had no magnet.

use std::collections::HashMap;
use std::sync::Mutex;

use scraper::{ElementRef, Html, Selector};
use serde_json::Value;

use crate::search::{BoxFuture, SearchBackend, SearchError, SearchHit, SearchQuery};

/// Where the site lives. Overridable for a mirror or a proxy.
pub const DEFAULT_BASE_URL: &str = "https://ext.to";

/// Name shown next to results.
pub const SOURCE_NAME: &str = "ext.to";

/// Rows ext.to returns per page. A shorter page is the last one.
const RESULTS_PER_PAGE: usize = 50;
/// Hits a title needs before it is treated as the anchor of a broad search.
const ANCHOR_MIN_HITS: usize = 3;
/// Searches whose paging state is remembered. Old ones are dropped rather than
/// kept forever.
const MAX_TRACKED_SEARCHES: usize = 8;

/// A current desktop browser user agent.
///
/// Cloudflare's clearance cookie is bound to the user agent that solved the
/// challenge, so this must match the browser the operator copied the cookie
/// from. `EXTTO_USER_AGENT` overrides it.
pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

/// A browser session that has cleared the Cloudflare challenge: the cookie
/// header to replay, and the user agent it is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Clearance {
    cookie: String,
    user_agent: String,
}

pub struct ExtToBackend {
    http: reqwest::Client,
    base_url: String,
    user_agent: String,
    /// A browser `Cookie` header, needed to pass the Cloudflare challenge.
    cookie: Option<String>,
    /// Base URL of a FlareSolverr-compatible solver, if one is configured.
    solver_url: Option<String>,
    /// A clearance obtained from the solver, cached for later requests. Interior
    /// mutability because `search` takes `&self`.
    clearance: Mutex<Option<Clearance>>,
    /// Paging state for searches in progress, keyed by query text, so
    /// [`ExtToBackend::search_more`] can stream further pages.
    searches: Mutex<HashMap<String, SearchProgress>>,
    /// Fall back to the solver the installer placed beside the binary when no
    /// explicit `solver_url` is set. Tests turn this off so they stay offline.
    bundled_solver: bool,
    /// Whether the backend is usable at all (the trait's disabled path).
    enabled: bool,
}

/// Where a search has got to, so the next page can be streamed.
#[derive(Debug, Clone)]
struct SearchProgress {
    /// The title the first page's results agreed on, if any. Its pages are the
    /// ones streamed; otherwise the raw query's pages are.
    anchor: Option<String>,
    /// The next page to fetch of whichever track is active.
    next_page: usize,
}

impl Default for ExtToBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtToBackend {
    /// Build a backend, taking its cookie and solver URL from the environment.
    ///
    /// Until `EXTTO_COOKIE` or `EXTTO_SOLVER_URL` is set the backend reports
    /// itself as not configured, so the UI lists it as needing setup and skips
    /// it in searches rather than failing on every query with a challenge. A
    /// direct `search` still runs and reports the challenge, for callers and
    /// tests that want that.
    pub fn new() -> Self {
        let cookie = std::env::var("EXTTO_COOKIE")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let user_agent = std::env::var("EXTTO_USER_AGENT")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string());
        let solver_url = std::env::var("EXTTO_SOLVER_URL")
            .ok()
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty());

        Self {
            http: reqwest::Client::builder()
                .user_agent(&user_agent)
                .timeout(std::time::Duration::from_secs(25))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "could not build the ext.to HTTP client");
                    reqwest::Client::new()
                }),
            base_url: DEFAULT_BASE_URL.to_string(),
            user_agent,
            cookie,
            solver_url,
            clearance: Mutex::new(None),
            searches: Mutex::new(HashMap::new()),
            bundled_solver: true,
            enabled: true,
        }
    }

    /// Build from the saved settings. The settings file is canonical, so this
    /// reads nothing from the environment.
    pub fn from_settings(settings: &crate::settings::CatalogSettings) -> Self {
        let user_agent = settings
            .ext_to_user_agent
            .clone()
            .filter(|ua| !ua.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string());
        let http = reqwest::Client::builder()
            .user_agent(&user_agent)
            .timeout(std::time::Duration::from_secs(25))
            .build()
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "could not build the ext.to HTTP client");
                reqwest::Client::new()
            });
        Self {
            http,
            base_url: settings
                .ext_to_base_url
                .clone()
                .filter(|url| !url.trim().is_empty())
                .map(|url| url.trim_end_matches('/').to_string())
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            user_agent,
            cookie: settings.ext_to_cookie.clone(),
            solver_url: settings.ext_to_solver_url.clone(),
            clearance: Mutex::new(None),
            searches: Mutex::new(HashMap::new()),
            bundled_solver: true,
            enabled: true,
        }
    }

    pub fn with_cookie(mut self, cookie: impl Into<String>) -> Self {
        self.cookie = Some(cookie.into());
        self
    }

    pub fn with_user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = user_agent.into();
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    /// Point at a FlareSolverr-compatible solver, so a challenge can be cleared
    /// without a manually copied cookie.
    pub fn with_solver_url(mut self, solver_url: impl Into<String>) -> Self {
        self.solver_url = Some(solver_url.into().trim_end_matches('/').to_string());
        self
    }

    /// Switch the backend off, so the UI can show it as configured-but-disabled.
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// Do not fall back to the solver installed beside the binary. Used by
    /// tests, and available to anyone who would rather rely on a cookie or an
    /// explicitly configured solver URL.
    pub fn without_bundled_solver(mut self) -> Self {
        self.bundled_solver = false;
        self
    }

    /// The clearance obtained from the solver so far, if any.
    fn clearance(&self) -> Option<Clearance> {
        self.clearance
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_clearance(&self, clearance: Clearance) {
        *self.clearance.lock().unwrap_or_else(|e| e.into_inner()) = Some(clearance);
    }

    /// One GET with a given session, returning the status and body.
    async fn fetch(
        &self,
        url: &str,
        cookie: Option<&str>,
        user_agent: &str,
    ) -> Result<(reqwest::StatusCode, String), SearchError> {
        let mut request = self
            .http
            .get(url)
            .header(
                reqwest::header::ACCEPT,
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .header(reqwest::header::USER_AGENT, user_agent)
            .header(reqwest::header::REFERER, &self.base_url);
        if let Some(cookie) = cookie.filter(|cookie| !cookie.is_empty()) {
            request = request.header(reqwest::header::COOKIE, cookie);
        }

        let response = request.send().await.map_err(parse_error)?;
        let status = response.status();
        let body = response.text().await.map_err(parse_error)?;
        Ok((status, body))
    }

    /// Fetch a page, clearing a Cloudflare challenge if one comes back.
    ///
    /// Shared by search and magnet resolution: both need the same clearance,
    /// and a solver-obtained cookie is cached for later requests.
    async fn fetch_authed(&self, url: &str) -> Result<String, SearchError> {
        let clearance = self.clearance();
        let (cookie, user_agent) = match &clearance {
            Some(clearance) => (Some(clearance.cookie.as_str()), clearance.user_agent.as_str()),
            None => (self.cookie.as_deref(), self.user_agent.as_str()),
        };

        let (status, body) = self.fetch(url, cookie, user_agent).await?;

        if is_challenge(&body) {
            // A FlareSolverr-compatible solver clears the challenge: the one the
            // user configured, or the one the installer placed beside the
            // binary, which is started on demand and then reused.
            let solver = match self.solver_url.clone() {
                Some(url) => Some(url),
                None if self.bundled_solver => {
                    crate::backends::solver::ensure_running().await
                }
                None => None,
            };
            let Some(solver) = solver else {
                return Err(backend_error(challenge_message(self)));
            };
            let (html, clearance) = self.solve(&solver, url).await?;
            self.set_clearance(clearance);
            if is_challenge(&html) {
                return Err(backend_error(
                    "the solver returned a Cloudflare challenge page; it could not clear the \
                     site",
                ));
            }
            return Ok(html);
        }

        if !status.is_success() {
            return Err(backend_error(format!(
                "HTTP {status}: {}",
                body.chars().take(200).collect::<String>()
            )));
        }

        Ok(body)
    }

    /// The cookie + user agent to send with a request: a solver-obtained
    /// clearance if we have one, else the configured cookie.
    fn session(&self) -> (Option<String>, String) {
        match self.clearance() {
            Some(clearance) => (Some(clearance.cookie), clearance.user_agent),
            None => (self.cookie.clone(), self.user_agent.clone()),
        }
    }

    /// Ask a FlareSolverr-compatible solver to fetch `target` in a real browser,
    /// returning the page and the clearance it used.
    async fn solve(&self, solver: &str, target: &str) -> Result<(String, Clearance), SearchError> {
        let endpoint = format!("{}/v1", solver.trim_end_matches('/'));
        let payload =
            serde_json::json!({ "cmd": "request.get", "url": target, "maxTimeout": 60_000 });
        let response = self
            .http
            .post(&endpoint)
            .json(&payload)
            .send()
            .await
            .map_err(parse_error)?;
        let status = response.status();
        let body = response.text().await.map_err(parse_error)?;

        if !status.is_success() {
            return Err(backend_error(format!(
                "solver HTTP {status}: {}",
                body.chars().take(200).collect::<String>()
            )));
        }

        let value: Value = serde_json::from_str(&body).map_err(|e| {
            backend_error(format!(
                "solver returned invalid JSON: {e}: {}",
                body.chars().take(200).collect::<String>()
            ))
        })?;
        solution_from_value(&value, &self.user_agent)
    }

    /// The search page for a query, e.g. `https://ext.to/browse/?q=The+Matrix+1999`.
    pub fn search_url(&self, query: &SearchQuery) -> String {
        let text = build_query_text(query);
        let base = format!("{}/browse/", self.base_url);
        if text.is_empty() {
            return base;
        }
        reqwest::Url::parse_with_params(&base, [("q", text.as_str())])
            .map(|url| url.to_string())
            .unwrap_or(base)
    }

    /// The search page for a raw text query and page number.
    fn search_url_page(&self, text: &str, page: usize) -> String {
        let base = format!("{}/browse/", self.base_url);
        let mut params: Vec<(&str, String)> = Vec::new();
        let text = text.trim();
        if !text.is_empty() {
            params.push(("q", text.to_string()));
        }
        if page > 1 {
            params.push(("page", page.to_string()));
        }
        reqwest::Url::parse_with_params(&base, &params)
            .map(|url| url.to_string())
            .unwrap_or(base)
    }

    /// Fetch and parse one page of a raw text query.
    async fn search_page(&self, text: &str, page: usize) -> Result<Vec<SearchHit>, SearchError> {
        let url = self.search_url_page(text, page);
        let html = self.fetch_authed(&url).await?;
        parse_search(&html, self)
    }

    /// Remember where a search got to, so later pages can be streamed.
    fn remember_search(&self, text: &str, anchor: Option<String>) {
        let mut searches = self.searches.lock().unwrap_or_else(|e| e.into_inner());
        if searches.len() >= MAX_TRACKED_SEARCHES {
            searches.clear();
        }
        searches.insert(
            text.to_string(),
            SearchProgress {
                anchor,
                next_page: 2,
            },
        );
    }

    fn search_progress(&self, text: &str) -> Option<SearchProgress> {
        self.searches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(text)
            .cloned()
    }

    fn advance_search(&self, text: &str, next_page: usize) {
        if let Some(progress) = self
            .searches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(text)
        {
            progress.next_page = next_page;
        }
    }

    fn forget_search(&self, text: &str) {
        self.searches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(text);
    }

    /// The page a person would open in a browser, from a row's relative href.
    pub fn detail_url(&self, href: &str) -> String {
        if href.starts_with("http://") || href.starts_with("https://") {
            href.to_string()
        } else {
            format!("{}/{}", self.base_url, href.trim_start_matches('/'))
        }
    }

    /// Resolve the magnet for a hit whose listing row had none.
    ///
    /// The site hands the magnet out through a token-signed AJAX call. The
    /// token (`window.pageToken`) and session id (`window.csrfToken`) live on
    /// the torrent's page, so one page is fetched, the two values are read out,
    /// and `SHA256("<id>|<timestamp>|<page token>")` is sent back with them.
    /// This is the whole reason a row is not directly addable.
    async fn resolve_magnet(&self, hit: &SearchHit) -> Result<Option<String>, SearchError> {
        if let Some(magnet) = hit.magnet.as_deref().filter(|m| m.starts_with("magnet:")) {
            return Ok(Some(magnet.to_string()));
        }
        let Some(detail_url) = hit.detail_url.as_deref() else {
            return Ok(None);
        };

        let html = self.fetch_authed(detail_url).await?;
        let page_token = inline_js_string(&html, "window.pageToken").ok_or_else(|| {
            backend_error("the torrent page had no pageToken; the markup may have changed")
        })?;
        let sessid = inline_js_string(&html, "window.csrfToken").unwrap_or_default();
        let torrent_id = torrent_id_from_page(&html, detail_url)
            .ok_or_else(|| backend_error("the torrent page had no torrent id"))?;

        let timestamp = unix_now();
        let hmac = magnet_token(torrent_id, timestamp, &page_token);

        let (cookie, user_agent) = self.session();
        // Every value is URL-safe (digits, "magnet", hex), so the body is built
        // directly rather than pulling in a form encoder.
        let form = format!(
            "torrent_id={torrent_id}&download_type=magnet&timestamp={timestamp}&hmac={hmac}&sessid={sessid}"
        );
        let mut request = self
            .http
            .post(format!("{}/ajax/getTorrentMagnet.php", self.base_url))
            .header(reqwest::header::USER_AGENT, user_agent)
            .header(reqwest::header::REFERER, detail_url)
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("X-Requested-With", "XMLHttpRequest")
            .body(form);
        if let Some(cookie) = cookie.filter(|cookie| !cookie.is_empty()) {
            request = request.header(reqwest::header::COOKIE, cookie);
        }

        let response = request.send().await.map_err(parse_error)?;
        let status = response.status();
        let body = response.text().await.map_err(parse_error)?;
        if is_challenge(&body) {
            return Err(backend_error(challenge_message(self)));
        }
        if !status.is_success() {
            return Err(backend_error(format!(
                "magnet request answered HTTP {status}: {}",
                body.chars().take(200).collect::<String>()
            )));
        }

        magnet_from_response(&body)
    }
}

/// The magnet out of a `getTorrentMagnet.php` response.
///
/// The shape is `{"success":true,"url":"magnet:?xt=…","type":"magnet",…}`; a
/// hash-only fallback is accepted, and anything else is reported with the
/// site's own error message.
fn magnet_from_response(body: &str) -> Result<Option<String>, SearchError> {
    let value: Value = serde_json::from_str(body).map_err(|e| {
        backend_error(format!(
            "magnet response was not JSON: {e}: {}",
            body.chars().take(200).collect::<String>()
        ))
    })?;

    if value.get("success").and_then(Value::as_bool) != Some(true) {
        let message = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("no message");
        return Err(backend_error(format!("magnet request failed: {message}")));
    }

    if let Some(url) = value
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| url.starts_with("magnet:"))
    {
        return Ok(Some(url.to_string()));
    }
    if let Some(hash) = value
        .get("hash")
        .and_then(Value::as_str)
        .filter(|hash| !hash.is_empty())
    {
        return Ok(Some(format!("magnet:?xt=urn:btih:{hash}")));
    }
    Ok(None)
}

/// `SHA256("<torrent id>|<timestamp>|<page token>")`, hex.
///
/// The site calls this its "hmac"; it is a plain salted SHA-256 over the three
/// fields it also sends, which is all that is needed to reproduce it.
fn magnet_token(torrent_id: i64, timestamp: i64, page_token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(format!("{torrent_id}|{timestamp}|{page_token}").as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Read `window.<name> = '<value>';` out of an inline script.
fn inline_js_string(html: &str, marker: &str) -> Option<String> {
    let rest = &html[html.find(marker)? + marker.len()..];
    let after = rest[rest.find('=')? + 1..].trim_start();
    let quote = after.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let body = &after[1..];
    let end = body.find(quote)?;
    let value = body[..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The torrent id for the magnet call: the trailing number of the page URL,
/// falling back to the `data-id` on the page's magnet button.
fn torrent_id_from_page(html: &str, detail_url: &str) -> Option<i64> {
    if let Some(id) = detail_url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .and_then(|segment| segment.rsplit('-').next())
        .and_then(|id| id.parse::<i64>().ok())
    {
        return Some(id);
    }

    let selector = Selector::parse("a.detail-magnet[data-id]").expect("static selector");
    let document = Html::parse_document(html);
    document
        .select(&selector)
        .next()
        .and_then(|node| node.value().attr("data-id"))
        .and_then(|id| id.parse::<i64>().ok())
}

/// The free text to search for: the title, plus episode and year markers the way
/// a person types them into an index site.
pub fn build_query_text(query: &SearchQuery) -> String {
    let mut parts = vec![query.text.trim().to_string()];
    if let (Some(season), Some(episode)) = (query.season, query.episode) {
        parts.push(format!("s{season:02}e{episode:02}"));
    }
    if let Some(year) = query.year {
        parts.push(year.to_string());
    }
    parts.retain(|part| !part.is_empty());
    parts.join(" ")
}

/// True when the body is Cloudflare's challenge interstitial rather than a page.
fn is_challenge(body: &str) -> bool {
    body.contains("Just a moment") || body.contains("cf_chl_")
}

/// The title a broad search's results agree on, as a query to fetch that
/// title's own releases (`Friends 1994` for `friends`).
///
/// `None` when the query already names one title (nothing to add) or nothing
/// dominates. This is what turns "mostly unrelated shows" into "the show's
/// torrents": ext.to matches the query anywhere in a name, so `friends` also
/// returns *Smiling Friends* and *Your Friends and Neighbors*.
fn anchor_query(query: &str, hits: &[SearchHit]) -> Option<String> {
    let query_norm = normalize_title(query);
    if query_norm.is_empty() {
        return None;
    }

    // Group the parsed titles and count them.
    let mut groups: Vec<(String, Option<u16>, usize)> = Vec::new();
    for hit in hits {
        let release = crate::release::analyse_name(&hit.title);
        let title = release.title.trim();
        if title.is_empty() {
            continue;
        }
        let year = release.year.or(hit.year);
        match groups
            .iter_mut()
            .find(|(existing, _, _)| existing.eq_ignore_ascii_case(title))
        {
            Some((_, existing_year, count)) => {
                *count += 1;
                if existing_year.is_none() {
                    *existing_year = year;
                }
            }
            None => groups.push((title.to_string(), year, 1)),
        }
    }

    groups
        .into_iter()
        .filter(|(_, _, count)| *count >= ANCHOR_MIN_HITS)
        .filter_map(|(title, year, count)| {
            let score = title_match_score(&normalize_title(&title), &query_norm);
            (score > 0).then_some((score, count, title, year))
        })
        .max_by_key(|(score, count, _, _)| (*score, *count))
        .and_then(|(_, _, title, year)| {
            let anchor = match year {
                Some(year) => format!("{title} {year}"),
                None => title,
            };
            // No point re-searching for what we already searched.
            (normalize_title(&anchor) != query_norm).then_some(anchor)
        })
}

/// Lowercase, punctuation to spaces, single-spaced.
fn normalize_title(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 3 when the title is the query, 2 when it contains it, 1 when it has every
/// query word, else 0.
fn title_match_score(title: &str, query: &str) -> u8 {
    if title == query {
        3
    } else if title.contains(query) {
        2
    } else {
        let words: std::collections::HashSet<&str> = title.split_whitespace().collect();
        if query.split_whitespace().all(|word| words.contains(word)) {
            1
        } else {
            0
        }
    }
}

/// Drop the repeats a refined search brings in, keyed by the torrent's page.
fn dedupe_hits(hits: &mut Vec<SearchHit>) {
    let mut seen = std::collections::HashSet::new();
    hits.retain(|hit| seen.insert(hit.key()));
}

/// Parse a search results page.
///
/// Returns an error when the page is the Cloudflare challenge, and an empty list
/// only when the site explicitly reports zero results.
pub fn parse_search(html: &str, backend: &ExtToBackend) -> Result<Vec<SearchHit>, SearchError> {
    if is_challenge(html) {
        return Err(SearchError::Backend {
            backend: SOURCE_NAME.to_string(),
            message: challenge_message(backend),
        });
    }

    let now = unix_now();

    // A no-results page keeps its shell but reports "(0 results)" in the title.
    if html.contains("(0 results)") {
        return Ok(Vec::new());
    }

    let table = Selector::parse("table.search-table").expect("static selector");
    let document = Html::parse_document(html);
    if document.select(&table).next().is_none() {
        return Err(SearchError::Backend {
            backend: SOURCE_NAME.to_string(),
            message: "the response had no results table; the markup may have changed".to_string(),
        });
    }

    let row_selector = Selector::parse("tbody tr").expect("static selector");
    let hits = document
        .select(&table)
        .next()
        .into_iter()
        .flat_map(|table| table.select(&row_selector))
        .filter_map(|row| parse_row(row, backend, now))
        .collect();

    Ok(hits)
}

fn parse_row(row: ElementRef<'_>, backend: &ExtToBackend, now: i64) -> Option<SearchHit> {
    let anchor = title_anchor(row)?;
    let title = normalize(&anchor.text().collect::<String>());
    if title.is_empty() {
        return None;
    }

    // Listings usually carry no magnet (see the module docs); one is used when
    // the row has it, and the page link is the fallback.
    let magnet_selector = Selector::parse("a[href^=\"magnet:\"]").expect("static selector");
    let magnet = row
        .select(&magnet_selector)
        .next()
        .and_then(|link| link.value().attr("href"))
        .map(str::to_string);

    let mut hit = SearchHit::new(title.clone(), SOURCE_NAME);
    hit.magnet = magnet;
    hit.size_bytes = cell_value(row, "Size").and_then(|value| parse_size(&value));
    hit.seeders = cell_value(row, "Seeds").and_then(|value| value.trim().parse().ok());
    hit.leechers = cell_value(row, "Leechs").and_then(|value| value.trim().parse().ok());
    hit.year = crate::release::parse_one(&title).1;

    let age = cell_value(row, "Age");
    hit.published_at = age.as_deref().and_then(|value| parse_age(value, now));

    // What the site shows that the shared fields do not cover: the category the
    // release was posted in, and how old it is.
    // The uploader is linked too (`external-user`), so skip that link.
    let category_selector =
        Selector::parse("div.related-posted a:not(.external-user) strong").expect("static selector");
    let category = row
        .select(&category_selector)
        .next()
        .map(|node| normalize(&node.text().collect::<String>()))
        .filter(|value| !value.is_empty());

    let mut parts: Vec<String> = vec![SOURCE_NAME.to_string()];
    if let Some(category) = category {
        parts.push(category);
    }
    if let Some(age) = age {
        parts.push(age);
    }
    hit.detail = Some(parts.join(" \u{2022} "));
    hit.detail_url = anchor.value().attr("href").map(|href| backend.detail_url(href));

    Some(hit)
}

/// The anchor holding the release name: the first link in the name cell that
/// wraps its text in `<b>`. The category link lives further down in the same
/// cell, so the bold filter separates them; if the markup ever stops bolding the
/// title, the first link is still the right one.
fn title_anchor(row: ElementRef<'_>) -> Option<ElementRef<'_>> {
    let selector = Selector::parse("td.text-left a[href]").expect("static selector");
    let bold = Selector::parse("b").expect("static selector");
    let mut first = None;
    for anchor in row.select(&selector) {
        if first.is_none() {
            first = Some(anchor);
        }
        if anchor.select(&bold).next().is_some() {
            return Some(anchor);
        }
    }
    first
}

/// Read a value cell by its label, e.g. `Seeds547` -> `547`.
///
/// Only cells carrying a `span.add-block` marker with exactly this label are
/// consulted, so a release whose title happens to start with a label word cannot
/// be mistaken for a value. The value is the cell text with the label removed.
fn cell_value(row: ElementRef<'_>, label: &str) -> Option<String> {
    let cell_selector = Selector::parse("td").expect("static selector");
    let block_selector = Selector::parse("span.add-block").expect("static selector");
    for cell in row.select(&cell_selector) {
        let Some(marker) = cell.select(&block_selector).next() else {
            continue;
        };
        if normalize(&marker.text().collect::<String>()) != label {
            continue;
        }
        let text = normalize(&cell.text().collect::<String>());
        let value = text.strip_prefix(label)?.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// Collapse runs of whitespace, so text split across elements reads as one line.
fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `676.9 MB` and `1.42 GB` to bytes. Binary units, matching what these tables
/// mean in practice; the exact base does not change how results order.
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text.find(|c: char| c.is_ascii_alphabetic())?;
    let (number, unit) = text.split_at(split);
    let number: f64 = number.trim().replace(',', "").parse().ok()?;
    if number < 0.0 {
        return None;
    }

    let factor: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "b" | "bytes" => 1.0,
        "kb" | "kib" => 1024.0,
        "mb" | "mib" => 1024.0 * 1024.0,
        "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * factor) as u64)
}

/// `5 years ago` to a Unix timestamp, using coarse month/year lengths since the
/// site only gives one unit. Returns `None` for anything unrecognised.
pub fn parse_age(text: &str, now: i64) -> Option<i64> {
    let text = text.trim().trim_end_matches("ago").trim();
    let split = text.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let (number, unit) = text.split_at(split);
    let number: f64 = number.trim().parse().ok()?;
    if number < 0.0 {
        return None;
    }

    let seconds = match unit.trim().to_ascii_lowercase().trim_end_matches('s') {
        "second" | "sec" => 1.0,
        "minute" | "min" => 60.0,
        "hour" => 3600.0,
        "day" => 86_400.0,
        "week" => 7.0 * 86_400.0,
        "month" => 30.44 * 86_400.0,
        "year" => 365.25 * 86_400.0,
        _ => return None,
    };
    Some(now - (number * seconds) as i64)
}

fn backend_error(message: impl Into<String>) -> SearchError {
    SearchError::Backend {
        backend: SOURCE_NAME.to_string(),
        message: message.into(),
    }
}

/// Read a FlareSolverr `request.get` response into the page and its clearance.
///
/// The shape is `{"status":"ok","solution":{"response":...,"cookies":[...],
/// "userAgent":...}}`; a non-`ok` status is reported with the solver's message.
fn solution_from_value(
    value: &Value,
    fallback_user_agent: &str,
) -> Result<(String, Clearance), SearchError> {
    let status = value.get("status").and_then(Value::as_str).unwrap_or("");
    if status != "ok" {
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("no message");
        return Err(backend_error(format!("solver reported `{status}`: {message}")));
    }

    let solution = value
        .get("solution")
        .ok_or_else(|| backend_error("solver response had no solution"))?;
    let html = solution
        .get("response")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if html.is_empty() {
        return Err(backend_error("solver returned an empty response"));
    }

    let user_agent = solution
        .get("userAgent")
        .and_then(Value::as_str)
        .filter(|ua| !ua.trim().is_empty())
        .unwrap_or(fallback_user_agent)
        .to_string();

    Ok((
        html,
        Clearance {
            cookie: cookies_from_solution(solution),
            user_agent,
        },
    ))
}

/// Join the solver's cookie list into a `Cookie` header value.
fn cookies_from_solution(solution: &Value) -> String {
    solution
        .get("cookies")
        .and_then(Value::as_array)
        .map(|cookies| {
            cookies
                .iter()
                .filter_map(|cookie| {
                    let name = cookie.get("name")?.as_str()?;
                    let value = cookie.get("value")?.as_str()?;
                    Some(format!("{name}={value}"))
                })
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default()
}

fn challenge_message(backend: &ExtToBackend) -> String {
    format!(
        "ext.to answered with a Cloudflare challenge instead of results: {}",
        challenge_hint(backend)
    )
}

/// What to suggest when a challenge could not be cleared.
fn challenge_hint(backend: &ExtToBackend) -> String {
    if backend.solver_url.is_some() {
        return "the configured solver could not clear it either".to_string();
    }
    if backend.cookie.is_some() || backend.clearance().is_some() {
        return "the stored clearance is stale — clear the ext.to cookie in Settings so the \
                solver can renew it"
            .to_string();
    }
    "the solver could not clear it (Cloudflare adapts; retrying later often works)".to_string()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn parse_error(error: reqwest::Error) -> SearchError {
    SearchError::Backend {
        backend: SOURCE_NAME.to_string(),
        message: error.to_string(),
    }
}

impl SearchBackend for ExtToBackend {
    fn name(&self) -> &str {
        SOURCE_NAME
    }

    fn is_configured(&self) -> bool {
        // A saved cookie or an explicit solver URL is enough; otherwise ext.to
        // is usable only when the installer's solver is present beside the
        // binary.
        self.enabled
            && (self.cookie.is_some()
                || self.solver_url.is_some()
                || (self.bundled_solver && crate::backends::solver::bundled_available()))
    }

    fn search<'a>(
        &'a self,
        query: &'a SearchQuery,
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, SearchError>> {
        Box::pin(async move {
            let text = build_query_text(query);

            // Page one first: it is the request that may have to clear a
            // Cloudflare challenge, and it is enough to find the anchor.
            let mut hits = self.search_page(&text, 1).await?;
            let anchor = anchor_query(&text, &hits);

            // When the results agree on one title, its own releases are the
            // useful ones; start that track too. Otherwise the raw query's next
            // page is streamed instead.
            if let Some(anchor) = &anchor {
                if let Ok(mut page) = self.search_page(anchor, 1).await {
                    hits.append(&mut page);
                }
            }

            self.remember_search(&text, anchor);
            dedupe_hits(&mut hits);
            tracing::debug!(query = %query.describe(), results = hits.len(), "ext.to search");
            Ok(hits)
        })
    }

    fn search_more<'a>(
        &'a self,
        query: &'a SearchQuery,
        round: usize,
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, SearchError>> {
        Box::pin(async move {
            let text = build_query_text(query);
            let Some(progress) = self.search_progress(&text) else {
                return Ok(Vec::new());
            };
            // Stream the anchor's pages when there is one, else the raw query's.
            let track = progress.anchor.clone().unwrap_or_else(|| text.clone());
            let page = progress.next_page;

            let hits = match self.search_page(&track, page).await {
                Ok(hits) => hits,
                Err(_) => {
                    // A later page failing just ends the stream.
                    self.forget_search(&text);
                    return Ok(Vec::new());
                }
            };

            if hits.len() < RESULTS_PER_PAGE {
                // The last page: nothing more to stream.
                self.forget_search(&text);
            } else {
                self.advance_search(&text, page + 1);
            }
            tracing::debug!(
                query = %query.describe(),
                round,
                page,
                results = hits.len(),
                "ext.to search page"
            );
            Ok(hits)
        })
    }

    fn warm<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if !self.enabled {
                return;
            }
            // The listing page is the one whose clearance the search reuses; the
            // home page's does not cover it. The clearance is cached, so the
            // search that follows is not the one that waits.
            let url = format!("{}/browse/", self.base_url);
            match self.fetch_authed(&url).await {
                Ok(_) => tracing::debug!("ext.to is warm"),
                Err(error) => tracing::debug!(%error, "ext.to warm-up failed"),
            }
        })
    }

    fn magnet_for<'a>(
        &'a self,
        hit: &'a SearchHit,
    ) -> BoxFuture<'a, Result<Option<String>, SearchError>> {
        Box::pin(self.resolve_magnet(hit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed but structurally faithful search page, taken from a real
    /// response. It keeps the header row, the `<span>`-split title, the value
    /// cells, and one row with no magnet link.
    const RESPONSE: &str = r#"
        <html><head><title>Download 100 s02e01 Torrent (281 results) - EXT Torrents</title></head>
        <body>
        <table class="table table-striped table-hover search-table">
            <thead>
                <tr>
                    <th class="text-left th-tname">TORRENT NAME</th>
                    <th><a class="sort-link" href="/search/100-s02e01/?order=size&amp;sort=desc">SIZE</a></th>
                    <th><a class="sort-link" href="/search/100-s02e01/?order=seed&amp;sort=desc">SEED</a></th>
                </tr>
            </thead>
            <tbody>
                <tr>
                    <td class="text-left">
                        <div class="float-left">
                            <i class="material-icons md-18 movie"></i>
                            <a href="/the-100-s06e01e02-web-x264-1xbet-eztv-222829/"><b>The <span>100 S06E01E02</span> WEB X264-1XBET [eztv]</b></a>
                            <div class="related-posted">
                                Posted in <a href="/tv/"><strong>TV</strong></a>
                            </div>
                        </div>
                        <div class="btn-blocks float-right">
                            <a class="dwn-btn torrent-dwn" data-id="222829" target="_blank" rel="nofollow" href="magnet:?xt=urn:btih:771B77B1AB402443CC9CE349888EEAA14D60683D"><i class="material-icons file_download"></i></a>
                        </div>
                        <div class="clearfix"></div>
                        <div class="mobile-info-block">
                            <div class="mobile-info"><i class="material-icons">file_upload</i><span class="text-success">547</span></div>
                            <div class="mobile-info"><i class="material-icons">storage</i><span>676.9 MB</span></div>
                            <div class="mobile-info"><i class="material-icons">access_time</i><span>5 years ago</span></div>
                        </div>
                    </td>
                    <td class="nowrap-td hide-on-mob"><div class="add-block-wrapper"><span class="add-block">Size</span><span>676.9 MB</span></div></td>
                    <td class="hide-on-mob"><div class="add-block-wrapper"><span class="add-block">Files</span><span>1</span></div></td>
                    <td class="nowrap-td hide-on-mob"><div class="add-block-wrapper"><span class="add-block">Age</span><span>5 years ago</span></div></td>
                    <td class="hide-on-mob"><div class="add-block-wrapper"><span class="add-block">Seeds</span><span class="text-success">547</span></div></td>
                    <td class="hide-on-mob"><div class="add-block-wrapper"><span class="add-block">Leechs</span><span class="text-danger">117</span></div></td>
                    <td class="hide-on-mob"><div class="add-block-wrapper"><span class="add-block">Source</span><a href="/the-100-s06e01e02-web-x264-1xbet-eztv-222829/#technical" class="source-link-tor"><img src="/static/img/source/eztv.png" alt="" /></a></div></td>
                </tr>
                <tr>
                    <td class="text-left">
                        <div class="float-left">
                            <i class="material-icons md-18 movie"></i>
                            <a href="/a-film-2021-1080p-bluray-x264-group-1000/"><b>A Film 2021 1080p BluRay x264-GROUP</b></a>
                            <div class="related-posted">Posted in <a href="/movies/"><strong>Movies</strong></a></div>
                        </div>
                        <div class="btn-blocks float-right">
                            <a class="dwn-btn torrent-dwn" data-id="1000" href="magnet:?xt=urn:btih:AAAAAAAABBBBCCCCDDDDEEEEFFFF000011112222"><i class="material-icons file_download"></i></a>
                        </div>
                    </td>
                    <td class="nowrap-td hide-on-mob"><div class="add-block-wrapper"><span class="add-block">Size</span><span>1.42 GB</span></div></td>
                    <td class="hide-on-mob"><div class="add-block-wrapper"><span class="add-block">Age</span><span>3 weeks ago</span></div></td>
                    <td class="hide-on-mob"><div class="add-block-wrapper"><span class="add-block">Seeds</span><span>12</span></div></td>
                    <td class="hide-on-mob"><div class="add-block-wrapper"><span class="add-block">Leechs</span><span class="text-danger">3</span></div></td>
                </tr>
                <tr>
                    <td class="text-left">
                        <div class="float-left">
                            <a href="/dead-torrent-999/"><b>Dead torrent with no magnet</b></a>
                            <div class="related-posted">Posted in <a href="/other/"><strong>Other</strong></a></div>
                        </div>
                    </td>
                    <td class="nowrap-td"><div class="add-block-wrapper"><span class="add-block">Size</span><span>10 MB</span></div></td>
                </tr>
            </tbody>
        </table>
        </body></html>
    "#;

    fn backend() -> ExtToBackend {
        // Tests never start the solver: they exercise the parser and the
        // cookie/solver paths deterministically.
        ExtToBackend::new()
            .with_base_url("https://ext.to")
            .without_bundled_solver()
    }

    /// A row from the current layout: the title links to the torrent's page,
    /// there is no magnet in the listing, and the uploader is linked too.
    const CURRENT_RESPONSE: &str = r#"
        <html><head><title>Download sintel Torrent (5 results) - EXT Torrents</title></head>
        <body>
        <table class="table table-striped table-hover search-table">
            <thead><tr><th class="text-left th-tname">TORRENT NAME</th><th>SIZE</th></tr></thead>
            <tbody>
                <tr>
                    <td class="text-left">
                        <div class="float-left has-movie">
                            <i class="material-icons md-18 local_movies"></i>
                            <a href="/sintel-2010-720p-bluray-x264-2-0-yts-yify-15037012/" class="torrent-title-link"><b><span>Sintel</span> (2010) 720p BluRay x264 2.0 YTS YIFY</b></a><a href="/browse/?sort=age&amp;order=desc&amp;imdb_id=tt1727587" title="Search by IMDb ID"><span class="imdbSearchButton">IMDb</span></a>
                            <div class="related-posted">
                                Posted by <a class="external-user simple-user" href="?source%5B%5D=3"><strong>YTS</strong></a>
                                in <a href="/movies/"><strong>Movies</strong></a> - <a href="/movies/animation/"><strong>Animation</strong></a>
                            </div>
                        </div>
                        <div class="btn-blocks float-right">
                            <a class="dwn-btn search-magnet-btn" href="javascript:void(0);" data-id="15037012"><i class="material-icons file_download"></i></a>
                        </div>
                    </td>
                    <td class="nowrap-td hide-on-mob"><div class="add-block-wrapper"><span class="add-block">Size</span><span>136.52 MB</span></div></td>
                    <td class="hide-on-mob"><div class="add-block-wrapper"><span class="add-block">Files</span><span>4</span></div></td>
                    <td class="nowrap-td hide-on-mob"><div class="add-block-wrapper"><span class="add-block">Age</span><span title="15 June 2024">2 years ago</span></div></td>
                    <td class="hide-on-mob"><div class="add-block-wrapper"><span class="add-block">Seeds</span><span class="text-success">33</span></div></td>
                    <td class="hide-on-mob"><div class="add-block-wrapper"><span class="add-block">Leechs</span><span class="text-danger">1</span></div></td>
                </tr>
            </tbody>
        </table>
        </body></html>
    "#;

    #[test]
    fn the_current_layout_gives_hits_with_a_page_and_no_magnet() {
        let hits = parse_search(CURRENT_RESPONSE, &backend()).unwrap();
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.title, "Sintel (2010) 720p BluRay x264 2.0 YTS YIFY");
        assert_eq!(hit.year, Some(2010));
        assert_eq!(hit.magnet, None);
        assert!(!hit.is_usable(), "no magnet, so the engine cannot take it");
        assert_eq!(
            hit.detail_url.as_deref(),
            Some("https://ext.to/sintel-2010-720p-bluray-x264-2-0-yts-yify-15037012/")
        );
        assert_eq!(hit.seeders, Some(33));
        assert_eq!(hit.leechers, Some(1));
        assert_eq!(hit.size_bytes, Some((136.52 * 1024.0 * 1024.0) as u64));
        // The category is the site's, not the uploader's name.
        assert!(hit.detail.as_deref().unwrap().contains("Movies"));
        assert!(!hit.detail.as_deref().unwrap().contains("YTS"));
    }

    #[test]
    fn a_query_becomes_a_browse_url() {
        let backend = backend();
        let url = backend.search_url(&SearchQuery::new("The Matrix"));
        assert_eq!(url, "https://ext.to/browse/?q=The+Matrix");

        let query = SearchQuery {
            text: "Some Show".into(),
            year: Some(2019),
            season: Some(2),
            episode: Some(1),
        };
        assert_eq!(backend.search_url(&query), "https://ext.to/browse/?q=Some+Show+s02e01+2019");
    }

    #[test]
    fn an_empty_query_still_makes_a_well_formed_url() {
        assert_eq!(backend().search_url(&SearchQuery::new("   ")), "https://ext.to/browse/");
    }

    #[test]
    fn rows_map_onto_hits() {
        let hits = parse_search(RESPONSE, &backend()).unwrap();

        // The row with no magnet is kept: its page is still a useful result.
        assert_eq!(hits.len(), 3);

        let first = &hits[0];
        assert_eq!(first.source, "ext.to");
        assert_eq!(first.title, "The 100 S06E01E02 WEB X264-1XBET [eztv]");
        assert_eq!(
            first.magnet.as_deref(),
            Some("magnet:?xt=urn:btih:771B77B1AB402443CC9CE349888EEAA14D60683D")
        );
        assert_eq!(first.seeders, Some(547));
        assert_eq!(first.leechers, Some(117));
        assert_eq!(first.size_bytes, Some((676.9 * 1024.0 * 1024.0) as u64));
        assert!(first.is_usable());
        assert!(first.detail.as_deref().unwrap().contains("TV"));
        assert_eq!(
            first.detail_url.as_deref(),
            Some("https://ext.to/the-100-s06e01e02-web-x264-1xbet-eztv-222829/")
        );

        // The year is read from the release name.
        let second = &hits[1];
        assert_eq!(second.title, "A Film 2021 1080p BluRay x264-GROUP");
        assert_eq!(second.year, Some(2021));
        assert_eq!(second.size_bytes, Some((1.42 * 1024.0 * 1024.0 * 1024.0) as u64));
        assert_eq!(second.seeders, Some(12));
        assert_eq!(second.leechers, Some(3));

        let page_only = &hits[2];
        assert_eq!(page_only.magnet, None);
        assert!(page_only.detail_url.is_some());
    }

    #[test]
    fn a_release_title_that_looks_like_a_label_is_not_read_as_one() {
        // "Seeds of Time" starts with a label word; without the add-block marker
        // it must not be mistaken for a value cell.
        let body = r#"<table class="search-table"><tbody><tr>
            <td class="text-left"><a href="/seeds-of-time-2014-1/"><b>Seeds of Time 2014 1080p</b></a>
            <a class="dwn-btn torrent-dwn" href="magnet:?xt=urn:btih:ABCDEF"></a></td>
        </tr></tbody></table>"#;
        let hits = parse_search(body, &backend()).unwrap();
        assert_eq!(hits.len(), 1, "the row has a magnet and is usable");
        assert_eq!(hits[0].title, "Seeds of Time 2014 1080p");
        assert_eq!(hits[0].seeders, None);
        assert_eq!(hits[0].published_at, None);
        assert_eq!(hits[0].size_bytes, None);
        assert_eq!(hits[0].year, Some(2014));
    }

    #[test]
    fn a_result_reports_how_old_it_is() {
        let hits = parse_search(RESPONSE, &backend()).unwrap();
        // "5 years ago" and "3 weeks ago" are turned into approximate times in
        // the past, not left unset.
        let now = unix_now();
        assert!(hits[0].published_at.is_some_and(|t| t < now));
        assert!(hits[1].published_at.is_some_and(|t| t < now));
    }

    #[test]
    fn sizes_parse_across_units() {
        assert_eq!(parse_size("676.9 MB"), Some((676.9 * 1024.0 * 1024.0) as u64));
        assert_eq!(parse_size("1.42 GB"), Some((1.42 * 1024.0 * 1024.0 * 1024.0) as u64));
        assert_eq!(parse_size("500 KB"), Some(500 * 1024));
        assert_eq!(parse_size("2 TB"), Some(2 * 1024 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("123"), None);
        assert_eq!(parse_size("nonsense"), None);
    }

    #[test]
    fn ages_parse_into_approximate_timestamps() {
        let now = 1_700_000_000;
        assert_eq!(parse_age("30 seconds ago", now), Some(now - 30));
        assert_eq!(parse_age("5 minutes ago", now), Some(now - 300));
        assert_eq!(parse_age("2 hours ago", now), Some(now - 7200));
        assert_eq!(parse_age("1 day ago", now), Some(now - 86_400));
        assert_eq!(parse_age("2 weeks ago", now), Some(now - 14 * 86_400));
        assert!(parse_age("5 years ago", now).unwrap() < now - 4 * 365 * 86_400);
        assert_eq!(parse_age("a while ago", now), None);
    }

    #[test]
    fn the_cloudflare_challenge_is_an_actionable_error() {
        let body = "<html><head><title>Just a moment...</title></head>\
                    <body>window._cf_chl_opt = {cRay: 'x'}; cf_chl_tk</body></html>";
        let error = parse_search(body, &backend()).expect_err("a challenge is not results");
        let message = error.to_string();
        assert!(message.contains("ext.to"), "{message}");
        assert!(message.contains("Cloudflare"), "{message}");
    }

    #[test]
    fn a_challenge_mentions_a_stale_cookie_when_one_is_set() {
        let body = "<title>Just a moment...</title>cf_chl_";
        let backend = backend().with_cookie("cf_clearance=deadbeef");
        let message = parse_search(body, &backend).unwrap_err().to_string();
        assert!(message.contains("stale"), "{message}");
    }

    #[test]
    fn zero_results_is_an_empty_success() {
        let body = "<html><head><title>Download zzz Torrent (0 results) - EXT Torrents</title></head>\
                    <body><table class=\"search-table\"><tbody></tbody></table></body></html>";
        assert!(parse_search(body, &backend()).unwrap().is_empty());
    }

    #[test]
    fn a_page_without_a_results_table_is_an_error_not_an_empty_success() {
        let body = "<html><body><h1>Site maintenance</h1></body></html>";
        let error = parse_search(body, &backend()).expect_err("no table is not zero results");
        assert!(error.to_string().contains("table"), "{error}");
    }

    #[test]
    fn urls_are_configurable_for_a_mirror() {
        let backend = ExtToBackend::new().with_base_url("http://localhost:1/");
        assert_eq!(
            backend.search_url(&SearchQuery::new("x")),
            "http://localhost:1/browse/?q=x"
        );
        assert_eq!(backend.detail_url("/some-torrent-1/"), "http://localhost:1/some-torrent-1/");
        assert_eq!(backend.detail_url("https://other.example/x/"), "https://other.example/x/");
    }

    #[test]
    fn a_backend_is_configured_only_once_it_has_a_cookie() {
        // Nothing set: listed as needing setup, not queried.
        assert!(!backend().is_configured());
        // A cookie makes it usable; disabling overrides even that.
        assert!(backend().with_cookie("cf_clearance=x").is_configured());
        assert!(!backend().with_cookie("cf_clearance=x").disabled().is_configured());
    }

    #[test]
    fn a_solver_also_makes_the_backend_configured() {
        assert!(!backend().is_configured());
        assert!(backend().with_solver_url("http://localhost:8191").is_configured());
        assert!(
            !backend()
                .with_solver_url("http://localhost:8191")
                .disabled()
                .is_configured()
        );
    }

    #[test]
    fn a_solver_response_yields_the_page_and_clearance() {
        let value: Value = serde_json::from_str(
            r#"{
                "status": "ok",
                "solution": {
                    "response": "<html>the results</html>",
                    "cookies": [
                        {"name": "cf_clearance", "value": "abc"},
                        {"name": "__cf_bm", "value": "xyz"}
                    ],
                    "userAgent": "Mozilla/5.0 (Solver)"
                }
            }"#,
        )
        .unwrap();

        let (html, clearance) = solution_from_value(&value, "fallback").unwrap();
        assert_eq!(html, "<html>the results</html>");
        assert_eq!(clearance.cookie, "cf_clearance=abc; __cf_bm=xyz");
        assert_eq!(clearance.user_agent, "Mozilla/5.0 (Solver)");
    }

    #[test]
    fn a_solver_response_without_a_user_agent_falls_back() {
        let value: Value = serde_json::from_str(
            r#"{"status":"ok","solution":{"response":"<html/>","cookies":[]}}"#,
        )
        .unwrap();
        let (_, clearance) = solution_from_value(&value, "fallback-ua").unwrap();
        assert_eq!(clearance.user_agent, "fallback-ua");
        assert_eq!(clearance.cookie, "");
    }

    #[test]
    fn a_solver_failure_is_reported_with_its_message() {
        let value: Value = serde_json::from_str(
            r#"{"status":"error","message":"Challenge not detected"}"#,
        )
        .unwrap();
        let error = solution_from_value(&value, "fallback").unwrap_err();
        assert!(error.to_string().contains("Challenge not detected"), "{error}");
    }

    #[test]
    fn a_solver_response_with_no_page_is_an_error() {
        let value: Value =
            serde_json::from_str(r#"{"status":"ok","solution":{"response":""}}"#).unwrap();
        assert!(solution_from_value(&value, "fallback").is_err());
        let value: Value = serde_json::from_str(r#"{"status":"ok"}"#).unwrap();
        assert!(solution_from_value(&value, "fallback").is_err());
    }

    #[tokio::test]
    async fn a_disabled_backend_is_reported_by_the_aggregator() {
        use crate::search::SearchAggregator;

        let aggregator = SearchAggregator::new(vec![Box::new(
            backend().with_cookie("cf_clearance=x").disabled(),
        )]);
        let error = aggregator
            .search(&SearchQuery::new("anything"))
            .await
            .expect_err("a disabled backend cannot answer");
        assert!(matches!(error, SearchError::NotConfigured));
    }

    #[tokio::test]
    async fn an_unconfigured_backend_is_skipped_rather_than_searched() {
        use crate::search::{SearchAggregator, SearchHit};

        // A working backend next to an ext.to with no cookie: the search
        // succeeds with the other backend's results and never touches ext.to.
        struct Working;
        impl SearchBackend for Working {
            fn name(&self) -> &str {
                "working"
            }
            fn search<'a>(
                &'a self,
                _query: &'a SearchQuery,
            ) -> BoxFuture<'a, Result<Vec<SearchHit>, SearchError>> {
                Box::pin(async move {
                    Ok(vec![SearchHit {
                        magnet: Some("magnet:?xt=urn:btih:abc".into()),
                        ..SearchHit::new("found", "working")
                    }])
                })
            }
        }

        let aggregator = SearchAggregator::new(vec![Box::new(backend()), Box::new(Working)]);
        assert_eq!(aggregator.configured(), ["working"]);
        let results = aggregator.search(&SearchQuery::new("x")).await.unwrap();
        assert_eq!(results.hits.len(), 1);
        assert!(results.failures.is_empty(), "ext.to is skipped, not failed");
    }

    /// The parts the stub cannot check: that ext.to is reachable at all from
    /// here, and that whatever it answers — results or a Cloudflare challenge —
    /// is handled without panicking. Run with a clearance cookie or a solver to get real
    /// results: `EXTTO_SOLVER_URL=http://localhost:8191 cargo test -p reel-catalog -- --ignored`.
    #[tokio::test]
    #[ignore = "requires network access"]
    async fn the_real_site_answers_as_expected() {
        let backend = ExtToBackend::new();
        match backend.search(&SearchQuery::new("sintel")).await {
            Ok(hits) => {
                assert!(!hits.is_empty(), "expected results for a common title");
                assert!(
                    hits.iter().all(|hit| hit.detail_url.is_some()),
                    "every hit must link to its page"
                );
                println!("top hit: {} ({:?})", hits[0].title, hits[0].detail_url);
                // With a solver or cookie configured, the magnet must resolve.
                if backend.is_configured() && backend.solver_url.is_some() {
                    match backend.magnet_for(&hits[0]).await {
                        Ok(Some(magnet)) => {
                            assert!(magnet.starts_with("magnet:"), "{magnet}");
                            println!("resolved magnet: {}", &magnet[..magnet.len().min(80)]);
                        }
                        other => panic!("expected a magnet, got {other:?}"),
                    }
                }
            }
            Err(error) => {
                // Without a browser clearance cookie, being challenged is the
                // expected, correctly-reported outcome — not a parse failure.
                let message = error.to_string();
                assert!(message.contains("Cloudflare"), "{message}");
                println!("challenged, as expected without EXTTO_COOKIE: {message}");
            }
        }
    }

    #[test]
    fn the_magnet_token_is_the_sites_sha256() {
        // `SHA256("<torrent id>|<timestamp>|<page token>")`, verified against a
        // real `getTorrentMagnet.php` request.
        assert_eq!(
            magnet_token(15037012, 1700000000, "tok123"),
            "4f8773943d95db6145c8da7b8651e3c935ca166cefd8816922848bed7e41858b"
        );
    }

    #[test]
    fn a_magnet_response_is_read_from_url_or_hash() {
        let with_url =
            r#"{"success":true,"url":"magnet:?xt=urn:btih:abc","type":"magnet","downloads":5}"#;
        assert_eq!(
            magnet_from_response(with_url).unwrap().as_deref(),
            Some("magnet:?xt=urn:btih:abc")
        );
        let with_hash = r#"{"success":true,"hash":"DEADBEEF"}"#;
        assert_eq!(
            magnet_from_response(with_hash).unwrap().as_deref(),
            Some("magnet:?xt=urn:btih:DEADBEEF")
        );
        let failure = r#"{"success":false,"error":"expired token"}"#;
        let error = magnet_from_response(failure).unwrap_err().to_string();
        assert!(error.contains("expired token"), "{error}");
    }

    #[test]
    fn the_page_tokens_are_read_out_of_inline_scripts() {
        let html = "<script>window.pageToken = 'abc123';</script>\n\
                    <script>window.csrfToken = \"def456\";</script>";
        assert_eq!(inline_js_string(html, "window.pageToken").as_deref(), Some("abc123"));
        assert_eq!(inline_js_string(html, "window.csrfToken").as_deref(), Some("def456"));
        assert_eq!(inline_js_string(html, "window.missing"), None);
    }

    #[test]
    fn the_torrent_id_comes_from_the_url_then_the_button() {
        assert_eq!(
            torrent_id_from_page("<html></html>", "https://ext.to/sintel-2010-720p-15037012/"),
            Some(15037012)
        );
        // A non-numeric trailing segment falls back to the magnet button.
        let html = r##"<a class="detail-magnet" data-id="777" href="#">Magnet</a>"##;
        assert_eq!(torrent_id_from_page(html, "https://ext.to/some-title/"), Some(777));
        assert_eq!(torrent_id_from_page("<html></html>", "https://ext.to/some-title/"), None);
    }

    #[test]
    fn a_broad_query_is_refined_to_the_title_it_agrees_on() {
        let hits: Vec<SearchHit> = [
            "Smiling.Friends.S03E01.1080p.WEB.h264-EDITH",
            "Your Friends and Neighbors S02E10 1080p",
            "Friends (1994) Season 01 S01 (1080p BluRay x265)",
            "Friends (1994) Season 02 S02 (1080p BluRay x265)",
            "Friends (1994) Season 03 S03 (1080p BluRay x265)",
        ]
        .into_iter()
        .map(|title| SearchHit::new(title, SOURCE_NAME))
        .collect();

        // The show's own releases dominate, so they anchor the search — even
        // though other titles also contain the word "friends".
        assert_eq!(anchor_query("friends", &hits).as_deref(), Some("Friends 1994"));
        // A query that already names the title needs no refinement.
        assert_eq!(anchor_query("Friends 1994", &hits), None);
        // Nothing dominates: no anchor.
        assert_eq!(anchor_query("zzz", &hits), None);
    }

    #[test]
    fn dedupe_keeps_one_copy_of_a_torrent() {
        let mut hits = vec![
            SearchHit {
                detail_url: Some("https://ext.to/a/".into()),
                ..SearchHit::new("a", SOURCE_NAME)
            },
            SearchHit {
                detail_url: Some("https://ext.to/a/".into()),
                ..SearchHit::new("a", SOURCE_NAME)
            },
            SearchHit {
                detail_url: Some("https://ext.to/b/".into()),
                ..SearchHit::new("b", SOURCE_NAME)
            },
        ];
        dedupe_hits(&mut hits);
        assert_eq!(hits.len(), 2);
    }
}
