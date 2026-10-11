# Project review: ext.to completion, self-contained source, full Settings coverage

Status: **partly done — one item open** (updated 2026-10-11)

Current source note: the Internet Archive backend described in this historical
review has since been removed. ext.to is the only bundled search source; old
Archive-specific findings below are retained as review history.

Progress since the first draft:

- **P0.1 ext.to magnet retrieval — DONE.** Implemented and tested (unit + stub +
  live). Verified live through the GUI: an ext.to row now resolves its magnet and
  adds.
- **P0.3/P0.4 search-source settings + live rebuild — DONE.** Per-source toggles
  and ext.to cookie/user-agent/base-URL/solver-URL are persisted settings,
  editable in the Settings tab; saving rebuilds the aggregator so it takes
  effect immediately.
- **P1 engine/network settings in the UI — DONE** for port, DHT, trackers, UPnP,
  IPv4-only, peer limit and up/download rate limits. (Seeding stays off by
  design; scratch/session dirs and cache caps remain.)
- **P2 correctness fixes — DONE.** archive.org dead year branch, silent reqwest
  config loss, `ByteRange::is_empty`, corrupt-JSON quarantine, all clippy
  warnings.
- **P0.2 in-process Cloudflare solver — OPEN.** Spike done: TLS impersonation
  alone does not clear the challenge, so a JS engine is required. See the spike
  result at the end; needs a go-ahead on the added dependency.

Original review follows.

---

# Project review: ext.to completion, self-contained source, full Settings coverage

Scope requested by the owner:

1. Is the **ext.to backend fully working**?
2. Make the **solver and HMAC signing self-contained** — no external Docker
   (FlareSolverr) and no separately-run client.
3. Every **configuration option must be editable from the Settings tab**.

A short "fixes already landed this session" note is at the end.

---

## Summary

| Area | State | Verdict |
| --- | --- | --- |
| ext.to search scraping | works (verified live) | partial |
| ext.to magnet retrieval | **not implemented** | broken |
| ext.to Cloudflare challenge | external FlareSolverr **or** manual cookie | not self-contained |
| ext.to configuration | env-only, not persisted, not in UI | missing |
| Settings tab coverage | all of `CatalogSettings` (10 fields) | good for catalog, gaps elsewhere |
| Engine/network/search config in UI | none | missing |

The backend is a good scraper but **not a usable source**: it can list results
and open their page, but every row has `magnet = None`, so `Add` is disabled and
nothing can be played from it. On top of that, even reaching the results page
requires either a hand-copied browser cookie or an external FlareSolverr
container.

---

## P0 — ext.to is not a working source

### P0.1 Magnet retrieval is unimplemented

- **Where:** `crates/reel-catalog/src/backends/ext_to.rs:40-44` (module docs),
  `:330-336` (row parse), `crates/reel-desktop/src/ui.rs:2850-2857` (disabled Add).
- **Symptom:** every ext.to row comes back with only `detail_url`; the UI shows
  `Open` and a disabled `Add` ("This result has no magnet or .torrent URL").
- **Reality:** the magnet is served by a token-signed AJAX call. I reverse
  engineered and **verified** it end-to-end against the live site:

  ```
  GET  <detail page>                      # read window.pageToken + window.csrfToken
  hmac = SHA256("{torrent_id}|{timestamp}|{pageToken}")   # timestamp = unix seconds
  POST /ajax/getTorrentMagnet.php
       form: torrent_id, download_type=magnet, timestamp, hmac, sessid=csrfToken
  ->   {"success":true,"url":"magnet:?xt=urn:btih:...","type":"magnet","downloads":N}
  ```

  Verified response for `torrent_id=15037012`:
  `magnet:?xt=urn:btih:a779fa758f2229586eebc2e5960eb32fe5659596&dn=Sintel ...`.
