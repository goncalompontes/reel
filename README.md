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
| Per-file selection: watch one episode of a season pack | done |
| Film vs series detection, season/episode parsing | done |
| Series view with episode list, titles and thumbnails | done |
| Multi-season packs grouped by season | done |
| Daily shows matched by air date | done |
| Per-episode resume positions | done |
| On-disk metadata and artwork cache | done |
| Search screen + pluggable sources | done |
| Bundled sources | one: the Internet Archive (public domain / CC film) |
| Settings file as the canonical configuration (not env) | done |
| Merge copies of a film or show into one title | done |
| Temporary streaming: memory first, spills to scratch, freed on stop | done |
| Streams only the window around the playhead (patched librqbit) | done |
| Never seeds — upload is compiled out and the session flag is set | done |
| Downloads are per item, with a Downloaded state and removal | done |
| Library survives independently of the torrent session | done |
| Subtitles: embedded tracks, sidecar files, track and delay controls | done |
| Audio track, speed and aspect controls; fullscreen + shortcuts | done |
| Transcoding for odd codecs | not yet |

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
| `~/.local/share/applications/reel.desktop` | launcher entry, with an **absolute** `Exec` |
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

The launcher entry gets an absolute path substituted in for exactly this reason:
a GUI session does not read your shell rc files, so `~/.local/bin` is typically
*not* on its `PATH`. An entry whose `Exec` cannot be found fails completely
silently — no window, no message, nothing in the journal. `scripts/e2e.sh` step
24 checks the installed entry names a binary that exists.

## Try it

```bash
reel-desktop            # engine + streaming server + player + catalog, one process
reel-desktop --demo     # sample library, no engine and no network
```

Everything is configured in **Settings**, which is saved to
`~/.local/share/reel/settings.json` and is the canonical source of truth. The
metadata key, download folder, streaming-vs-downloading default, subtitle
preferences and volume all live there and take effect without a restart (the
download folder on the next start). `REEL_TMDB_API_KEY` still works as a
first-run fallback, but a saved key always wins.

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
# `--seed` is required to upload; without it reel never serves pieces. The
# desktop app always runs without it.
reel serve --seed --dir ~/Videos --overwrite --add ~/Videos/holiday.mp4.torrent

# Or just play something.
reel add 'magnet:?xt=urn:btih:...'
reel ls
reel play 0          # launches mpv on the stream URL
```

## Verify it

```bash
cargo test --workspace     # ~190 tests: engine, ranges, matching, UI, catalog, search, backend, player tracks
bash scripts/e2e.sh        # 71 assertions: create, seed, stream, decode, magnet, one-episode, packaging
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
`crates/reel-desktop/tests/`. `ui_scenarios.rs` drives the **real widget tree**
through AccessKit — find a button by label, click it, type, press keys, step
frames — which is how interaction bugs become scripted tests; see
[`docs/DEBUGGING_UI.md`](docs/DEBUGGING_UI.md) for how to write one. The rest:

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

## Getting metadata from TMDB

Everything works without this — search, streaming, and a catalog with artwork
generated from each title. A key is what turns that into real posters, synopses,
episode titles and thumbnails.

**What you need: a free TMDB account.** Nothing else. No key is needed for image
downloads or for the bundled Internet Archive search source.

1. Make an account at `themoviedb.org`.
2. Settings → API → request a key. Either type works:
   * a **v3 API key** — a short hex string, sent as `?api_key=`;
   * a **v4 API Read Access Token** — a JWT starting with `eyJ`, sent as a
     `Bearer` header. The app detects which you pasted.
3. Put it in the app: **Settings → Metadata API key**, then **Check**.

The key is stored in `~/.local/share/reel/settings.json` with mode `600` (it is a
credential), and takes effect immediately — no restart.

`REEL_TMDB_API_KEY` also works, but only as a **first-run fallback**: once a key is
saved in Settings it wins. That is deliberate, and the reason is worth knowing: a
desktop app is started by a launcher, and a launcher does not read shell rc files,
so a variable exported from `.zshrc` is simply absent for anyone clicking an icon.
That is the same trap as a launcher entry that depends on `PATH`.

The metadata provider is TMDB. IMDb has no official public API, so "the IMDb
key" is the TMDB key this app uses; the Settings label says so.

If you use your own key, note TMDB's terms ask for attribution: *"This product
uses the TMDB API but is not endorsed or certified by TMDB."*

## Films and series look different

