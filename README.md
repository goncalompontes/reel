# reel

A native desktop client for streaming torrents, with a Netflix-shaped catalog.

`reel` is a Rust workspace: a torrent engine, an HTTP streaming server, a
libmpv-backed player, a CLI, and an **egui desktop app that renders with wgpu
directly to the window — no webview, no browser**. Video is decoded by libmpv
and uploaded as a texture, and playback starts after a few pieces rather than
after the whole file.

**Status: engine, streaming, player and desktop app all work end to end.**
Catalog metadata (TMDB artwork, search backends) is the next milestone.

---

## What works today

| Area | State |
| --- | --- |
| Torrent engine (DHT, trackers, uTP/TCP, magnet + `.torrent`) | done |
| Sequential on-demand streaming of a single file | done |
| HTTP `Range` requests (seek while downloading) | done |
| Media detection ("only download the video, skip the sample") | done |
| JSON control plane + live SSE updates | done |
| Native playback (libmpv, RGBA frames into an egui texture) | done |
| Desktop app: library grid, hero banner, detail page, player, settings | done |
| CLI (`serve`, `add`, `ls`, `play`, `rm`, `create`) | done |
| Rate limits, pause/resume, delete with/without files | done |
| Torrent creation + seeding your own content | done |
| TMDB metadata, real posters, search backends | not yet |
| In-window subtitles, transcoding for odd codecs | not yet |

## Try it

```bash
cargo build --release

# The desktop app: engine + streaming server + player, all in one process.
./target/release/reel-desktop

# No engine or network, just the sample library (good for a quick look):
./target/release/reel-desktop --demo
```

Then paste a magnet link into **Add**. reel fetches the metadata, picks the
video, and starts streaming it while it downloads.

The CLI is still there and shares the same engine:

```bash
# Turn a file you own into a torrent, then seed it.
./target/release/reel create ~/Videos/holiday.mp4
./target/release/reel serve --dir ~/Videos --overwrite --add ~/Videos/holiday.mp4.torrent

# Or just play something.
./target/release/reel add 'magnet:?xt=urn:btih:...'
./target/release/reel ls
./target/release/reel play 0          # launches mpv on the stream URL
```

## Verify it

```bash
cargo test --workspace     # 56 tests: range parsing, media/title detection, ABI, UI, backend
bash scripts/e2e.sh        # 51 assertions: create, seed, stream, decode, verify bytes
```

`scripts/e2e.sh` is the real proof. It generates a video, seeds it from one
instance with a throttled upload, and streams out of a second instance that must
fetch the data from the first. It asserts, among other things, that:

* a 64 KiB range returns `206` with byte-exact content after only ~2 MiB of a
  7.5 MiB file had been downloaded,
* prefix, mid-file and suffix ranges are all exact, `416` on unsatisfiable,
  `404`/`400` on bad input,
* **libmpv decodes a real 1280×720 picture out of the live torrent stream**
  (step 17) and can seek to 5 s inside it (step 18),
* **the desktop app's own video surface renders that stream** (step 19): the
  same `PlayerController` the UI uses decodes frames and uploads them to egui as
  textures, with no window and no display server,
* `ffprobe` can demux the stream while it is still downloading,
* the full download is byte-identical to the source.

The desktop app is verified separately, without a display server, by
`crates/reel-desktop/tests/`:

* `ui.rs` drives the real widget tree through AccessKit (`egui_kittest`) and
  renders the library and detail pages to PNG snapshots in
  `crates/reel-desktop/tests/snapshots/`,
* `backend.rs` starts the real engine and streaming server and talks to it over
  a real localhost socket,
* `glyphs.rs` pins the icon font coverage, because egui's bundled fonts render
  `←`, `↓` and `●` as empty boxes.

## Architecture

```
crates/reel-core      engine, media/title detection, wire model   (no HTTP, no UI)
crates/reel-http      axum API + range streaming + SSE
crates/reel-player    libmpv playback, frames as RGBA             (no UI toolkit)
crates/reel-cli       the `reel` binary: daemon + HTTP client
crates/reel-desktop   the native app: egui UI over the above
```

Dependencies only ever point downwards:

```
reel-desktop ──> reel-player ──> libmpv (dlopen)
      │
      ├──────> reel-http ──> reel-core ──> librqbit
      └──────> reel-core
```

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the reasoning.

## How the desktop app fits together

* **Engine and streaming server run in-process**, on an ephemeral localhost
  port chosen by the OS. The player is handed ordinary `http://127.0.0.1/stream/…`
  URLs, so playback goes through exactly the same range-request path the CLI and
  the integration tests exercise.
* **The UI never touches mpv.** `reel-player` runs a worker thread that owns
  libmpv end to end, renders frames into memory, and publishes them; the UI
  uploads the latest one as a texture. That keeps mpv's threading rules and
  teardown crashes away from the event loop.
* **No video is ever handed to a browser.** eframe renders with wgpu (Vulkan on
  this machine) into the OS window.

## Notes and caveats

* **Security.** The streaming API is bound to `127.0.0.1` and CORS is off by
  default; the API can delete files, so do not expose it.
* **Codecs.** Streaming is byte-exact, it does not transcode. libmpv plays
  essentially anything, so `mkv`/HEVC are fine — but this is why an embedded
  browser engine would not have been a substitute for a real player.
* **Performance.** libmpv's software renderer costs ~1.2 ms per 1080p frame
  (~2 ms with dense subtitles) against a 16.7 ms budget; 1080p60 plays in
  realtime. The OpenGL render API is the upgrade path if 4K60 with subtitles
  becomes the target.
* **Faststart.** For instant playback the container should have its index at the
  front (`ffmpeg -movflags +faststart`); otherwise the engine still works, it
  just fetches the index from the end of the file first.
* **Content.** This is a general-purpose BitTorrent client and HTTP server. Use
  it for content you own or are licensed to distribute. It ships with no
  indexers and none are hard-coded.

## Requirements

Rust 1.86+ (edition 2024). Linux, macOS and Windows are supported by the stack;
Linux/Wayland is what has been exercised. `mpv`/libmpv is optional — without it
the desktop app falls back to handing playback to an external `mpv` process, and
the CLI still works.

## License

MIT OR Apache-2.0.
