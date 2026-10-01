# Playback, downloads and settings: plan

This document is the working plan for six reported problems with the desktop
client. It records the intended design so the implementation can be reviewed
against it, and so the "why" survives after the code lands.

> **Status: implemented.** Settings are canonical; Play resumes the torrent and
> fetches only the watched file; per-file/per-episode/per-season Download
> actions exist; the Settings screen edits every option; subtitles, audio,
> speed, aspect and fullscreen are wired through libmpv; and the test suite and
> snapshots are updated. The only deliberate limitation is that librqbit streams
> through on-disk pieces, so "stream only" means "fetch only what you watch",
> not "never write to disk" (see *Out of scope*).

## Problems

1. Individual files / episodes / seasons cannot be downloaded — only the whole
   torrent.
2. Pressing Play on an episode does not start streaming or downloading; nothing
   plays.
3. Downloading should be optional; streaming on demand should be the main way to
   use the app.
4. There is no place in Settings for the metadata API key.
5. Subtitles and related player features are missing.
6. There is no way to go fullscreen.

Plus: **Settings must be the canonical way to configure the app, not environment
variables or shell settings.**

## Root causes found

| # | Cause |
| - | ----- |
| 1 | `set_only_files` exists and the detail page has checkboxes, but there is no explicit download action and no stream-vs-keep distinction. |
| 2 | A multi-file torrent is added **paused**; `App::play_file` narrows the file selection but never resumes the torrent, so the HTTP stream request blocks forever. |
| 2 | Resume position comes from the torrent, not the file, so a series resumes the wrong episode. |
| 3 | The app's default is still "fetch every playable file", and there is no setting to change it. |
| 4 | The key field is hidden whenever `REEL_TMDB_API_KEY` is set, and the env var overrides the stored key. |
| 5 | No track list is read from mpv, external subtitle files are never selected or served, and there is no subtitle/audio/speed UI. |
| 6 | Fullscreen was never wired to the egui viewport. |
| — | `REEL_TMDB_API_KEY`, `REEL_DOWNLOAD_DIR` and `REEL_NO_BUNDLED_SOURCES` are read from the environment; the API key overrides the settings file. |

## Design

### Settings

A single persisted record, `CatalogSettings`, in
`~/.local/share/reel/settings.json` (mode 600), becomes the source of truth:

- `tmdb_api_key` — metadata provider key.
- `download_dir` — where torrent data goes (applies on next start).
- `stream_only` — streaming on demand instead of background downloading
  (default `true`).
- `subtitles_enabled` — whether to turn subtitles on automatically.
- `subtitle_language` — preferred subtitle language (`slang`).
- `default_volume` — starting volume, 0–100.

Precedence: **stored value wins**. Environment variables are only consulted as a
first-run convenience when nothing is stored, so scripts still work but saving in
the app always takes effect. The desktop binary no longer binds
`REEL_DOWNLOAD_DIR`; an explicit `--dir` flag still overrides for one run.

The Settings screen becomes an editor over a draft copy of the settings with a
single **Save**, plus **Check** for the key. The metadata key field is always
visible.

### Streaming vs downloading

`stream_only = true` (default):

- A multi-file torrent is added paused, as today.
- Pressing Play selects only the played file (plus matching sidecar subtitles)
  **and resumes the torrent**, so playback starts immediately.
- Files are fetched only when they are played. **Download** buttons (per file,
  per episode, per season, and "all") are the only way to intentionally keep
  content, and each one selects the files and resumes.

`stream_only = false`: newly added torrents fetch their playable files in the
background, as today.

Resume positions are read and written per file, never per torrent.

### Subtitles and player features

`reel-player` gains:

- `track-list` reading (subtitle and audio tracks), refreshed when the count
  changes or a file loads.
- Subtitle track selection (`sid`), visibility (`sub-visibility`), delay
  (`sub-delay`); audio track selection (`aid`); playback speed (`speed`);
  video aspect override.
- External subtitle loading (`sub-add`) for sidecar files served over the same
  range-request stream.
- Language preferences (`slang`, `alang`) from settings.

The desktop player gains a CC/tracks menu, an audio menu, a speed menu,
subtitle-delay controls and keyboard shortcuts. Matching sidecar subtitle files
are added to the fetch selection and registered with mpv when the video loads.

### Fullscreen

A fullscreen toggle in the controls and the `F` key, `Esc` to leave. In
fullscreen the chrome auto-hides after a few seconds of no input and returns on
pointer movement.

## Verification

- Settings: round-trip, and "stored wins over environment".
- Play: playing a paused multi-file torrent resumes it and narrows to the chosen
  file (and its subtitles).
- Downloads: selecting a season selects exactly that season's files; stopping
  fetch deselects the rest.
- Player: track-list JSON parsing; subtitle/audio selection round-trips.
- UI: settings render the new controls; snapshots refreshed intentionally.
- `cargo test --workspace` and `bash scripts/e2e.sh` stay green.

## Out of scope

- An OMDb/IMDb provider. IMDb has no official public API; TMDB is the provider
  the app integrates, and the settings label says so.

---

# Phase 2: merging torrents into works, and temporary streaming

> **Phase 2 status: merging is implemented** (`reel-catalog/src/work.rs`,
> `rows.rs`, the desktop library/detail screens, and the *Merge copies* setting).
> The temporary-streaming / persisted-library lifecycle below is the next pass.

## Merging

Multiple torrents that are the same film or show become one *work*:

* **Identity.** Same work when both have metadata and share
  `(source, source_id)`, or otherwise when normalized title + compatible year +
  `MediaKind` agree. `kind` is in the key, so the *Fargo* film and the *Fargo*
  series never merge. A metadata-less torrent folds into a metadata group only
  when exactly one group matches and the years do not conflict.
* **Films** get a `Version` list (torrent + feature file + attributes + size),
  ordered best-first by resolution, then source, then size.
* **Series** merge episode lists keyed by `(season, episode)`, or air date for
  daily shows, so the same episode from two torrents becomes one row with a
  chooser. Distinct seasons from distinct torrents become one season list.
* Settings gets a **Merge duplicate titles** toggle (default on).

This lives in a pure `reel-catalog/src/work.rs`; the desktop backend supplies
`primary_file_id` and the UI renders works.

## Temporary streaming and optional downloads

Playing must not create a persistent download; downloading is an explicit,
cancellable action.

* **Play** streams from storage that is discarded when playback stops.
* **Download** persists to the download folder and the button toggles to
  cancel/remove.

### The storage constraint

librqbit 9's `FileStream::poll_read` trusts the chunk-tracker have-bit and reads
storage directly; a missing piece is a hard stream error, and the tracker is not
reachable from outside the crate. A bounded in-memory buffer that **evicts** is
therefore unsafe for a seekable player. The chosen design is **memory-first with
spill**: pieces stay in RAM up to a budget (~512 MiB), then spill to a scratch
file deleted when the stream stops. Bounded, seek-safe, and nothing persists.

### Library lifecycle

Ephemeral streaming requires the library to survive independently of the
session, so a persisted store of sources (magnet/info hash + metadata +
`streaming | downloading`) is introduced; the session then holds only what is
actively streaming or downloading. `Play` ensures a temporary-storage torrent
and removes it on stop; `Download` ensures a filesystem-storage torrent and
keeps it. This is implemented as a separate pass, after merging lands.