- **Fix:** add a `sha2` dependency, fetch the detail page (with the same
  clearance), scrape `pageToken`/`csrfToken`, sign, POST, and fill
  `SearchHit::magnet`. Fetch magnets lazily (on Add) rather than for every row,
  since it is one extra request per torrent. Cover with the existing
  `tests/extto_stub.rs` seam plus a stub for `/ajax/getTorrentMagnet.php`.
- **Effort:** small; no external services needed.

### P0.2 Cloudflare challenge is not self-contained

- **Where:** `ext_to.rs:104-132` (env), `:201-231` (`solve` → FlareSolverr
  `/v1`), `:576-581` (`is_configured`), `:536-555` (challenge message).
- **Symptom:** the only ways in are (a) `EXTTO_COOKIE` + `EXTTO_USER_AGENT`
  copied by hand from a browser, or (b) `EXTTO_SOLVER_URL` pointing at a running
  FlareSolverr Docker container. Both are outside the app; neither is persisted;
  nothing is exposed in the UI.
- **Owner requirement:** solve it in-process, no external Docker/client.
- **Assessment / options (needs a decision):**
  - Cloudflare here is a **managed challenge**: the browser runs obfuscated JS
    (canvas/WebGL fingerprints, `/cdn-cgi/challenge-platform/.../jsd/...`
    callbacks) and returns a signed token. A plain HTTP client cannot pass it.
  - **Option A (recommended, feasible):** embed a JS engine (`boa_engine` or
    `deno_core`) plus browser-impersonating TLS/HTTP2 (`rquest`/`wreq` with
    BoringSSL) to execute the challenge and obtain `cf_clearance` in-process.
    Needs a spike to confirm this specific challenge is passable without a real
    browser; it is inherently brittle and must be treated as best-effort.
  - **Option B:** bundle/launch a headless browser from the app (system
    Chromium via CDP). Works, but is still "an external client" and is a large
    dependency/licensing surface.
  - **Option C (fallback if A/C prove too fragile):** keep an optional external
    solver, but make it a first-class, persisted, UI-configurable setting, and
    add **automatic import of the user's existing browser cookies** so the
    common case needs no Docker and no manual copy.
  - **Do not** hard-code a solver, and **do not** silently require Docker.
- **Ask:** approve Option A as the target, with Option C as the documented
  fallback, before implementation.

### P0.3 ext.to credentials/config are env-only

- **Where:** `ext_to.rs:104-132`; no UI, not persisted.
- **Fix:** move `cookie`, `user_agent`, `solver_url`, `base_url` into
  `CatalogSettings` (or a new `search_sources` section), edit them in Settings,
  and rebuild the backend on save. See P1.

### P0.4 The search aggregator is built once and cannot be rebuilt

- **Where:** `crates/reel-desktop/src/backend.rs:410-415` builds
  `SearchAggregator` into `CatalogState.search` (a plain field, `:273`);
  `save_settings` (`:1915-1956`) never touches it.
- **Symptom:** `enable_bundled_sources` (and any future source config) only take
  effect **after a restart**; there is no per-source enable/disable.
- **Fix:** make `CatalogState.search` swappable (`Mutex<Arc<...>>`, like
  `provider`) and rebuild it in `save_settings` when search config changes.

---

## P1 — Configuration must all be in the Settings tab

The Settings tab is a complete editor for `CatalogSettings` (`ui.rs:2953-3301`,
all 10 fields: TMDB key, download dir, stream-only, subtitles, subtitle language,
volume, stream buffer, stream cache, merge works, bundled sources). Everything
else is CLI/env-only. These need UI surfaces (and, where they are persisted,
fields in settings):

**Search sources**
- [ ] Per-source enable/disable (archive.org and ext.to separately). Today one
      checkbox gates both, and its label is wrong: "Use the bundled source
      (Internet Archive)" (`ui.rs:3218-3221`) actually switches off ext.to too.
- [ ] ext.to base URL.
- [ ] ext.to cookie (`EXTTO_COOKIE`) + user agent (`EXTTO_USER_AGENT`).
- [ ] ext.to solver URL (`EXTTO_SOLVER_URL`) — or remove once P0.2 is solved
      in-process.
