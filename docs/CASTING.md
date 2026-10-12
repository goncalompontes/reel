# Casting: hand-off between this machine and a renderer

`reel` can play a torrent on a DLNA/UPnP renderer (the TV) instead of inside the
app. The point of the feature is that this is a **hand-off**, not a mode switch:
casting continues from where the local player is, and stopping the cast brings
the picture back to this machine.

## Model

`reel-cast` owns a `CastSession` (see `crates/reel-cast/src/session.rs`) that
drives a `CastTarget` (the DLNA implementation in `target.rs`). The desktop
mounts it behind `CastService` (`crates/reel-desktop/src/cast.rs`), which owns
the LAN media server, conversion and discovery.

The renderer is treated as a **state machine whose transitions are verified**,
because DLNA renderers are unreliable: they accept `Stop` and keep playing, keep
one transport position across `SetAVTransportURI`, or fault with `701` while
transitioning.

- `CastSession::stop` sends `Stop` (one retry on a fault), then polls
  `GetTransportInfo` and only reports success — clearing the position — once the
  transport reads `STOPPED`/`NO_MEDIA_PRESENT`.
- `CastSession::start` calls `stop_if_needed` first, so new media is only loaded
  once the previous one is confirmed stopped.
- `CastSession::start_at(url, metadata, position)` starts a cast and seeks to
  `position`; `settle_position` also resets a stale position to 0 when no
  hand-off position was given. This is the resume-across-casts fix; see
  [`CAST_RESUME_REPORT.md`](CAST_RESUME_REPORT.md).

## Two-way hand-off

**This machine → the TV.** When a cast is requested, the UI builds a
`CastTarget` (`crates/reel-desktop/src/ui/mod.rs`) that carries a `start_at`:
the local player's live position when it is open on that file, otherwise the
saved watch position. The target travels through `BackendEvent::Ready` into
`Backend::start_cast(..., start_at)`, and `CastService` hands it to
`CastSession::start_at`, so the TV continues from where you were. When the cast
actually starts, `App::hand_over_to_cast` closes the local player (without
stopping the stream the cast needs).

**The TV → this machine.** When a cast ends — the user pressed Stop, or the
renderer stopped itself — `CastService` reports `BackendEvent::CastEnded` with
the last position and duration (from the stop path, or from the poll loop that
watches for a renderer-side stop). `App::on_cast_ended` resumes locally with
`play_file_at(..., position)` unless the cast looks finished (within 30 s of the
end), in which case nothing plays. The app does **not** clear a cast's state
until the renderer confirms the stop.

## UI surfaces

- **Cast picker** (`cast_picker`): chooses a renderer. It shows what will be cast
  and where it continues from ("Continues from 56:40"), so the hand-off is never
  a surprise. Devices can also be added by address for networks that drop SSDP.
- **Cast bar** (`cast_bar`): the primary surface. A compact strip along the
  bottom shows the title, position and device, a slim progress rail, and
  Play/Pause, Stop and Remote. It carries the connecting state inline (with a
  Cancel) instead of taking over the screen.
- **Cast remote** (`cast_overlay`): an opt-in modal with the seek bar and larger
  controls. Closing it keeps the cast running. "Stop & watch here" is explicit
  about the hand-off: it stops the TV and resumes on this screen.

Snapshots: `cast-bar` and `cast-picker` in
`crates/reel-desktop/tests/snapshots/`. Regression tests:
`crates/reel-cast/tests/control.rs`, and `tests/ui.rs` +
`src/ui/mod.rs` for the hand-off and bar.

## Settings and environment

- Casting is opt-in (Settings → Casting) because it opens a LAN-facing media
  port. The port defaults to a fixed `41337` so a firewall rule can allow it.
- Env vars (`REEL_*`) are first-run fallbacks; `settings.json` wins.
