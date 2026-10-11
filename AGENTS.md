# AGENTS.md

`reel` — native torrent streaming/player. Rust workspace, edition 2024, no CI.

## Commands

```bash
cargo build                                   # debug workspace
cargo build --release                         # what install.sh ships
cargo test --workspace                        # ~190 tests (see gotchas below)
cargo test -p reel-catalog matching::         # one crate, one module
cargo test -p reel-desktop --test ui <name>   # one integration test file
bash scripts/e2e.sh                           # real create->seed->stream; needs ffmpeg ffprobe curl python3
scripts/install.sh                            # user-local (~/.local), no root; --prefix /usr, --uninstall
```

UI snapshot suites: `UPDATE_SNAPSHOTS=1 cargo test -p reel-desktop --test ui -- --test-threads=1`
(same for `--test ui_scenarios` / `--test e2e`). Snapshots live in
`crates/reel-desktop/tests/snapshots/`.

## Gotchas (non-obvious, will bite you)

- **The tree is not rustfmt-clean.** `cargo fmt` rewrites dozens of files. Do
  not run it repo-wide; match the surrounding style, and format only files you
  changed (many of those are already clean).
- **Run the wgpu/snapshot suites with `--test-threads=1`.** They can segfault
  (Mesa/wgpu contention) with default parallelism; single-threaded is reliable.
- `reel-desktop/tests/backend.rs::a_pack_streams_and_downloads_only_the_chosen_episode`
  is flaky under parallel engine load (times out). It passes alone — re-run
  before assuming you broke it.
- Network tests are `#[ignore]`d: TMDB live, ext.to live, and the bundled-source
  search. Run them with `-- --ignored` and expect to need a key/cookie.
- `vendor/librqbit` is a **patched** crate via `[patch.crates-io]` so streaming
  fetches only the window around the playhead. Do not bump `librqbit` without
  porting that patch (`vendor/librqbit/src/{piece_tracker,torrent_state/streaming}.rs`).
- libmpv is `dlopen`ed at runtime, not linked. The player can be unavailable;
  the app falls back to an external `mpv` and the CLI still works.

## Layout

Crates depend only downward — keep it that way:

```
reel-core      engine + wire model (no HTTP, no UI)
reel-http      axum API + range streaming + SSE   -> reel-core
reel-player    libmpv -> RGBA frames (no UI)       -> libmpv (dlopen)
reel-catalog   metadata, artwork cache, search     -> reel-core
reel-cli       `reel` binary: daemon + HTTP client -> reel-http
reel-desktop   egui app                            -> the four above
```

- `reel` = CLI; `reel-desktop` = GUI. The desktop mounts the streaming server
  **in-process** on an ephemeral `127.0.0.1` port; the UI never touches mpv
  directly (`reel-player` owns it on a worker thread).
- Data dir `~/.local/share/reel/`: `settings.json`, `library.json`, `artwork/`,
  `metadata/`, `torrents/`, `reel.log` (truncated each start). `REEL_DATA_DIR`
  overrides the root.
- **`settings.json` is the canonical config.** Env vars (`REEL_TMDB_API_KEY`,
  `EXTTO_COOKIE`, ...) are first-run fallbacks only; a saved setting always wins.

## Desktop tests

`crates/reel-desktop/tests/`:
`ui.rs` renders snapshot PNGs, `ui_scenarios.rs` drives the widget tree through
AccessKit (find by label, click, type), `e2e.rs` is mouse-driven journeys,
`backend.rs`/`app_player_resume.rs` start the real engine + streaming server on
localhost, `glyphs.rs` pins font coverage, `packaging.rs` checks the launcher/icons.

UI is immediate-mode egui. Reuse `crates/reel-desktop/src/components.rs` (type +
spacing scales and shared widgets) instead of ad-hoc sizes/colours. Design notes:
`docs/ARCHITECTURE.md`, `docs/DEBUGGING_UI.md` (headless UI debugging, including a
socket-driven `examples/ui_debug.rs`), and the phase plans `docs/UI_REWORK_PLAN.md`,
`docs/SEARCH_UI_PLAN.md`.