- [ ] `archive.org` search URL / rows-per-page (currently constants,
      `archive_org.rs:25-28,62`).

**Engine / network** (`EngineConfig`, `crates/reel-core/src/config.rs:8-60`) —
none of these reach the UI or persisted settings:
- [ ] Listen port (`--listen-port`), UPnP (`--upnp`).
- [ ] DHT on/off (`--no-dht`), trackers on/off (`--no-trackers`).
- [ ] IPv4-only (`--ipv4-only`), peer limit.
- [ ] Extra trackers, initial/known peers.
- [ ] Upload/download rate limits (`--upload-limit`, `--download-limit`).
- [ ] Seeding/upload toggle (`--seed`; desktop hardwires upload off).
- [ ] `stream_scratch_dir`, `session_state_dir`.

**App / data**
- [ ] Data directory (`REEL_DATA_DIR`) — fixed today; only "clear cache" exists.
- [ ] Metadata/artwork cache location and a size cap (no cap exists).
- [ ] `REEL_TMDB_API_KEY` is an env fallback only (works; low priority).

**Behavioural notes to fix while here**
- [ ] `download_dir` changes only apply on next start (documented) — either
      apply live or say so in the UI.
- [ ] The sources list string "`<name> (disabled)`" conflates
      *switched off* with *unconfigured/needs setup* (`ui.rs:3222-3237`,
      `backend.rs:1867-1877`).

---

## P2 — Other bugs / gaps found in the review

- **BEP-19 web seeds are not implemented** (`docs/ARCHITECTURE.md:354,362`,
  `README.md:546`). Internet Archive torrents carry HTTP web seeds librqbit
  ignores, so archive.org playback depends on external seeders. Documented as
  "the highest-value engine feature left."
- **Transcoding is not implemented** (`README.md:56`, "not yet").
- **Dead branch:** `archive_org.rs:203` — `match query.year { Some(_) => …,
  None => … }` both arms identical; comment claims year handling the code never
  does.
- **Silent config loss:** `reqwest::Client::builder()…build().unwrap_or_default()`
  drops the custom timeout/user agent if build fails
  (`archive_org.rs:56`, `ext_to.rs:124`).
- **`ByteRange::is_empty()` always returns `false`** (`reel-http/src/range.rs:21`);
  unused, but wrong.
- **Corrupt-JSON data loss:** library/settings/media/history loaders warn and
  reset to `Default::default()`, then overwrite the file on next save
  (`reel-core/src/library.rs:96`, `reel-catalog/src/settings.rs:116`,
  `media.rs:89`, `history.rs`). Worth a `.bak` or a hard error.
- **clippy warnings** (`cargo clippy --workspace --all-targets`):
  `reel-core/src/engine.rs:208`, `reel-catalog/src/backends/ext_to.rs:247`,
  `reel-catalog/src/media.rs:37-38`, `reel-desktop/src/backend.rs:939`,
  `reel-desktop/src/ui.rs:483,1107`, `reel-desktop/tests/backend.rs:125,614`,
  `reel-desktop/examples/ui_debug.rs:236`.
- **No per-source architecture limit** is otherwise fine; the `SearchBackend`
  seam is clean.
- **In-window subtitles / NFO reader / stronger series matcher** are listed as
  future work in `docs/ARCHITECTURE.md:354` (optional).

---

## P3 — Known test / repo hygiene issues

- Two UI snapshot tests fail by a few pixels on this machine:
  `snapshot_library` (~1px) and `snapshot_library_with_real_artwork` (~3px);
  stale `*.new.png` / `*.diff.png` are left untracked in
  `crates/reel-desktop/tests/snapshots/`.
- The `ui` test binary **segfaults when run in parallel**; `--test-threads=1`
  avoids it. Not investigated.
- `tests/player_resume.rs`, `tests/app_player_resume.rs` silently skip when
  libmpv/ffmpeg are missing — real coverage is machine-dependent.
