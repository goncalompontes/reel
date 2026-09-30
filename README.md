# reel

A native desktop client for streaming torrents, with a Netflix-shaped catalog.

`reel` is a Rust workspace: a torrent engine, an HTTP streaming server, a
libmpv-backed player, a metadata/artwork catalog, a CLI, and an **egui desktop
app that renders with wgpu directly to the window — no webview, no browser**.
Video is decoded by libmpv and uploaded as a texture, playback starts after a
few pieces rather than after the whole file, and titles are matched against a
metadata provider so the library shows posters and synopses instead of release
names.

**Status: engine, streaming, player, catalog, search and desktop app all work
end to end.** Exactly one search source is bundled — the Internet Archive, which
serves public-domain and Creative Commons film — and adding your own is a
documented, tested extension point: see
[`docs/ADDING_A_SOURCE.md`](docs/ADDING_A_SOURCE.md).

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
| Desktop app: catalog rows, hero banner, detail page, player, settings | done |
| CLI (`serve`, `add`, `ls`, `play`, `rm`, `create`) | done |
| Rate limits, pause/resume, delete with/without files | done |
| Torrent creation + seeding your own content | done |
| TMDB metadata: posters, backdrops, synopsis, genres, rating, runtime | done |
| Title matching that prefers a miss over a wrong match | done |
| Watch history, "continue watching", resume playback | done |
| On-disk metadata and artwork cache | done |
| Search screen + pluggable sources | done |
| Bundled sources | one: the Internet Archive (public domain / CC film) |
| In-window subtitles, transcoding for odd codecs | not yet |

## Install

```bash
git clone <this repo> && cd reel
scripts/install.sh
```

That builds the release binaries and installs them into `~/.local`, which needs
no root and is already on the `PATH` of most desktop Linux setups. It writes:

| Path | What |
| --- | --- |
| `~/.local/bin/reel` | the CLI and daemon |
| `~/.local/bin/reel-desktop` | the app |
| `~/.local/share/applications/reel.desktop` | launcher entry |
| `~/.local/share/icons/hicolor/{16..256,scalable}/apps/reel.{png,svg}` | icons |
| `~/.local/share/licenses/reel/` | licence texts |

Re-running it is a no-op rather than an error, and there is an undo:

```bash
scripts/install.sh --uninstall        # removes files, keeps your library
```

Your downloads and metadata live in `~/Downloads/reel` and `~/.local/share/reel`
and are never touched by install or uninstall.

### Other ways

```bash
# System-wide, if you would rather it lived in /usr
sudo scripts/install.sh --prefix /usr

# A systemd user unit for the headless daemon (off by default)
scripts/install.sh --with-service
systemctl --user enable --now reel.service

# Straight from cargo, if you only want the binaries and no desktop entry
cargo install --path crates/reel-cli
cargo install --path crates/reel-desktop

# Arch: the same installer, driven by makepkg
cd packaging && makepkg -si
```

`scripts/install.sh --help` lists everything, including `--destdir`, which is
how the PKGBUILD reuses it — the file layout has one source of truth.

## Try it

```bash
# Optional: real posters and synopses. Both a v3 API key and a v4 API token work.
export REEL_TMDB_API_KEY=...

reel-desktop            # engine + streaming server + player + catalog, one process
reel-desktop --demo     # sample library, no engine and no network
```

Without the script, `cargo build --release` and `./target/release/reel-desktop`
work exactly the same.

Then paste a magnet link into **Add**. reel fetches the metadata, picks the
video, starts streaming it while it downloads, and looks up the title so the
card gets a poster. Without an API key everything still works — artwork is
generated from each title instead.

A torrent can be addressed four ways and all four work: a **magnet URI**, an
**http(s) `.torrent` URL**, a **local `.torrent` file**, or a bare
**40-character info hash**. A magnet shows up as *Resolving magnet…* until its
metadata arrives, and the API stays responsive while it does.

The CLI ships alongside it and shares the same engine:

```bash
# Turn a file you own into a torrent, then seed it.
reel create ~/Videos/holiday.mp4
reel serve --dir ~/Videos --overwrite --add ~/Videos/holiday.mp4.torrent

# Or just play something.
reel add 'magnet:?xt=urn:btih:...'
reel ls
reel play 0          # launches mpv on the stream URL
```

