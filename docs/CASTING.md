# Casting: playback target — this machine or a renderer

`reel` plays a torrent either here or on a DLNA/UPnP renderer (the TV). Casting
is a **playback target**, not a one-off hand-off: choose a device and it becomes
the target, and everything played afterwards goes there — like the cast button in
YouTube. Disconnect and playback is local again.

## Targets

reel casts to two kinds of receiver, behind one `CastTarget` trait and one
session/UI:

- **DLNA/UPnP-AV** (smart TVs), the original target: SSDP/mDNS discovery, AVTransport
  SOAP control, DIDL metadata, external subtitle sidecars.
- **Google Cast** (Chromecast, Google TV, Android TV with Cast) and **FCast**,
  via the `reel-cast-gcast` crate (the FCast sender SDK owns the CASTV2/TLS
  protocol and mDNS discovery). The device picker labels each receiver with its
  protocol; casting to a Chromecast reuses the same LAN media server and the
  same conversion pipeline, so a file that needs an MP4/H.264/AAC transcode for a
  TV is prepared the same way here.

The sender SDK is vendored and patched for the Default Media Receiver
(`vendor/fcast-sender-sdk/REEL_PATCHES.md`): without the patches the receiver
rejects the load (`streamType: NONE`), never autoplays, and ignores commands
(`mediaSessionId` sent as a string). The LAN media port must also be allowed
through the host firewall (e.g. `ufw allow from <lan> to any port 41337`), or
the receiver cannot fetch the stream and sits loading forever.

Cast receivers are discovered with a long-lived mDNS browse and merged into the
same device list as the DLNA renderers. A Google Cast receiver is treated
conservatively (MP4/H.264/AAC; HEVC is transcoded) because its codec
capabilities are not queried yet — a follow-up. Mute is emulated (volume to zero)
since the Cast protocol has no mute command.

## Model

`reel-cast` owns a `CastSession` (see `crates/reel-cast/src/session.rs`) that
drives a `CastTarget` (the DLNA implementation in `target.rs`). The desktop
mounts it behind `CastService` (`crates/reel-desktop/src/cast.rs`), which owns
the LAN media server, conversion and discovery.

Playback has **one target at a time**, held in `App::cast_device`
(`crates/reel-desktop/src/ui/mod.rs`): `Some(device)` means a connected
renderer, `None` means local. Every play action funnels through
`App::prepare_play`, which routes to the device when one is connected:

- the title page's Play button, episode and release picks,
- search-result Play, and Up next,
- "Stop & watch here" hands back to the local player with `force_local`, so it
  cannot route straight back to the TV.

Choosing a device connects to it; the local player is closed at that moment
(keeping the stream), so the picture can never keep playing here while the cast
starts. Starting a cast does **not** take over the screen — a mini bar appears
and the full remote is opt-in (see [`CASTING_UI_DESIGN.md`](CASTING_UI_DESIGN.md)).

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
  hand-off position was given.

## Starting a cast reliably

The backend makes a title streamable, not the UI. `Backend::start_cast` takes the
library id **and** the info hash, reports `CastStatus::Preparing` with a
`PreparePhase` at once, starts the fetch, and waits (bounded, generously) for the
title to deliver bytes before probing and handing the URL to the renderer. The
phase tells the UI which step it is on — `Connecting → WaitingForStream →
Checking → Preparing (percent) → Starting` — so a slow prepare is legible rather
than a bare spinner. If it never comes up, the cast becomes `CastStatus::Failed`
with **Try again / Switch device / Watch here**.

Two robustness details matter here: the chosen `CastDevice` is cached, so a
discovery pass that comes back empty cannot fail an in-flight cast; and the wait
treats a torrent that is still `initializing` as progress, not failure.

This replaced a UI-side wait on `BackendEvent::Ready` that could be stranded: a
`set_only_files` that failed while the torrent initialised emitted an error and
no `Ready`, so the pending cast never started and the picture kept playing
locally.

## Two-way hand-off

**This machine → the TV.** When a play is routed to a connected device the UI
builds a `CastTarget` carrying a `start_at`: the local player's live position
when it is open on that file, otherwise the saved watch position. The backend
starts the renderer there, so the TV continues from where you were.