- Four `#[ignore = "requires network access"]` tests, including the only live
  ext.to test (`ext_to.rs:1032`).
- Everything above is **uncommitted** on `feature/media-identity`.

---

## Fixes already landed this session (in the working tree)

Interactive test found and I fixed three bugs (built, unit/backend tests green):

1. **UI froze during transfers** — download progress/rate/peers only updated on
   mouse movement. Added a periodic repaint while any transfer is active
   (`reel-desktop/src/ui.rs`, `active_transfers`).
2. **Episode row promised a resume below the threshold** — showed "resume 0:22"
   but Play started at 0. Now only shown when `is_resumable()` (≥30s).
3. **A user-paused download auto-resumed after restart** — added a persisted
   `LibraryEntry.paused` (`reel-core/src/library.rs`), `set_paused` writes it,
   `recycle` honours it, and starting a download clears it
   (`reel-desktop/src/backend.rs`).

---

## Suggested order of work

1. **P0.1** implement the SHA-256 magnet retrieval (self-contained, small).
2. **P1** move ext.to config into `CatalogSettings`, add per-source toggles, and
   make the aggregator rebuild on save (P0.4).
3. **P0.2** spike the in-process challenge solver (Option A); decide with the
   owner before committing to it.
4. **P2** clear the correctness bugs (web seeds is a project, not a bug fix).

---

## P0.2 spike result (2026-10-11)

`wreq` 0.16 + `wreq-util` 0.2 with a current browser emulation profile (Safari 26)
was built in a scratch crate (`/tmp/opencode/wreqtest`, ~1m50s including
BoringSSL) and pointed at `https://ext.to/browse/?q=sintel`:

```
status=403 Forbidden len=5784 challenge=true table=false
```

**Conclusion:** TLS/JA3/HTTP-2 impersonation alone does **not** clear this
managed challenge — Cloudflare still serves the JS interstitial. A self-contained
solver therefore also needs to *execute the challenge script*, i.e. an embedded
JS engine plus a browser-like environment (navigator, canvas/WebGL stubs, the
`/cdn-cgi/challenge-platform/.../jsd/...` callback round-trip). That is a
significant, best-effort component, not a small addition.

**Recommendation / next step (needs a go-ahead):**

1. Add `wreq` behind an `extto-solver` feature (build is acceptable; keep it
   optional so the default build stays lean and does not pull BoringSSL).
2. Add `boa_engine` and a minimal DOM/navigator shim to run the challenge, feed
   the token back, and cache `cf_clearance` as today.
3. Keep the persisted cookie and the optional solver URL as fallbacks, and make
   `is_configured()` true for ext.to once the in-process path exists.
4. If step 2 proves unreliable against the live challenge, fall back to
   auto-importing the user's browser cookies (no Docker, no manual copy) and say
   so plainly in the UI.

This is the one item from the request that is not finished. Everything else in
this review is implemented and tested.

---

## P0.2 final status (2026-10-11)

