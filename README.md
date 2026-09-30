# reel

Stream torrents over HTTP, with a Netflix-shaped catalog on top.

`reel` is a native (Rust) torrent engine plus a range-request streaming server.
It is the core of a desktop streaming client: it can start playing a file from a
torrent after downloading a few pieces, and it exposes a JSON API that a GUI, a
CLI, or a browser can drive.

**Status: milestone 1 complete.** The engine and the streaming server work
end-to-end and are covered by an integration test ([`scripts/e2e.sh`](scripts/e2e.sh)).
The catalog UI, TMDB metadata and search backends are the next milestones.

---

## What works today

| Area | State |
| --- | --- |
| Torrent engine (DHT, trackers, uTP/TCP, magnet + `.torrent`) | done |
| Sequential on-demand streaming of a single file | done |
| HTTP `Range` requests (seek while downloading) | done |
| Media detection ("only download the video, skip the sample") | done |
| JSON control plane + live SSE updates | done |
| CLI (`serve`, `add`, `ls`, `play`, `rm`, `create`) | done |
| Rate limits, pause/resume, delete with/without files | done |
| Torrent creation + seeding your own content | done |
| Netflix-style catalog UI, TMDB, search backends | not yet |
| Desktop shell (Tauri) and native player surface | not yet |

## Try it

```bash
cargo build --release

# 1. turn a file you own into a torrent
./target/release/reel create ~/Videos/holiday.mp4

# 2. seed it (on this machine or another one)
./target/release/reel serve --dir ~/Videos --overwrite --add ~/Videos/holiday.mp4.torrent

# 3. stream it from a second instance, seeking while it downloads
./target/release/reel serve --api-addr 127.0.0.1:3042 --dir ~/Downloads/reel \
    --peer 127.0.0.1:51413 --add ~/Videos/holiday.mp4.torrent

# 4. play it
./target/release/reel ls
./target/release/reel play 0          # launches mpv on the stream URL
```

Or with a magnet link and nothing local:

```bash
./target/release/reel serve --dir ~/Downloads/reel
./target/release/reel add 'magnet:?xt=urn:btih:...'
curl -s localhost:3030/api/torrents | jq '.[0].files[0].stream.url'
# -> http://127.0.0.1:3030/stream/0/0/Movie.mkv
mpv 'http://127.0.0.1:3030/stream/0/0/Movie.mkv'
```

While a stream is served, `reel serve` prints a small catalog page at
`http://127.0.0.1:3030/` for quick sanity checks.

## Verify it

```bash
cargo test --workspace     # unit tests (range parsing, media detection, ...)
bash scripts/e2e.sh        # full integration test: creates, seeds, streams, verifies bytes
```

`scripts/e2e.sh` generates a video, seeds it from one instance with a throttled
upload, streams ranges out of a second instance, and asserts — among other
things — that:

* a 64 KiB range returns `206` with the exact source bytes, after only ~2 MiB of
  a 7.5 MiB file had been downloaded (i.e. it really streams),
* mid-file and suffix ranges work (so seeking works),
* `ffprobe` can demux the stream over HTTP while the download is still running,
* the full download is byte-identical to the original (sha256),
* unsatisfiable ranges return `416` and unknown ids return `404`.

## Architecture

```
crates/reel-core    engine + model   (no HTTP, no UI)
crates/reel-http    axum API + range streaming
crates/reel-cli     the `reel` binary
```

The split exists so the future desktop app can embed `reel-core` and
`reel-http` in-process while reusing the exact same API the CLI talks to.
See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## HTTP API

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/api/health` | version, download dir |
| `GET` | `/api/config` | effective engine configuration |
| `GET` | `/api/torrents` | every torrent with files and stats |
| `POST` | `/api/torrents` | add one (JSON, or raw `.torrent` bytes) |
| `GET` | `/api/torrents/{id}` | one torrent |
| `DELETE` | `/api/torrents/{id}?files=true` | forget (optionally delete files) |
| `GET` | `/api/torrents/{id}/files` | file list with stream targets |
| `PUT` | `/api/torrents/{id}/files` | choose which files to download |
| `GET` | `/api/torrents/{id}/stats` | progress, speeds, peers, ETA |
| `POST` | `/api/torrents/{id}/pause` / `/resume` | pause / resume |
| `GET` | `/api/events` | SSE snapshot once per second |
| `GET` | `/stream/{id}/{file_id}[/{name}]` | the bytes, with `Range` support |

Adding a torrent:

```bash
curl -s localhost:3030/api/torrents -H 'content-type: application/json' -d '{
  "source": "magnet:?xt=urn:btih:...",
  "media_only": true,
  "allow_overwrite": false,
  "initial_peers": ["10.0.0.5:51413"],
  "upload_limit_bps": 1048576
}'

# or upload a .torrent file
curl -s --data-binary @movie.torrent -H 'content-type: application/x-bittorrent' \
  'localhost:3030/api/torrents?media_only=true'
```

## Notes and caveats

* **Security.** The API can add torrents and delete files, so bind it to
  localhost (the default) and do not expose it to the internet. CORS is off by
  default on purpose: a permissive local API is reachable by any web page you
  visit. Use `--cors-origin http://localhost:5173` while developing a web UI.
* **Codecs.** Streaming is byte-exact; it does not transcode. `mkv`/`hevc`
  will play in mpv/VLC but not in a plain browser. A browser-based player will
  need a transcoding step later.
* **Faststart.** For instant playback, the container should have its index at
  the front (`ffmpeg -movflags +faststart`). Without it the engine still works —
  it just fetches the index from the end of the file first.
* **Content.** This is a general-purpose BitTorrent client and HTTP server.
  Use it for content you own or are licensed to distribute (your own media,
  Creative Commons, public domain, Internet Archive, Linux ISOs). It ships with
  no indexers or search backends, and none are hard-coded; if you add any, you
  are responsible for what they return.

## Requirements

Rust 1.86+ (uses edition 2024). Linux, macOS and Windows are supported by the
stack; only Linux has been exercised so far.

## License

MIT OR Apache-2.0.
