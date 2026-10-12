# Cast: playback resumes across casts (fixed)

Status: **fixed.** The renderer is now treated as a state machine and every
transition is verified: `Stop` is confirmed via `GetTransportInfo` before the
app forgets the cast, new media is only loaded once the transport is stopped,
and a position the renderer failed to reset is diagnosed and corrected. The
diagnosis below is kept for context; the section **Fix** describes what changed.

## Symptom

- Cast episode A, stop it part-way, "end the cast", then cast episode B: **B
  starts at A's position** instead of 0:00.
- "Ending the cast" does not reliably stop playback on the renderer.

## What we know

- Target: **LG webOS TV SM9000PLA**, DLNA `MediaRenderer` at `http://<tv>:1241/`.
- Capabilities (`ConnectionManager.GetProtocolInfo`): 69 `SinkProtocolInfo`
  entries, `hevc=false ac3=false eac3=false mp4=true mkv=true`, so the planner
  chooses **Original** (serves the `.mkv` as-is, no conversion).
- Media URLs already differ per episode:
  `/media/<token>/original/<engine-id>/<file-id>/<name>`.
- Example from `~/.local/share/reel/reel.log` (two consecutive casts):

  ```
  starting a stream id=6 live=false cache_hit=false kept=0 selections=1
  opening the player url=http://127.0.0.1:37375/stream/0/5/The.Bear.S01E06...mkv start_at=None
  renderer capabilities entries=69 hevc=false mp4=true mkv=true
  ...
  starting a stream id=6 live=true cache_hit=true kept=0 selections=1
  opening the player url=http://127.0.0.1:37375/stream/0/6/The.Bear.S01E07...mkv start_at=None
  renderer capabilities entries=69 hevc=false mp4=true mkv=true
  ```

- A direct SOAP probe afterwards showed the renderer `STOPPED`, `CurrentURI` =
  the E07 URL, `RelTime 00:00:00 / TrackDuration 00:20:29` — i.e. in that
  instance it *was* reset. So the resume is intermittent and happens around
  "ending the cast".

## A workaround was tried and reverted (do not repeat)

Appending a unique `?cast=<n>` query to the media URL, an unconditional
`Seek 0` before `Play`, and a Stop retry. Reverted as a hack: it masks the cause
and changes behaviour blindly. Fix the state machine instead.

## Hypotheses (ranked) — 1 and 3 confirmed, 2 covered defensively

1. **Stop is best-effort.** `CastSession::start`
   (`crates/reel-cast/src/session.rs`) sends `Stop` first but **ignores its
   error**. If the renderer is wedged (`TRANSITIONING`) or answers `701`, it
   keeps playing the old media, and the following `SetAVTransportURI` may be
   ignored while `Play` resumes the transport position.
2. **The LG keeps one transport position.** On this renderer, `SetAVTransportURI`
   may not reset the position unless the transport is `STOPPED` first.
3. **App state is cleared regardless.** `CastService::control(Stop)`
   (`crates/reel-desktop/src/cast.rs`) does `*guard = None;
   clear_status_locked(...)` **even when the renderer refused the Stop**, so
   "Stop casting" looks done in-app while the TV keeps playing.

## Fix (implemented)

The renderer is now treated as a state machine and transitions are verified,
which addresses hypotheses 1–3:

- `CastSession::stop` (`crates/reel-cast/src/session.rs`) sends `Stop` (with one
  retry on a fault), then checks `GetTransportInfo` and only reports success —
  and clears the position — once the transport reads `STOPPED`/`NO_MEDIA_PRESENT`.
  A refused `Stop` is surfaced as an error instead of being ignored.
- `CastSession::start` calls `stop_if_needed` first: it only loads the new URI
  once the transport is confirmed stopped, so a still-playing or wedged renderer
  cannot resume the old media. If it will not stop, the cast fails loudly.
- After `Set` + `Play`, `CastSession::correct_start_position` reads
  `GetPositionInfo` and seeks to 0 **only if** `RelTime` is materially non-zero
  (> 3 s), so a renderer that failed to reset its single transport position is
  corrected without seeking a stream that already started at the beginning.
- `CastService::control` (`crates/reel-desktop/src/cast.rs`) only drops the
  session and clears the status once `stop()` confirms; on failure it keeps the
  cast visible with a failure message (and stops the poll loop from overwriting
  it), so "Stop casting" no longer looks done while the TV keeps playing.
- No per-cast URL uniqueness was added: the state machine above makes it
  unnecessary, and the earlier `?cast=<n>` hack stays reverted.

Covered by `crates/reel-cast/tests/control.rs`: `starting_new_media_stops_a_playing_renderer`,
`start_seeks_back_when_the_renderer_keeps_its_position`, `a_refused_stop_is_reported`.

## Reproduce / diagnose on the real TV

- TV name + capabilities:
  `cargo run -p reel-cast --example find_tv -- <tv-ip>` (prints `hevc/ac3/eac3/
  mp4/mkv`).
- Direct SOAP probe against the renderer (replace the control URL from the
  description):

  ```bash
  # GetTransportInfo / GetMediaInfo / GetPositionInfo / Stop
  curl -s -X POST 'http://<tv>:1241/AVTransport/<uuid>/control.xml' \
    -H 'CONTENT-TYPE: text/xml; charset="utf-8"' \
    -H 'SOAPAction: "urn:schemas-upnp-org:service:AVTransport:1#GetTransportInfo"' \
    --data '<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:GetTransportInfo xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID></u:GetTransportInfo></s:Body></s:Envelope>'
  ```

- Log: `grep -iE "cast|renderer capabilities|stopping" ~/.local/share/reel/reel.log`.
- Environment facts from the last session: the LAN media port is **fixed**
  (Settings → Casting, default `41337`) and the firewall must allow it from the
  LAN, otherwise `SetAVTransportURI` fails with **UPnP 716** before playback.
  SSDP multicast is filtered on this network, so discovery uses the mDNS +
  port-scan path.

## Files

- `crates/reel-cast/src/session.rs` — `CastSession::start` / `stop`, plus
  `stop_if_needed`, `settle_until` and `settle_position` (the verified state
  machine; `start_at` added the two-way hand-off).
- `crates/reel-cast/src/target.rs` — `DlnaTarget` SOAP (`stop`, `play`, `seek`,
  `position_info`, `transport_info`).
- `crates/reel-desktop/src/cast.rs` — `CastService::start_cast`, `control`
  (only clears the cast once the stop is confirmed),
  `prepare_and_start`.
- `crates/reel-desktop/src/ui/cast.rs` — the cast bar and remote ("Stop" →
  `cast_action(Stop)`).
- `crates/reel-cast/tests/control.rs` — the regression tests.

## Acceptance

- Cast A, stop part-way, cast B → **B starts at 0:00**.
- "Stop casting" leaves the renderer `STOPPED` (verified via `GetTransportInfo`)
  with nothing playing.

Both are exercised against the fake renderer in `control.rs`; the real-TV check
below remains for a manual pass.