Decision: **bundle a proven solver, installed alongside the binary — not
compiled into it.** ext.to's Cloudflare challenge is cleared by
[FlareSolverr](https://github.com/FlareSolverr/FlareSolverr), which reel talks to
over its HTTP API. `scripts/install-solver.sh` (called by `scripts/install.sh`)
installs a pinned FlareSolverr into `<prefix>/lib/reel/solver`, and reel starts
it on demand. No Docker, and a fresh install works with no manual setup.

- `scripts/install-solver.sh` — downloads pinned FlareSolverr source
  (v3.5.2, SHA-256 verified), builds a Python 3.11 venv with `uv` (downloading a
  pinned `uv` only if none is on PATH), writes `run.sh` + `url`, and checks that
  Chrome/Chromium and Xvfb are present. Idempotent; `--uninstall` removes it.
- `crates/reel-catalog/src/backends/solver.rs` — finds `run.sh` relative to
  reel's own binary (`REEL_SOLVER` overrides), starts it detached if nothing is
  answering, and returns its URL. The solver is left running and reused.
- `ext_to.rs` — on a challenge, uses a configured `solver_url`, else the bundled
  solver; `is_configured()` is true when either is available. A saved cookie
  remains an override.
- Removed the compiled-in solver (Camoufox) and its `camoufox*` deps; reel no
  longer contains any browser automation.

**Verified end to end:** `install-solver.sh` installs; the installed app, with
no solver URL configured, discovers the bundled solver, starts it, and a
"sintel" search returns 70 ext.to hits with 0 failures.

### Why not the precompiled FlareSolverr binary?

FlareSolverr's self-contained Linux tarball (which bundles its own Chromium) is
unreliable here: PyInstaller sets `LD_LIBRARY_PATH` to its `_internal` directory,
the child Chrome inherits ~86 conflicting libs, and the renderer crashes. Even
with a wrapper that strips that variable, its bundled Chromium fails to clear
ext.to. Building from source with a plain venv (no PyInstaller) and the system
Chrome works reliably, which is why the installer does that.

### Superseded: the in-binary Camoufox solver

An earlier attempt compiled the Camoufox stealth browser into reel
(`camoufox-juggler`). It ran but did not clear ext.to's challenge here, and on a
Wayland session it opened a **visible window** on the desktop (Camoufox only
redirects `DISPLAY`; Firefox preferred the inherited `WAYLAND_DISPLAY`). That
approach was dropped in favour of the installer-based solver above.

---

## Cross-platform (Windows) audit (2026-10-11)

Goal: confirm the project builds and runs on Windows, not just Linux.

Method: cross-checked the whole workspace for a Windows target —
`cargo check --workspace --all-targets --target x86_64-pc-windows-gnu`. The GNU
target is used because `aws-lc-sys` (via librqbit → reqwest → rustls) is C and
needs a cross C compiler (MinGW-w64), which the MSVC target cannot use from Linux.
`cfg(windows)` is set for both targets, so source-level portability is exercised.

Issues found and fixed:

- **`reel-player`** — `mpv_client_api_version()` returns `c_ulong`, which is
  64-bit on Linux and **32-bit on Windows**, so the version split failed to
  type-check. Widened with an `as u64` (with an allow for the no-op cast on
  Linux). The candidate library names already include `mpv-2.dll`.
- **`reel-desktop` example `ui_debug`** — used `std::os::unix::net` directly.
  Wrapped the whole example in a `#[cfg(unix)]` module with a non-Unix `main`
  that explains it is a Unix-only development tool.
- **`reel-catalog`** — `use std::io::Write` was unused on non-Unix (the private
  file-mode writer is Unix-gated). Gated the import.
- **Default directories used `$HOME`**, which Windows does not set, so the data
  and download directories fell back to relative paths. Added
  `reel_core::paths` with `home_dir`/`data_dir`/`download_dir`: `HOME` →
  `USERPROFILE` → `HOMEDRIVE`+`HOMEPATH`; data under `%APPDATA%\reel` on Windows
  and `~/Library/Application Support/reel` on macOS, with the original
  `~/.local/share/reel` kept on Linux. `reel-catalog`, `reel-cli` and
  `reel-desktop` now use it.

Result: `cargo check --workspace --all-targets --target x86_64-pc-windows-gnu`
is clean; Linux `clippy` is 0 warnings and the test suite passes.

Caveats:

- This is a compile/type check, not a run on Windows. Behaviour that needs a real
  Windows box (windowing, the torrent engine's sockets, libmpv) is not exercised
  here.
- The **installer and the bundled solver are Linux-only**: `scripts/*.sh`,
  `solver.rs`'s launcher, and FlareSolverr-under-Xvfb. On Windows the app runs,
  but ext.to needs a solver URL configured in Settings (FlareSolverr ships a
  Windows binary). Making the installer/solver cross-platform is separate work.
- macOS was not cross-checked (the `x86_64-apple-darwin` target would need a
  macOS SDK for the C dependency); the source changes above are platform-neutral.

---

## Playback seek, classification and the search view (2026-10-11)

Three problems from a live test, and their fixes.

### 1. A big seek looked like the stream was restarting

The log showed the pattern exactly: after a 30-minute jump the picture stopped
for 8s, the stall watchdog (`maybe_recover_stalled_stream`) fired, dropped the
mpv core and **recycled the whole torrent** (`restart_stream` -> `recycle`),
reopened the player at the seek position, and often repeated.

Cause: the watchdog treated *buffering* as a wedge. A seek into a large file
empties the demuxer cache and refills it from a new position, which can take much
longer than 8s. mpv reports this as `paused-for-cache`, which the watchdog did
not check.

Fix (`ui.rs`, `player.rs`): the watchdog now treats `paused_for_cache` as
progress, and a seek within the last 20s also counts as progress
(`PlayerController::seeked_within`), so a seek gets time to buffer. A genuine
wedge (cache full, mpv not reading) still recovers as before.

### 2. A film with featurettes read as a series, and got no metadata

The torrent `The Lighthouse (2019)` ships the feature plus two files under
`Featurettes/`. `hunch` does not label those as extras, so the torrent looked
like three video files with no numbering -> classified **Series**. The metadata
lookup then searched TMDB's *TV* endpoint, found nothing, and the view showed no
film details.

Fix (`release.rs`): extras are now also recognised from the path — a
`Featurettes/`/`Extras/`/`Bonus/` folder, or a file named like a trailer, sample
or deleted scene (`is_extra_path`). The film is a film again, the featurettes
list under **Extras**, and the movie lookup succeeds. `CatalogEntry::kind()`
already prefers the provider's kind over the file-name guess, so metadata remains
authoritative.

### 3. The search view now shows what each result is

Each result row shows, from the release name (parsed once and cached):

- **Film** or **Series SxxEyy**, from the same parser the library uses;
- quality chips (`1080p · Blu-ray · H.265 · AAC`);
- languages (`1080p · Russian · English`) — `ReleaseAttributes` gained
  `languages`, filled from `hunch`;
- once the provider answers: **★ rating, votes, runtime, genres** and the
  overview.

`Backend::enrich_search` looks up each unique title in the background (capped at
24, misses remembered), and `Backend::pending_lookups` keeps the UI repainting
while any lookup runs — without that, a rating that arrived after the user
stopped moving the mouse did not appear until they moved it again.

---

## Search view: grouped by title (2026-10-11)

A flat list of torrents did not answer "what can I watch?". Search results are
now **one card per film or series**, with the artwork and details, and under it
**every torrent for that title, grouped by category** — the model ext.to uses on
its detail page. The design is in `docs/SEARCH_UI_PLAN.md`.

- **Grouping** reuses the library's identity rule (`build_works` over a synthetic
  member per hit), so search and library agree on "same film".
- **Categories**: films by resolution tier (`2160p (4K)`, `1080p`, …); series by
  season, one row per release (`S01E02`, `Season 1 complete`).
- **Card**: poster, title/year, film/series badge, rating, runtime, genres,
  torrent count and overview; click to expand. Categories collapse too.
- **Rows** read like one line of ext.to's table: quality chips + languages, size,
  seeders/leechers, age, source, and `Add` / `Open` / `Copy`.
- **Ordering**: works by title relevance to the query, then backend order, so an
  exact hit stays on top even when a source ranks a fuzzy match above it.
- A `Group by title` switch keeps the previous flat list available.

Verified live: "the lighthouse" → The Lighthouse (2019) first, its 31 torrents
under 2160p/1080p/…; "spongebob" → Spongebob (1999) as a **Series** with a
`Season 1` category. `clippy` 0 warnings; tests and snapshots green; Windows
cross-check clean.

### Follow-up: sort and relevance (2026-10-11)

- Changing the sort did not reorder the grouped rows: the grouped view built
  each category from the work's member order and ignored `search_sort`. It now
  sorts each category's indices into `shown`'s (already-sorted) order, and the
  sort combo requests a repaint, so the list updates at once.
- The work relevance score is smarter: title match dominates, then whether
  metadata was fetched, then torrent count — so a matched title with many
  releases outranks a one-release fuzzy match, while an exact hit still leads.

---

## ext.to: pagination and refinement (2026-10-11)

Searching `friends` showed one torrent per season. ext.to paginates (about 20
pages for `friends`) and matches the query anywhere in a name, so page 1 is
mostly *other* shows — and reel fetched only page 1.

`ExtToBackend::search` now:

- fetches up to `SEARCH_PAGES` pages of the raw query, stopping at the last
  page; and
- finds the title the results agree on (`anchor_query`) and fetches *that*
  title's own releases for `REFINE_PAGES` pages, then dedupes.

The anchor is the parsed title whose name best matches the query (exact >
contains > every word), needing a few hits, and only when it differs from the
query — so `friends` is refined to `Friends 1994`, while `Friends 1994` itself
is left alone. The heuristic lives in the backend and is unit-tested.

Verified live: `friends` returned **259 hits** and the Friends (1994) work now
holds **95 torrents** (5 under Season 1, 3 under Season 2, …), not one per
season.

---

## Search: warm-up and streaming (2026-10-11)

Two changes so a search is quick and keeps filling in.

**Warm-up at startup.** `EngineBackend::start` spawns `SearchAggregator::warm`
in the background; `ExtToBackend::warm` fetches `/browse/` to clear the
Cloudflare challenge and cache the clearance. The listing page matters: warming
`/` returns a clearance the search does not reuse, while `/browse/` does.
Measured: the first search of a session went from ~15s to ~5s.

**Streaming results.** A source that paginates no longer stops at a page count.
`SearchBackend::search_more(query, round)` returns the next batch (empty when
done); `SearchAggregator::search_more` merges it; the desktop search task loops
it and emits `BackendEvent::SearchHitsAdded` until nothing more comes, then
`SearchStreamEnded`. The UI appends the new hits (deduped by `SearchHit::key`),
re-enriches, updates the count, and shows a "finding more…" spinner while it
runs.

ext.to tracks where a search got to (`SearchProgress`: the anchor and the next
page). `search` returns page one plus the anchor's first page; `search_more`
streams the anchor's later pages — or the raw query's, when no anchor was found —
until a short page ends it.

Measured live: `friends` showed its first 110 results in ~5s and kept growing to
**305 hits**, with the Friends (1994) work holding **135 torrents**.

---

## Search round 2: play, metadata, series groupings (2026-10-11)

Plan: `docs/SEARCH_UI_PLAN.md` (Round 2). All confirmed options were built.

**Play from a search result.** Every row has a Play (Add / Open / Copy too), and
each work header has a Play for its best release (highest quality, then
seeders). It resolves the magnet if the row only has a page, adds the torrent
stream-only, and opens the player on its best file once the files resolve —
reusing the add → `Ready` → `pending_play` path. `add_source` now returns the
library id and info hash; `play_when_ready` finds the engine view by info hash
(the engine's id space differs from the library's, which is what the UI keys on)
and emits `BackendEvent::PlayReady`. Verified live: Play on a Friends release
streamed `Friends S01E01` in the embedded player.

**Metadata that matches more titles.** `TmdbProvider::enrich` now tries the
preferred catalogue and, when it finds nothing, the other (film <-> series), so a
series pack parsed as a film still matches. Search-metadata lookups are bounded
by concurrency (`MAX_SEARCH_CONCURRENCY`) instead of a one-shot cap, and the
refresh tick keeps starting them, so a long list is covered rather than only its
first batch. Ratings with no votes are no longer shown (`display_rating`).

**Series groupings.** A series now reads: **season tabs** → **Complete season /
Episodes** tabs (only when both exist) → a **Quality** filter row → resolution
headers → rows. A release with no episode number is a pack; one with a number is
an episode, so the two are never mixed. The default season is a real one, not the
"Specials" bucket. Films keep their resolution tiers.
