# Adding a search source

`reel` searches nothing by default except one bundled source, and that is a
deliberate design: where torrents come from is the operator's decision and the
operator's responsibility. This document explains the seam, walks through the
bundled backend as a template, and describes what writing your own involves.

## The seam

One trait, in `crates/reel-catalog/src/search.rs`:

```rust
pub trait SearchBackend: Send + Sync {
    /// Shown next to every result.
    fn name(&self) -> &str;

    /// Return false when the backend needs configuration it does not have, so
    /// the UI can say so instead of showing an empty result list.
    fn is_configured(&self) -> bool { true }

    fn search<'a>(
        &'a self,
        query: &'a SearchQuery,
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, SearchError>>;
}
```

`SearchQuery` carries the text plus optional year, season and episode.
`SearchHit` is what you return:

| field | notes |
| --- | --- |
| `title` | shown to the user |
| `year`, `size_bytes` | optional, shown if present |
| `seeders`, `leechers` | optional. **Set `None` rather than guessing** if your source is not a swarm tracker |
| `popularity` | optional, used only to order results when `seeders` is unknown |
| `source` | your `name()` |
| `magnet` / `torrent_url` | **at least one is required**: a hit with neither is dropped by the aggregator |
| `detail`, `published_at` | free-form extras |

`SearchAggregator` runs every configured backend, merges the results, drops
unusable hits, sorts by seeders → popularity → size, and collects per-backend
failures instead of letting one broken source hide the others.

## The worked example

`crates/reel-catalog/src/backends/archive_org.rs` implements the whole trait
against the Internet Archive: one HTTP request, no scraping, no credentials.
Copy it. The shape worth copying:

```rust
pub struct MyBackend {
    http: reqwest::Client,
    base_url: String,
}

impl SearchBackend for MyBackend {
    fn name(&self) -> &str { "my-source" }

    fn search<'a>(
        &'a self,
        query: &'a SearchQuery,
    ) -> BoxFuture<'a, Result<Vec<SearchHit>, SearchError>> {
        Box::pin(async move {
            let response = self.http
                .get(&self.base_url)
                .query(&[("q", build_query(&query.text))])
                .send()
                .await
                .map_err(|e| SearchError::Backend {
                    backend: self.name().into(),
                    message: e.to_string(),
                })?;

            if !response.status().is_success() {
                return Err(SearchError::Backend { /* ... */ });
            }

            let hits = parse_hits(&response.text().await /* ... */)?;
            Ok(hits)
        })
    }
}
```

Things that example gets right and are easy to miss:

* **Build the query in a pure function.** `build_query` is unit-tested, including
  with hostile input, because a user can type anything and a stray bracket must
  not change the query's meaning.
* **Parse in a pure function.** `parse_search` takes a `serde_json::Value` and is
  tested against fixtures. This is where the real bugs live, and it needs no
  network to test.
* **Expect inconsistent field types.** Real APIs return `"year": 1922`,
  `"year": "1922"`, `"year": "1922-03-04"` and `"title": ["a", "b"]` in the same
  response. `first_string` and `first_year` handle it.
* **Set a timeout and a user agent** on the client. Not optional: a hanging
  backend makes search feel broken.
* **Return `SearchError`, never panic.** One bad source must not take the app
  down.

## Registering it

The desktop app builds its aggregator in one place,
`crates/reel-desktop/src/backend.rs`:

```rust
let mut backends: Vec<Box<dyn SearchBackend>> = Vec::new();
if !catalog_options.disable_bundled_sources {
    backends.push(Box::new(ArchiveOrgBackend::new()));
}
// Your source goes here, reading its own configuration:
if let Ok(url) = std::env::var("MY_INDEXER_URL") {
    backends.push(Box::new(MyBackend::new(url)));
}
let search = SearchAggregator::new(backends);
```

That is the whole integration. The search screen, the results list, the Add
button and the failure reporting all work against the trait already.

Configuration belongs here rather than in the trait: a backend should be
constructible only when it has what it needs, and `is_configured` should reflect
that.

## Testing

Follow the pattern in `archive_org.rs`:

1. **Unit-test the pure parts** — query building and response parsing — against
   fixtures. That is where the coverage should be.
2. **Add one `#[ignore]`d test** that hits the real source:

   ```bash
   cargo test -p reel-catalog -- --ignored --nocapture
   ```

   A stub can only confirm that your code matches *your* idea of the API.
3. **If your source is a JSON or HTML endpoint you control**, run a stub server
   in a test and point `base_url` at it — `crates/reel-catalog/tests/tmdb_stub.rs`
   is a complete example, including asserting which auth header was sent.

## If you are thinking about a public indexer site

It is your app and your call, but the practical picture is worth knowing before
you start, because "just scrape it" is not the hard part:

* **Anti-bot protection.** Most public indexers sit behind Cloudflare or
  equivalent. You will be handling JS challenges, cookie/token flows, and
  frequent changes to them. It is not a `reqwest::get` away.
* **It breaks constantly.** These sites change markup and add protections
  regularly. Expect your parser to rot on a schedule.
* **Rate limits and bans.** Aggressive querying gets an IP blocked. Be polite:
  cache, back off, and set a realistic user agent.
* **Terms of service and law.** Scraping may breach the site's terms, and
  helping people find copyrighted material carries exposure that varies by
  jurisdiction. That is a decision worth making deliberately rather than by
  accident.
* **It is not needed for the engine.** Nothing else in reel cares where a magnet
  came from. `AddSource` accepts a magnet, a `.torrent` URL, a local file or an
  info hash, and the streaming path is identical.

For the record: I did not write an adapter for ext.to. Its purpose is indexing
copyrighted film, so writing the scraper is not something I will do. The seam
above is what you would use, the bundled Archive backend is a complete template,
and the rest is a decision for whoever runs the app.

## A note on what still needs building

The search *screen* exists (`Screen::Search`), but it is deliberately thin:

* one query box, no filters (year, resolution, seeders),
* no pagination — `rows` is fixed per backend,
* results are not cached between queries.

Those are all small, content-neutral additions to `App::search_screen` in
`crates/reel-desktop/src/ui.rs`, and none of them need a particular source.
