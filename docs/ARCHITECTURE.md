# Architecture

## Goals that shaped the design

1. **Native, cross-platform.** Rust everywhere; no Node/Electron runtime. The
   eventual GUI is a Tauri shell, so all real work must live in reusable
   libraries rather than in a framework.
2. **Play before the download finishes.** Sequential piece prioritisation plus
   HTTP range serving, not "download then play".
3. **One API, many frontends.** The CLI is already a client of the HTTP API. The
   desktop app will be too, so the API has to be pleasant and stable.
4. **No catalog in the engine.** Torrents are identified by opaque ids; anything
   about *titles, posters and discovery* belongs in a layer above.

## Crate layout

```
crates/reel-core     engine, media detection, wire model. No HTTP, no CLI.
crates/reel-http     axum router, range parsing, SSE. Depends on reel-core only.
crates/reel-cli      the `reel` binary: daemon + HTTP client.
scripts/e2e.sh       integration test that proves the streaming path.
```

Dependency direction is strictly one-way:

```
reel-cli ──> reel-http ──> reel-core ──> librqbit
```

Nothing in `reel-core` knows that HTTP exists. `reel-http` knows nothing about
the CLI. That is what makes the GUI a small addition rather than a rewrite.

## reel-core

### `Engine`

Wrapper around a `librqbit::Session`. It owns the session (DHT, listeners,
trackers, storage) and is the only place that maps engine state into `reel`'s
own types.

Why wrap instead of using `librqbit` directly?

* `librqbit`'s wire types (`TorrentDetailsResponse`, `TorrentStats`, ...) change
  between versions and are designed for its own UI. `reel` exposes
  `TorrentView` / `FileView` / `StatsView`, so the API does not churn.
* Its streaming type `FileStream` lives in a private module and is therefore not
  nameable from outside. `Engine` returns a downstream-friendly
  `Box<dyn AsyncRead + Send + Unpin>` instead, and owns positioning.
* Version-specific behaviour (like the media-only file filter, which needs a
  regex that matches nothing) is handled in one place.

Key methods:

| Method | Notes |
| --- | --- |
| `Engine::new(config)` | creates dirs, starts DHT/listeners, optional JSON session persistence |
| `add(source, opts)` | magnet / info hash / `.torrent` URL / `.torrent` bytes |
| `list` / `view` / `stats` | engine state as `reel` model types |
| `probe_file(id, file_id)` | lightweight per-file lookup for the hot streaming path |
| `stream_from(id, file_id, offset)` | the bytes, positioned; opening a stream prioritises that file's pieces |
| `pause` / `resume` / `remove` / `set_only_files` | control |

`add()` sets the per-file filter so only playable files are fetched, and
transparently retries without it if the filter matches nothing (disc images,
odd containers), rather than failing.

### Media detection (`media.rs`)

Torrents are full of things that are not the film: samples, `.nfo`, screenshots.
`pick_primary_file` prefers a non-"sample" video, then the largest video, then
the largest audio file. `media_only_regex()` turns that into a single
`only_files_regex` so the engine never downloads the junk in the first place.
`mime_for_name` provides the `Content-Type` the streaming endpoint reports.

Both are pure functions with unit tests — no I/O, no engine.

### Model (`model.rs`)

`TorrentView`, `FileView`, `StreamTarget`, `StatsView`, `AddRequest`,
`AddOutcome`. Serde-ready, so the same structs serve JSON responses, SSE events
and CLI parsing. `TorrentView::with_base_url` is how relative stream paths
(`/stream/0/0/Movie.mkv`) become absolute URLs in responses — the engine does
not know its own public URL, the HTTP layer does.

## reel-http

### Range handling (`range.rs`)

The part most worth testing, so it is separated from the handlers and covered by
11 unit tests. It handles `bytes=N-M`, `bytes=N-`, `bytes=-N` (suffix),
clamps over-long ends, takes the first of a multi-range request, ignores
malformed headers (per RFC 9110) and reports unsatisfiable ranges so the caller
can emit `416`.

### Streaming

`GET /stream/{id}/{file_id}[/{name}]`:

1. `probe_file` for name, length and MIME type (no full view needed).
2. parse `Range`; decide `200` vs `206` vs `416`.
3. `engine.stream_from(id, file_id, offset)` — this both seeks and, crucially,
   tells the engine to prioritise the pieces around that offset.
4. pipe through `ReaderStream` into an axum body stream.

Because the engine blocks until the needed piece arrives, backpressure is
natural: the HTTP response simply stalls instead of returning corrupt data.

Each request opens its own stream (as rqbit does). Dropping the response drops
the stream, releasing the priority hint.

### Control plane

JSON endpoints under `/api`, plus `/api/events` — an SSE stream that pushes a
full snapshot once per second. The GUI consumes that instead of polling, which
keeps progress bars, speeds and peer counts live for free.

CORS is **off** unless explicit origins are passed to
`build_router_with_allowed_origins`. A localhost API that can delete files must
not be silently reachable from any web page the user visits.

## reel-cli

`main.rs` holds clap definitions and the daemon; `client.rs` is a thin HTTP
client (the CLI is a first-class API consumer, which keeps the API honest);
`format.rs` is presentation only.

`serve` binds, computes the public base URL, adds any `--add` sources, then
serves until Ctrl-C and flushes session state so the next start is fast.

## Integration test

`scripts/e2e.sh` is the real proof. It generates a video, creates a torrent,
seeds it from one instance with a throttled upload, and streams ranges out of a
second instance that must fetch the data from the first. Throttling matters:
without it, a LAN/host-local transfer finishes before any assertion can observe
partial state.

## Where the catalog goes next

The engine deliberately knows nothing about discovery. Planned layering:

```
reel-core        engine (this repo)
reel-http        API (this repo)
reel-catalog     TMDB metadata + poster cache + search-backend trait   <- next
reel-desktop     Tauri app: catalog UI + native player surface         <- next
```

`reel-catalog` maps a `TorrentView` (or a magnet) to a catalog entry: title,
year, artwork, description, rows. Search backends are behind a trait so the
project ships no indexers of its own and any source is pluggable.

Playback in the desktop app can then be either the embedded player (WebKit
webkitmedia / a native surface fed by the same `/stream` URL) or an external
`mpv` handoff, which the CLI already demonstrates.
