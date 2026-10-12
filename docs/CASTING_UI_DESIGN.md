# Casting UX design — "Playback has a destination"

Status: **implemented**. This documents the casting experience in
`reel-desktop`: the surfaces, the state model, the copy, and the reasoning. The
behavioural contract lives in [`CASTING.md`](CASTING.md).

## 1. Principle

Playback has a **destination**: either *here* or *a device*. Choosing a device
connects to it and everything played afterwards goes there — like the cast button
in YouTube. The UI always states the destination; it never says "mode".

Five rules carried through the implementation:

1. **Destination, not mode.** Label where playback goes, not that casting is on.
2. **Progressive disclosure.** A mini bar by default; a full remote on request;
   settings only when something is wrong.
3. **Optimistic, reversible.** Show "connecting" immediately; keep the last good
   state until the renderer confirms; always offer an undo.
4. **Never strand the user.** A slow prepare shows progress and a **Watch here**
   escape; a failure shows choices, not a dead end.
5. **Tell the truth about the TV.** DLNA is unreliable; surface unreachable,
   position-unavailable, and renderer faults plainly.

## 2. Surfaces

Four surfaces, each with one job. There is no header chip and no floating picker
window; the shell control *is* the entry point.

| Surface | Code | Job |
|---|---|---|
| Shell control | `cast_indicator` | Always-present state + way to connect / open the remote |
| Device picker | `cast_picker` | Choose a device; add by address; disconnect |
| Remote | `cast_screen` | The full-screen, on-demand remote |
| Mini bar | `cast_bar` | Compact control while a cast runs and the remote is closed |

The shell control lives in the rail (always present) and in the header on pushed
views. It is hidden while casting is turned off — Settings → Casting turns it on
and shows a status block.

## 3. State model

`App::cast_device: Option<DeviceInfo>` is the destination (`None` = here). The
surfaces render a derived flow:

```rust
enum CastFlow { Off, Idle, Ready, Connecting, Playing, Failed }
```

derived from `cast_device` and the backend's `CastStatus`:

```rust
enum CastStatus {
    Preparing { device, title, phase: PreparePhase },
    Active { device, title, state, position, duration },
    Failed { message },
}
enum PreparePhase { Connecting, WaitingForStream, Checking, Preparing { percent }, Starting }
```

`PreparePhase` is what turns "Connecting…" into a legible story instead of a
spinner. `position: None` (renderers without `GetPositionInfo`) renders as
"Live · position unavailable" rather than a frozen 0:00.

Routing is unchanged and central: `App::prepare_play` sends a play to
`cast_here` when a device is connected. A hand-back uses `force_local` so it
cannot route straight back to the TV.

## 4. The surfaces in detail

### Shell control

Painted, so the state reads at a glance: a badge glyph (`⧉` connected, spinner
connecting, `✓` ready, `✕` failed) with a **progress ring** around it, and the
device name. Click opens the picker (idle) or the remote (connected). Right-click
gives Switch device / Watch here / Disconnect. Tooltips carry the title and
device.

### Device picker

An `egui::Modal` (the app's existing pattern for Add and Remove). It shows what
will be cast with a poster and "Resumes from 12:34", the nearby devices with the
last-used one pinned first, an "Add by address" escape hatch, and "Disconnect and
play here" when connected. Empty state names the fix ("make sure the TV is on and
on the same network").

### Remote

Opened on demand — **never** automatically. Full screen but built like the app:
the title's **backdrop artwork** behind a scrim, the metadata, a transport row
and seek scrubber shared with the local player, Up next, and the actions
(Watch here, Switch device, Disconnect). States:

- **Connecting**: a 4-step phase story (`Contacting → Buffering → Preparing
  (62%) → Starting`) rather than a bare spinner.
- **Playing / paused / buffering**: transport and scrubber; "Live · position
  unavailable" when the renderer does not report a position.
- **Failed**: the renderer's message with Try again / Switch device / Watch here.

Escape or "Browse" returns to the shell with the cast running.

### Mini bar

The default surface. A compact strip showing the title, device and position with
a progress rail, play/pause, Watch here and expand. It never takes over the
screen; the toast stacks above it.

## 5. Destination affordances

The title page, the hero and the search result state the destination in the Play
label: "Play on Living Room TV" when connected, "Play" when not. Double-clicking a
card while connected casts it. `C` opens the picker (idle) or the remote
(connected), guarded so it never steals a keystroke from a text field.

## 6. Robustness behind the UI

These make the UX honest (see `CASTING.md` for the contract):

- The backend waits for the title to deliver bytes, treating `initializing` as
  progress and reporting a phase; a generous bound, cancellable at any time.
- The chosen `CastDevice` is cached, so an empty discovery pass cannot fail an
  in-flight cast.
- Failure becomes `CastStatus::Failed` with actionable choices; no request can be
  stranded while the picture plays on locally.
- A renderer that stops on its own is reported, not silently resumed locally.
- The last device is remembered so the picker pins it; the connection itself is
  session-only.

## 7. Design system

Tokens come from `theme.rs` and `components.rs`; icons from `icons.rs`. Casting
adds small local widgets in `ui/cast.rs` — `ring` (progress dial), `phase_steps`,
`seek_scrubber` — built on the existing type/spacing/colour scales. Snapshots live
in `crates/reel-desktop/tests/snapshots/`.