**The TV → this machine.** When a cast ends, `CastService` reports
`BackendEvent::CastEnded` with the last position and duration (from the stop
path, or from the poll loop that watches for a renderer-side stop). The UI
distinguishes:

- a **deliberate** stop — "Watch here" — resumes here with
  `play_file_at(..., position)`, unless the cast had reached the end (within
  30 s), in which case nothing plays.
- a **deliberate disconnect** stops the renderer and clears the connection
  without playing anything.
- the renderer stopping **on its own** — TV went to sleep, network dropped — is
  only reported with a toast; it does **not** yank the user back into playback
  while they are browsing.

The app does not clear a cast's state until the renderer confirms the stop.

## Subtitles

DLNA has no standard subtitle channel, so subtitles are delivered as an external
sidecar the renderer fetches itself. When a cast is prepared and subtitles are
enabled:

- a sidecar file shipped with the title is preferred (matched to the Settings →
  Playback subtitle language when one is set), otherwise
- a text track embedded in the video is extracted once with ffmpeg to a cached
  `.srt` (image tracks such as PGS are skipped).

Either way the `.srt` is served by the LAN media server
(`/media/{token}/subtitle/{name}` with `text/srt`, and a WebVTT sibling with
`text/vtt` for browser-based receivers) and advertised in the DIDL
metadata both as an extra `<res>` and as `sec:CaptionInfoEx`, which is what
LG/Samsung renderers read. The user then picks the track in the TV's own
subtitle menu. The cast remote has a **Subtitles: on/off** control; toggling it
re-casts at the current position, since the choice is part of the metadata.

**Image subtitles (PGS/DVD)** cannot become a text sidecar, and renderers do not
expose a container's image track over DLNA. For those, subtitles are **burned
into a seekable MP4**: a full faststart transcode with the chosen track overlaid
(`overlay=shortest=1`), served from the cache with byte-range support, so the TV
can seek. The trade-off is start-up latency — the file is converted before
playback, reported as "Preparing the video… N%" (the result is cached, so it is
one-time per episode) — because a live stream has no index to seek within.
This means a title with only PGS subtitles is re-encoded when subtitles are
wanted, and passed through untouched when they are not.

**HEVC under-reporting.** Many televisions (LG webOS especially) advertise
`hevc=false` in `ConnectionManager` while playing HEVC fine. Settings → Casting →
*Trust HEVC even when the TV does not advertise it* (on by default) passes HEVC
through instead of re-encoding, so native playback and container subtitles are
kept. Turn it off for a TV that genuinely cannot decode H.265.

## Audio and volume

A file can carry several audio tracks (a dual-audio release, for example), and a
DLNA renderer cannot be told which one to play — it always picks its own. So when
Settings → Playback has an **audio language**, reel picks the track whose
language tag matches and maps it explicitly (`-map 0:<stream>`), which forces a
remux or transcode; the choice is part of the conversion cache key, so switching
tracks re-converts rather than replaying the wrong track. With no preference (or
no matching track) the file is delivered as-is and the renderer chooses.

The remote has a **volume** slider and a mute button when the renderer exposes
`RenderingControl`; a renderer without it simply shows no volume control. Volume
is read from the renderer each poll, so a change made on the TV is reflected here.

**Up next.** The remote can hand the next episode to the renderer ahead of time
(`SetNextAVTransportURI`) so the TV preloads it. This is best-effort: many
renderers ignore it, and a renderer that does simply keeps playing the current
item.

## Hardware encoding

A re-encode is the only expensive part of casting, and on the CPU it is often
slower than realtime for a 4K source. When a file must be re-encoded, reel asks
ffmpeg which hardware encoders the build lists (`ffmpeg -encoders`) and, under
Settings → Casting → *Video encoding* → **Auto**, uses the best one for the
platform (VA-API on Linux, VideoToolbox on macOS, NVENC/Quick Sync/AMF on
Windows). **CPU** forces `libx264`.