There is no standard for release names, so this is best-effort by construction.
Parsing is delegated to [`hunch`](https://crates.io/crates/hunch), a pure-Rust
descendant of `guessit`, after comparing its output against the shapes this app
meets. Writing that by hand would mean re-deriving a decade of accumulated edge
cases: `S01E02`, `1x02`, `Season 1 Complete`, `S01E01E02`, anime's absolute
numbering, daily shows, and the tags that must *not* be mistaken for any of them.

What the app adds on top is the torrent-level judgement, because a torrent is a
*set* of files:

* **The files are parsed together.** Three names differing only in a digit are a
  series in a way no single name is — `Some.Show.Disc1/2/3.mkv` parses as a film
  called "Some Show Disc1" one at a time, and as episodes 1, 2, 3 together.
* **Series or film?** Numbered episodes decide it. Failing that, one video is a
  film and several are a series — except for the very common case of a feature
  plus a sample, which stays a film.
* **Extras are separated.** Behind-the-scenes and previews are listed, but never
  numbered alongside episodes.
* **hunch reports confidence**, and a low-confidence guess is treated as one
  rather than as fact.

A **series** gets an episode list: thumbnail, `S01E03`, the episode's real title
from TMDB, its runtime and air date, the file backing it, its size, whether it is
being fetched, and a Play button. A **film** gets its files, as before. Both show
the release's own attributes, which are extra information the torrent already
carried: resolution, source, codec and release group.

Metadata comes from TMDB's **TV endpoints** for a series — search, then the show,
then the season — and only the thumbnails for episodes the torrent actually holds
are downloaded, since a season is twenty-odd images and a torrent is usually a
few.

**Multi-season packs** are grouped by season. Each file keeps the season its own
name says it belongs to, every season the torrent holds is looked up, and the list
gets a heading per season. A torrent of seasons 1-3 is one entry, three headings.

**Daily shows** number by air date rather than by episode:
`The.Daily.Show.2024.01.15.1080p` has no `S01E02` to find. The date in the name is
matched against the provider's episode list, which is what turns it into
`S29E06 January 15, 2024`. Finding *which* season a date belongs to uses the
season list TMDB returns, so only the one relevant season is fetched rather than
all twenty-nine.

### What this cannot do

* **Anime absolute numbering.** `[Group] Show - 12` is read as episode 12 with no
  season, which is usually right but cannot be mapped to a season without
  knowing the show's cour layout.
* **Multi-episode files.** `S01E01E02.mkv` is one file; it is treated as the
  first of the two, because one file cannot be two entries in a list.
* **A date that matches nothing.** If the provider has no episode with that air
  date the row shows the date instead of a code, and is not guessed at.
* **A wrong guess is possible.** When parsing fails or the provider disagrees,
  the file list is the fallback, and it is always accurate.

## Watching one episode of a season pack

**A torrent with more than one playable file is added paused**, so a season pack
waits for you to choose an episode instead of fetching all of it. A film has
nothing to choose, so it starts on its own. (The CLI keeps pausing off by
default, because it is also used for seeding.)

Pressing play on one file **resumes the torrent and narrows the fetch to that
file**; the other episodes keep whatever they had and stop being requested, and
matching sidecar subtitles come along. The detail page shows a checkbox per file,
the bytes fetched per file, a **Download** button per file or episode, a
**Download season** button per season, and a *Fetch every file* button, so the
choice is visible and reversible.

Downloading is **optional and explicit**. The default is to stream: a title is
fetched only while you watch it, kept in memory (spilling to a scratch file when
it outgrows the budget) and **thrown away when playback stops**. **Download** on
a version or episode keeps it in the download folder instead, and the button
becomes **Stop download**, which deletes it again. **Settings → Streaming and
downloads → Stream on demand** turns the whole default off, so new titles are
downloads from the start. See *Streaming is temporary* below.

Selection is at **piece granularity**, which is a property of BitTorrent, not a
shortcut: a piece that straddles a file boundary belongs to both files, so a
neighbour can pick up one piece of bleed. Measured on a three-episode fixture:

```
S01E02  selected  fetched 719730 of 719730   <- the one being watched
S01E03  skipped   fetched  15218 of 719730   <- one boundary piece
S01E01  skipped   fetched   2332 of 719730
```

A season pack has gigabytes per episode against the same 2 MiB pieces, so the
same overlap is a rounding error. It only looks large when files are *smaller*
than a piece, which is why the test sets a 16 KiB piece length rather than
pretending the effect does not exist.

Two things resume, and they resume independently:

* **Where you were watching, per episode.** Positions are stored per file, not
  per torrent: a series is one torrent with many episodes, and one position for
  all of them would resume episode one at episode two's timestamp. Each episode
  row shows its own resume point, and *Continue watching* points at the newest.
* **What you had already downloaded.** The session is persisted by default, so a
  part-fetched episode comes back as a part-fetched episode and continues, not
  as an empty file. Verified by stopping a throttled download partway,
  restarting, and checking both that progress was retained and that the retained
  bytes matched the source.

## Streaming is temporary

Pressing **Play** streams the file without keeping it. The title lands in the
library, the bytes do not.

* **It never seeds.** Uploading is compiled out of the engine and the session
  flag is set, so reel cannot serve pieces to anyone. Only the CLI's explicit
  `reel serve --seed` turns it on, for content you own.
* **Only the window around the playhead.** librqbit selects files, not byte
  ranges, so upstream would fetch a whole episode even for a stream. A small
  vendored patch (`vendor/librqbit`) makes a stream-only torrent fetch **only the
  window around the playhead** — never the beginning of a file you resumed
  halfway through, and never the whole file. Seeking moves the window.
* **Storage.** A streamed torrent uses temporary storage: pieces stay in RAM up
  to a budget (512 MiB by default) and spill, whole files at a time, to a scratch
  directory under the system temp dir. Nothing is written to your download
  folder, and the scratch is deleted when the stream is released.
* **Nothing survives a crash either.** Spill files are unlinked as soon as they
  are created, so the bytes live only while the process holds them: even
  `kill -9` frees the space. Empty scratch directories left by a dead process are
  swept on the next start.
* **Why not a pure RAM cache.** librqbit's reader trusts the chunk tracker's
  have-bit and reads storage directly, so an evicted piece is a hard stream
  error; a bounded cache that evicts is unsafe for a seeker. Keeping whole files
  while they fit, and spilling the rest, is bounded *and* seek-safe.
* **Released on stop.** When you leave the player, a title that is not being
  downloaded is removed from the engine and re-added paused, so its temporary
  storage is freed. The library keeps the title; playing it again brings it back
  from the saved `.torrent` (offline, instantly) and the player waits for it.
* **Downloading is per item.** There is one way to download — a version, an
  episode, or a whole season — and **Download** / **Stop download** toggle it
  on that row. The backend keeps the set of files you asked for, and manages
  storage, re-adding and cleanup behind that. Downloading one episode of a pack
  keeps that episode and nothing else; playing another episode does not cancel
  what is already being kept.
* **A clear download state.** Each row carries its own progress bar while it
  downloads, and turns into **Downloaded** with a *Remove download* button once
  the file is complete — not a forever "Stop download" at 100%.
* **Replay is instant in the same session.** Leaving the player pauses the
  stream instead of discarding it, so pressing play again resumes from the
  buffer. **Settings → Playback → Session cache (MB)** bounds how much is kept
  across titles (default 1 GiB); the least recently watched streams are released
  when it overflows, and `0` releases each one as soon as playback stops.
* **The library is durable.** Since a streamed torrent is removed when it stops,
  a persisted `library.json` (plus a saved `.torrent` per title) is what makes a
  title stay in the library. Downloads are re-added at startup; streams wait
  until you play them.

## One title, several torrents

Adding two releases of the same thing should not produce two unrelated cards.
reel groups torrents into one **work** — one film, or one series — automatically.

**What merges.** Two torrents are the same work when they share a metadata id
(both resolved to TMDB `603`), or, with no metadata, when their normalised title,
a compatible year and the same kind agree. Kind is part of the identity, so the
*Fargo* film and the *Fargo* series never merge. A torrent whose lookup failed
joins a metadata group only when exactly one group matches and the years do not
conflict: a slightly split library is better than one that claims two different
films are the same.

**A film** gets a **Versions** list, best copy first — resolution, then source
(Remux/Blu-ray/WEB), then size. Each copy has its own Play, Download and Remove,
so a 4K and a 1080p copy are a choice rather than a duplicate. The card and the
title's Play button use the best copy.

**A series** merges its episode lists. Episodes are matched by `SxxEyy`, or by
air date for daily shows, so two torrents that overlap on a few episodes become
one row with a **copy chooser** on it; the best copy is selected by default.
Seasons held in different torrents become one season list, and *Download season*
fetches the best copy of each episode across whichever torrent holds it.

The whole merge is a setting: **Settings → Merge copies of the same film or show
into one title**, on by default. Turn it off to list every torrent separately.

Removing is explicit: **Remove** opens a dialog listing every source with a tick
box, so you can drop one bad copy without touching the rest, and optionally
delete its files. Each row in Versions and Sources also has its own Remove.

## Subtitles and player controls

The player reads the current file's track list from libmpv. Embedded subtitles
work out of the box; sidecar files (`.srt`, `.ass`, `.vtt`, …) that the torrent
holds next to the video are matched by episode code or name, added to the fetch
selection, and registered with the player automatically. **Settings → Playback**
turns subtitles on by default and sets a preferred language.

In the player, the second control row has:

* a **CC** menu — subtitles on/off, every subtitle track, and `±0.1s` timing;
* an **audio** menu when the file has more than one track;
* a **speed** menu (0.5×–2×) and an **aspect** menu (auto, 16:9, 4:3, 21:9, 1:1);
* a **fullscreen** button.

Keyboard shortcuts: `space` play/pause, `←`/`→` back 10s / forward 30s,
`↑`/`↓` volume, `F` fullscreen, `Esc` leave fullscreen (or the player). In
fullscreen the chrome fades out after two seconds without input and returns as
soon as the pointer or a key moves.

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

* **Logs.** A launcher starts the app with no terminal, so stderr goes nowhere;
  the app also writes `~/.local/share/reel/reel.log` (truncated each start) for
  exactly this reason.
* **Compressing the stream cache would not pay.** The bytes being cached are
  already-compressed video; compressing them again costs real CPU and adds
  latency to the very replay the cache exists to make instant, for almost no
  space saved. The cache is bounded by size instead (above).
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
* **A stream request waits, it does not fail.** If the pieces a player asks for
  never arrive — a paused torrent, a dead swarm — the HTTP response simply
  blocks rather than erroring, because that is what a buffering player wants.
  Clients should set their own timeout; a request can outlive the reason it was
  made.
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