## Verify it

```bash
cargo test --workspace     # 147 tests: engine, ranges, matching, UI, catalog, search, backend
bash scripts/e2e.sh        # 57 assertions: create, seed, stream, decode, magnet, verify bytes
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
* **a bare magnet resolves its metadata from the swarm and streams** (step 23):
  steps 1-22 all use a `.torrent` file, so the other half of `AddSource` — parse
  the URI, fetch metadata from a peer, then stream — is covered too,
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
  `←`, `↓` and `●` as empty boxes,
* a snapshot with real PNG posters on disk proves the artwork path end to end:
  cached file → image loader → decode → texture → painted into a card.

The catalog is verified separately by `crates/reel-catalog`:

* `matching` is unit-tested against the cases that actually go wrong — sequels,
  remakes, off-by-one years, typos, punctuation,
* `tests/tmdb_stub.rs` drives the **real** client against a stub server, covering
  query building, both auth styles, candidate scoring end to end, artwork
  downloads landing in the cache, clean misses, and a cached lookup still
  working with the network gone.

Search sources are covered the same way: `archive_org`'s parsing is unit-tested
against fixtures with the field-type inconsistencies the real API produces, plus
an `#[ignore]`d test that searches the live Archive and checks the torrent URL it
produces actually resolves.

There is also an `#[ignore]`d test that talks to the **real** TMDB API:

```bash
# No key needed: checks that an invalid key produces the 401 we expect.
cargo test -p reel-catalog --test tmdb_stub -- --ignored --nocapture

# With a key, it also resolves a film end to end, artwork included.
REEL_TMDB_API_KEY=... cargo test -p reel-catalog --test tmdb_stub -- --ignored --nocapture
```

## Architecture

```
crates/reel-core      engine, media/title detection, wire model   (no HTTP, no UI)
crates/reel-http      axum API + range streaming + SSE
crates/reel-player    libmpv playback, frames as RGBA             (no UI toolkit)
crates/reel-catalog   metadata, artwork cache, watch history, search (no UI)
crates/reel-cli       the `reel` binary: daemon + HTTP client
crates/reel-desktop   the native app: egui UI over the above
```

Dependencies only ever point downwards:

```
reel-desktop ──> reel-player ──> libmpv (dlopen)
      │
      ├──────> reel-http ──> reel-core ──> librqbit
      ├──────> reel-catalog ──> reel-core (release-name cleaning only)
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
* **Metadata enrichment runs in the background.** At most two lookups are in
  flight at once, each title is attempted once per session, and results arrive
  as events, so a library of two hundred titles does not stall the first frame.
* **Watch positions are keyed by info hash**, not torrent id, so they survive a
  restart, and writes are throttled to a few seconds.

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
* **Content and sources.** This is a general-purpose BitTorrent client and HTTP
  server. Use it for content you own or are licensed to distribute. Exactly one
  search source is bundled, chosen because it indexes only material anyone may
  share; anything else you add is your decision and your responsibility.
* **The bundled source needs a swarm.** Internet Archive torrents carry
  BEP-19 HTTP web seeds, which librqbit does not implement, so playback depends
  on the Archive's own seeders. Popular items are well seeded; obscure ones may
  not be. Web-seed support would fix this properly and is the most valuable
  engine feature left.
* **Matching.** Title matching prefers "no match" over a wrong match, so some
  films simply will not be found. That is deliberate: a confidently wrong poster
  is worse than generated artwork. If a title is wrong, the fix belongs in
  `reel-catalog/src/matching.rs`, and there are tests there that show the shape.
* **Artwork aspect.** TMDB's standard shapes (2:3 posters, 16:9 backdrops) are
  assumed when cropping artwork to fit, so an unusual image would be cropped
  slightly off-centre rather than distorted.

## Requirements

Rust 1.86+ (edition 2024). Linux, macOS and Windows are supported by the stack;
Linux/Wayland is what has been exercised. `mpv`/libmpv is optional — without it
the desktop app falls back to handing playback to an external `mpv` process, and
the CLI still works.

## License

MIT OR Apache-2.0.
