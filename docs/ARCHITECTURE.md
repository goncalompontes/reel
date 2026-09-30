# Architecture

## Goals that shaped the design

1. **Native, cross-platform.** Rust everywhere; no Node/Electron runtime and no
   webview. All real work lives in reusable libraries, so the GUI is a consumer
   rather than the place the logic lives.
2. **Play before the download finishes.** Sequential piece prioritisation plus
   HTTP range serving, not "download then play".
3. **One API, many frontends.** The CLI is a client of the HTTP API, and the
   desktop app mounts that same server in-process so its player receives plain
   HTTP stream URLs.
4. **No catalog in the engine.** Torrents are identified by opaque ids; anything
   about *titles, posters and discovery* belongs in a layer above.

## Crate layout

```
crates/reel-core     engine, media/title detection, wire model. No HTTP, no UI.
crates/reel-http     axum router, range parsing, SSE. Depends on reel-core only.
crates/reel-player   libmpv playback as RGBA frames. No UI toolkit.
crates/reel-cli      the `reel` binary: daemon + HTTP client.
crates/reel-desktop  the egui/wgpu app. Depends on the three above.
scripts/e2e.sh       integration test that proves the streaming path.
```

Dependency direction is strictly one-way:

```
reel-desktop ──> reel-player ──> libmpv (dlopen)
      │
      ├───────> reel-http ──> reel-core ──> librqbit
      └───────> reel-core
reel-cli ─────> reel-http ──> reel-core
```

Nothing in `reel-core` knows that HTTP or mpv exist. `reel-http` knows nothing about
the CLI, and `reel-player` knows nothing about egui. That is what makes the GUI a
small addition rather than a rewrite.

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
partial state. Steps 17–19 then hand the stream URL to libmpv and assert that a
real, non-blank, fully opaque 1280×720 picture comes out, and that seeking inside
the torrent works.

## reel-player

Playback is deliberately kept out of every other layer.

* **libmpv is `dlopen`ed, never linked.** The build works on machines without
  mpv; at runtime the app probes for `libmpv.so.2` and, when it is missing or too
  old, falls back to spawning an external `mpv` process.
* **`build.rs` resolves the render-parameter enum from the local headers.**
  Those values are not stable across mpv releases — `SW_SIZE..SW_POINTER` are
  17..20 on mpv 0.40+, 18..21 before — and getting them wrong corrupts memory
  silently. Where the headers are unavailable the built-in values are used and
  embedded playback is refused unless the runtime library reports API ≥ 2.5.
* **One thread owns mpv.** It creates, drives and destroys the context, so
  commands, the event queue and rendering never cross threads and no callback can
  fire into freed memory. mpv's documented render/threading rules are easy to
  violate by accident; confining it to one thread makes that structural.
* **Frames are published, not pulled.** The worker renders into a recycled
  buffer (a small pool keeps the steady state allocation-free) and swaps it into
  shared state; the UI uploads it as a texture. An early version removed the
  frame from the shared slot before mpv's blocking render call, which meant
  consumers missed most frames — worth remembering.
* **Software rendering.** `MPV_RENDER_API_TYPE_SW` needs no OpenGL context, so
  it works identically on Wayland, X11, Windows and macOS, and it is measurably
  fast enough (~1.2 ms per 1080p frame). Sharing a texture through the GL render
  API instead would buy 4K headroom at the cost of context interop.

## reel-desktop

* **All engine access goes through a `Backend` trait.** `EngineBackend` runs the
  engine and the HTTP server in-process; `FakeBackend` serves fixtures. That is
  what lets the whole interface be tested headlessly.
* **The UI thread never blocks on the engine.** Slow operations (`add`,
  `pause`, `remove`) are spawned onto a runtime and report back as events, which
  the UI drains each frame. Metadata reads are synchronous because they are
  quick.
* **Text is real widgets, not painted glyphs.** Titles were originally drawn with
  `Painter::text`, which made them invisible to accessibility tools — and the
  first version of the tests could not find them. They are `Label`s now.
* **Posters are generated, not faked.** Until the metadata layer exists, each
  title maps to a stable hue derived from a hash, drawn behind its initials.
  This is honest about the absence of real artwork while keeping the grid
  readable.

## Where the catalog goes next

The engine deliberately knows nothing about discovery. Planned layering:

```
reel-core        engine (done)
reel-http        API (done)
reel-player      native playback (done)
reel-desktop     native UI (done)
reel-catalog     TMDB metadata + poster cache + search-backend trait   <- next
```

`reel-catalog` maps a `TorrentView` (or a magnet) to a catalog entry: title,
year, real artwork, synopsis, rows, and a "continue watching" list. Search
backends sit behind a trait so the project ships no indexers of its own and any
source is pluggable.

Playback can then grow in-window subtitles (mpv renders them into the frame
today, which costs a little CPU) and a transcoding step for clients that cannot
take the original bytes.