Detection is a hint, not a guarantee: a hardware encoder is listed even on a
machine with no matching GPU or driver. Every hardware attempt is therefore
retried once on the CPU if it fails (`reel-media`), so `Auto` can never break a
cast — it only makes a slow one fast when the hardware cooperates. The resolved
encoder is shown under the setting ("Using VA-API (GPU)" / "Using CPU
(libx264)"). The output is always H.264 in an MP4, so the conversion cache key
does not depend on which encoder produced it.

## UI surfaces

Four surfaces, each with one job; the design is in
[`CASTING_UI_DESIGN.md`](CASTING_UI_DESIGN.md).

- **Shell control** (`cast_indicator`): the always-present entry in the rail
  (and the header on pushed views). It shows the device and a live progress ring,
  opens the picker when idle and the remote when connected, and carries
  Switch/Watch here/Disconnect on its context menu. It is hidden while casting is
  turned off (Settings → Casting turns it on and shows its status).
- **Device picker** (`cast_picker`): a modal. Shows what will be cast and where it
  continues from, the nearby devices (the last-used one pinned first), an
  "Add by address" escape hatch, and "Disconnect and play here" when connected.
- **Remote** (`cast_screen`): the full-screen, artwork-led remote — opened on
  demand. Title and metadata, transport and seek, Up next, Watch here, Switch
  device and Disconnect; a phase story while connecting; and Try again / Switch
  device / Watch here on failure. Escape or "Browse" returns to the shell with
  the cast running.
- **Mini bar** (`cast_bar`): the compact strip along the bottom while a cast runs
  and the remote is closed. Never takes over the screen. Shows the title, device
  and position with a progress rail, play/pause, Watch here and expand.

Play controls across the app state the destination: "Play on Living Room TV"
when connected, "Play" when not.

Snapshots: `cast-screen`, `cast-connecting`, `cast-failed`, `cast-bar` and
`cast-picker` in `crates/reel-desktop/tests/snapshots/`. Regression tests:
`crates/reel-cast/tests/control.rs`, and `tests/ui.rs` + `src/ui/mod.rs` for the
routing, the surfaces and the hand-off.

## Known limitations

- **Google Cast subtitles** are delivered as a side-loaded **WebVTT** track (the
  `tracks`/`activeTrackIds` load fields, via the vendored SDK patch); the
  receiver fetches the `.vtt` sibling. DLNA still uses the SubRip sidecar.
- **A file that needs a container remux is remuxed to a seekable file for Cast**
  (not a live fragmented stream), because the receiver derives the media
  duration from what has streamed and would otherwise stop early. This is a
  short wait (a copy) but not instant like the DLNA live remux.
- **Google Cast codec capabilities are not queried.** The SDK reports no
  capabilities for Chromecast, so Cast starts from the MP4/H.264/AAC baseline;
  Settings → Casting → *Trust HEVC* then decides whether HEVC is passed through
  rather than re-encoded.
- **Up next is not preloaded.** `SetNextAVTransportURI` (DLNA) and the Cast
  queue exist in the control layer and are tested, but the UI does not wire them:
  resolving a second item's media URL touches the per-cast live-remux state and
  the torrent's file selection, so it needs a broader refactor than it is worth
  for a hint many renderers ignore. The "Play next on TV" button still casts the
  next episode immediately.
- **DLNA state is polled, not evented.** The session polls transport/position at
  1 Hz rather than subscribing to GENA events. This is deliberate: the verified
  stop/seek logic is built on polling and replacing it risks that robustness.
- **Casting needs ffmpeg.** Without ffmpeg/ffprobe, casting is disabled entirely
  rather than guessing a delivery plan from the file extension.
- **Out of scope:** AirPlay (no mature Rust sender; needs per-device SRP
  pairing), DIAL (launches TV apps, does not cast files), HLS/adaptive ladders,
  and audio-only casting.

## Settings and environment

- Casting is opt-in (Settings → Casting) because it opens a LAN-facing media
  port. The port defaults to a fixed `41337` so a firewall rule can allow it.
- Env vars (`REEL_*`) are first-run fallbacks; `settings.json` wins.
