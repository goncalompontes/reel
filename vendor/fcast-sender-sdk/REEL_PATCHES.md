# reel patches to `fcast-sender-sdk`

This is the upstream crate `fcast-sender-sdk` 0.3.0, vendored and patched so
Google Cast playback works with the Default Media Receiver (`CC1AD845`). The
root `Cargo.toml` maps it in via `[patch.crates-io]`. **Port these on any
version bump** (all in `src/chromecast.rs`).

1. **`streamType` must be `BUFFERED`, not `NONE`.** The load hardcoded
   `protocol::StreamType::None`; the receiver rejects that with `LOAD_FAILED`.
   A file load is buffered (on-demand).

2. **The load must autoplay.** `auto_play` was `None`; the receiver loaded the
   media and stayed `IDLE` forever. Now `Some(true)`.

3. **`mediaSessionId` must be a number, not a string.** `Pause`/`Resume`/`Seek`/
   `Stop` sent it as `json::Value::String(...)`; the receiver rejects that with
   `INVALID_REQUEST / INVALID_MEDIA_SESSION_ID`. Now `json::Value::Number(...)`.

4. **Parse-failure logging includes the raw payload.** A protocol-crate parse
   gap (a media `ERROR` message without `itemId`) was undiagnosable. This only
   affects the log line.

5. **Side-loaded subtitle tracks.** `LoadRequest::Url`/`Video` gained a
   `subtitle_url`, and the load now sends a `tracks` entry plus
   `activeTrackIds: [1]`, so a Chromecast shows subtitles. This also needs the
   `google-cast-protocol` patch below (`MediaInformation.tracks` and
   `Media::Load.active_track_ids`).

`google-cast-protocol` is likewise vendored (`vendor/google-cast-protocol`) for
patch 5. reel passes the **WebVTT** sidecar URL (a Cast receiver cannot read
SubRip), extracted from the DIDL metadata the caller already builds.

Verified end to end against a real Android TV (Chromecast built in) with
`cargo run -p reel-cast-gcast --example cast_smoke "<device>"` — discovery,
connect, load, autoplay, advancing position, and volume.

Note: `google-cast-protocol` 0.1.0's `Media::Error` requires `itemId`, which
some receivers omit, so a receiver-reported media error may still be dropped by
the SDK. The reel adapter surfaces receiver errors only when the SDK parses
them; the raw payload is logged (patch 4) so the gap is visible.
