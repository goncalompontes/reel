#!/usr/bin/env bash
# End-to-end test for reel's engine + streaming server.
#
#   1. generates a video with ffmpeg
#   2. turns it into a torrent with `reel create`
#   3. seeds it with one `reel serve` (upload throttled, so the transfer is slow
#      enough to observe)
#   4. streams byte ranges out of a second `reel serve` that has to fetch the
#      data from the first -- the "watch while downloading" path
#   5. checks the bytes are exact, ranges/seek work, errors are sane, and that a
#      real demuxer (ffprobe) can read the stream while it is still downloading
#
# Usage:  bash scripts/e2e.sh
# Env:    REEL=/path/to/reel   ROOT=/tmp/opencode/reel-e2e-data

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REEL="${REEL:-$HERE/../target/debug/reel}"
ROOT="${ROOT:-/tmp/opencode/reel-e2e-data}"

PASS=0
FAIL=0
ok()   { echo "  PASS  $*"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL  $*"; FAIL=$((FAIL+1)); }
check(){ if [ "$2" = "$3" ]; then ok "$1 ($2)"; else bad "$1: expected [$3] got [$2]"; fi; }
jq_()  { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)" 2>/dev/null || echo ""; }

for tool in ffmpeg ffprobe curl python3; do
  command -v "$tool" >/dev/null || { echo "missing required tool: $tool"; exit 2; }
done
[ -x "$REEL" ] || { echo "reel binary not found at $REEL (run: cargo build)"; exit 2; }

rm -rf "$ROOT"
mkdir -p "$ROOT/seed" "$ROOT/leech" "$ROOT/logs"

echo "== 1. generate a test video =="
ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i testsrc=duration=45:size=1280x720:rate=30 \
  -c:v libx264 -preset ultrafast -pix_fmt yuv420p -b:v 3000k \
  "$ROOT/seed/test.mp4" || { echo "ffmpeg failed"; exit 1; }
SRC_SIZE=$(stat -c%s "$ROOT/seed/test.mp4")
SRC_SHA=$(sha256sum "$ROOT/seed/test.mp4" | cut -d' ' -f1)
echo "  source: $SRC_SIZE bytes sha256=${SRC_SHA:0:16}..."

echo "== 2. create torrent =="
"$REEL" create "$ROOT/seed/test.mp4" -o "$ROOT/test.torrent" > "$ROOT/logs/create.log" 2>&1
grep -E "wrote|info hash" "$ROOT/logs/create.log"
[ -f "$ROOT/test.torrent" ] || { echo "no torrent file produced"; exit 1; }

# The same content, addressed the other way. Everything below uses the .torrent
# file; step 23 uses this magnet, which has to find its metadata in the swarm.
INFO_HASH=$(grep -oE 'info hash +[0-9a-f]{40}' "$ROOT/logs/create.log" | awk '{print $3}')
[ -n "$INFO_HASH" ] || { echo "could not read the info hash"; exit 1; }

echo "== 3. start seeder (api 3041, peer port 51413, upload capped at 400 KiB/s) =="
"$REEL" serve --seed \
  --dir "$ROOT/seed" --api-addr 127.0.0.1:3041 --listen-port 51413 \
  --no-dht --no-trackers --no-persist --overwrite --upload-limit 409600 \
  --add "$ROOT/test.torrent" > "$ROOT/logs/seed.log" 2>&1 &
SEED_PID=$!

echo "== 4. start leecher (api 3042, peer port 51414, peer=seeder) =="
"$REEL" serve \
  --dir "$ROOT/leech" --api-addr 127.0.0.1:3042 --listen-port 51414 \
  --no-dht --no-trackers --no-persist --peer 127.0.0.1:51413 --add "$ROOT/test.torrent" \
  > "$ROOT/logs/leech.log" 2>&1 &
LEECH_PID=$!

cleanup() { kill "$SEED_PID" "$LEECH_PID" 2>/dev/null; wait 2>/dev/null; }
trap cleanup EXIT

echo "== 5. wait for both APIs =="
for _ in $(seq 1 40); do
  curl -sf -o /dev/null http://127.0.0.1:3041/api/health &&
    curl -sf -o /dev/null http://127.0.0.1:3042/api/health && break
  sleep 0.5
done
curl -sf -o /dev/null http://127.0.0.1:3041/api/health && ok "seeder api up" || bad "seeder api down"
curl -sf -o /dev/null http://127.0.0.1:3042/api/health && ok "leecher api up" || bad "leecher api down"

echo "== 6. discover the torrent, files and stream URL =="
LIST=""
for _ in $(seq 1 40); do
  LIST=$(curl -sf http://127.0.0.1:3042/api/torrents || true)
  [ -n "$LIST" ] && [ "$LIST" != "[]" ] && break
  sleep 0.5
done
ID=$(echo "$LIST" | jq_ 'd[0]["id"]')
TOTAL=$(echo "$LIST" | jq_ 'd[0]["stats"]["total_bytes"]')
DETAIL=$(curl -sf "http://127.0.0.1:3042/api/torrents/$ID")
PRIMARY=$(echo "$DETAIL" | jq_ 'd.get("primary_file_id")')
STREAM_URL=$(echo "$DETAIL" | jq_ 'd["files"][0]["stream"]["url"]')
echo "  id=$ID total=$TOTAL"
echo "  stream url: $STREAM_URL"
check "primary file id" "$PRIMARY" "0"
check "total bytes match source" "$TOTAL" "$SRC_SIZE"
check "stream url is absolute" "$STREAM_URL" "http://127.0.0.1:3042/stream/$ID/0/test.mp4"
check "primary file flag" "$(echo "$DETAIL" | jq_ 'd["files"][0]["is_video"]')" "True"
check "only playable files selected" "$(echo "$DETAIL" | jq_ 'd["files"][0]["included"]')" "True"

echo "== 7. wait for a live peer connection to the seeder =="
PEERS=0
for _ in $(seq 1 60); do
  PEERS=$(curl -sf "http://127.0.0.1:3042/api/torrents/$ID" | jq_ 'd["stats"]["peers"]["live"]')
  [ "${PEERS:-0}" -ge 1 ] && break
  sleep 0.5
done
# 2 is normal here: the explicit --peer connection plus local peer discovery.
if [ "${PEERS:-0}" -ge 1 ]; then ok "connected to the seeder (live peers: $PEERS)"; else bad "no live peers"; fi

echo "== 8. streaming state BEFORE any range request =="
BEFORE=$(curl -sf "http://127.0.0.1:3042/api/torrents/$ID" | jq_ 'd["stats"]["progress_bytes"]')
echo "  progress_bytes=$BEFORE of $TOTAL"

echo "== 9. partial range request: first 64 KiB =="
START=$(date +%s%N)
curl -s -D "$ROOT/logs/range1.headers" -r 0-65535 \
  -o "$ROOT/chunk1.bin" "http://127.0.0.1:3042/stream/$ID/0/test.mp4"
ELAPSED_MS=$(( ($(date +%s%N) - START) / 1000000 ))
echo "  request took ${ELAPSED_MS}ms (blocks until the covering piece is fetched)"
STATUS=$(head -1 "$ROOT/logs/range1.headers" | tr -d '\r' | awk '{print $2}')
C_RANGE=$(grep -i '^content-range:' "$ROOT/logs/range1.headers" | tr -d '\r' | awk '{print $2" "$3}')
C_LEN=$(grep -i '^content-length:' "$ROOT/logs/range1.headers" | tr -d '\r' | awk '{print $2}')
C_TYPE=$(grep -i '^content-type:' "$ROOT/logs/range1.headers" | tr -d '\r' | awk '{print $2}')
ACCEPT=$(grep -i '^accept-ranges:' "$ROOT/logs/range1.headers" | tr -d '\r' | awk '{print $2}')
check "status" "$STATUS" "206"
check "content-range" "$C_RANGE" "bytes 0-65535/$SRC_SIZE"
check "content-length" "$C_LEN" "65536"
check "content-type" "$C_TYPE" "video/mp4"
check "accept-ranges" "$ACCEPT" "bytes"
head -c 65536 "$ROOT/seed/test.mp4" > "$ROOT/expect1.bin"
cmp -s "$ROOT/chunk1.bin" "$ROOT/expect1.bin" && ok "bytes 0-65535 match the source" || bad "first chunk differs"

echo "== 10. proof it streamed before finishing =="
AFTER=$(curl -sf "http://127.0.0.1:3042/api/torrents/$ID" | jq_ 'd["stats"]["progress_bytes"]')
echo "  progress_bytes=$BEFORE -> $AFTER of $TOTAL"
if [ "${AFTER:-0}" -lt "$TOTAL" ]; then
  ok "served real bytes while only $AFTER/$TOTAL bytes had been downloaded"
else
  bad "file was already fully downloaded before any range was served"
fi

echo "== 11. a real demuxer reads the stream while downloading =="
PROBE=$(ffprobe -v error -show_entries stream=codec_name,width,height \
  -of json "$STREAM_URL" 2>"$ROOT/logs/ffprobe.err" || true)
echo "  $PROBE" | tr -d '\n' | head -c 200; echo
check "ffprobe codec" "$(echo "$PROBE" | jq_ 'd["streams"][0]["codec_name"]')" "h264"
check "ffprobe width" "$(echo "$PROBE" | jq_ 'd["streams"][0]["width"]')" "1280"
check "ffprobe height" "$(echo "$PROBE" | jq_ 'd["streams"][0]["height"]')" "720"
if ffprobe -v error -show_entries stream=codec_name -of json "$STREAM_URL" >/dev/null 2>&1; then
  ok "ffprobe succeeded over HTTP range requests"
else
  bad "ffprobe could not read the stream"
fi

echo "== 12. mid-file range (seeking): 1 KiB at offset 5,000,000 =="
curl -s -D "$ROOT/logs/range2.headers" -r 5000000-5000999 \
  -o "$ROOT/chunk2.bin" "http://127.0.0.1:3042/stream/$ID/0/test.mp4"
STATUS2=$(head -1 "$ROOT/logs/range2.headers" | tr -d '\r' | awk '{print $2}')
C_RANGE2=$(grep -i '^content-range:' "$ROOT/logs/range2.headers" | tr -d '\r' | awk '{print $2" "$3}')
check "status" "$STATUS2" "206"
check "content-range" "$C_RANGE2" "bytes 5000000-5000999/$SRC_SIZE"
dd if="$ROOT/seed/test.mp4" of="$ROOT/expect2.bin" bs=1 skip=5000000 count=1000 status=none
cmp -s "$ROOT/chunk2.bin" "$ROOT/expect2.bin" && ok "mid-file bytes match (random access works)" || bad "mid-file chunk differs"

echo "== 13. suffix range: last 1 KiB =="
curl -s -D "$ROOT/logs/range3.headers" -r -1024 \
  -o "$ROOT/chunk3.bin" "http://127.0.0.1:3042/stream/$ID/0/test.mp4"
STATUS3=$(head -1 "$ROOT/logs/range3.headers" | tr -d '\r' | awk '{print $2}')
check "status" "$STATUS3" "206"
check "suffix length" "$(stat -c%s "$ROOT/chunk3.bin")" "1024"
tail -c 1024 "$ROOT/seed/test.mp4" > "$ROOT/expect3.bin"
cmp -s "$ROOT/chunk3.bin" "$ROOT/expect3.bin" && ok "suffix bytes match" || bad "suffix chunk differs"

echo "== 14. unsatisfiable range rejected =="
STATUS4=$(curl -s -o /dev/null -w '%{http_code}' -r 999999999- \
  "http://127.0.0.1:3042/stream/$ID/0/test.mp4")
check "status" "$STATUS4" "416"

echo "== 15. error handling =="
check "unknown torrent" "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:3042/api/torrents/9999)" "404"
check "unknown file" "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:3042/stream/$ID/7/x.mkv")" "404"
check "bad add body" "$(curl -s -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:3042/api/torrents \
  -H 'content-type: application/json' -d '{"source":"not-a-magnet"}')" "400"

echo "== 16. control plane: pause / resume / SSE =="
check "paused state" "$(curl -s -X POST "http://127.0.0.1:3042/api/torrents/$ID/pause" | jq_ 'd["state"]')" "paused"
check "resumed state" "$(curl -s -X POST "http://127.0.0.1:3042/api/torrents/$ID/resume" | jq_ 'd["state"]')" "live"
SSE=$(curl -s --max-time 3 -N http://127.0.0.1:3042/api/events | head -4 | tr -d '\r')
echo "$SSE" | grep -q "event: torrents" && ok "SSE emits torrents events" || bad "no SSE events"

echo "== 17. native playback: libmpv decodes the torrent stream =="
# This is the whole point of the project: the stream URL is fed to a real
# video pipeline (libmpv, the same decoder mpv and VLC use) and must produce
# actual picture while the torrent is still incomplete.
DUMP="${DUMP_FRAME:-$HERE/../target/debug/examples/dump_frame}"
if [ ! -x "$DUMP" ]; then
  bad "dump_frame not built (run: cargo build -p reel-player --example dump_frame)"
else
  PLAYER_LOG="$ROOT/logs/player.log"
  "$DUMP" "$STREAM_URL" --frames 12 --width 640 --height 360 --timeout 90 \
    > "$PLAYER_LOG" 2>&1
  PLAYER_RC=$?

  grep -E "^(backend|frames_rendered|nonblack_percent|opaque|video_size|state_error|position)=" "$PLAYER_LOG" | sed 's/^/  /'

  check "player exit code" "$PLAYER_RC" "0"
  check "player backend" "$(sed -n 's/^backend=//p' "$PLAYER_LOG")" "embedded"
  check "frames decoded" "$(sed -n 's/^frames_rendered=//p' "$PLAYER_LOG")" "12"
  check "decoded video size" "$(sed -n 's/^video_size=//p' "$PLAYER_LOG")" "1280x720"
  check "frames fully opaque" "$(sed -n 's/^opaque=//p' "$PLAYER_LOG")" "true"
  PLAYER_MEAN=$(sed -n 's/^mean_luma=//p' "$PLAYER_LOG")
  if awk -v v="$PLAYER_MEAN" 'BEGIN { exit !(v > 20) }' 2>/dev/null; then
    ok "decoded picture is not blank (mean luma $PLAYER_MEAN)"
  else
    bad "decoded picture looks blank (mean luma '$PLAYER_MEAN')"
  fi

  echo "== 18. native playback: seeking through the torrent stream =="
  SEEKS="$ROOT/logs/player-seek.log"
  "$DUMP" "$STREAM_URL" --seek 5 --frames 5 --width 640 --height 360 --timeout 90 \
    > "$SEEKS" 2>&1
  SEEK_POS=$(sed -n 's/^position=//p' "$SEEKS")
  echo "  position after seek: $SEEK_POS"
  if awk -v v="$SEEK_POS" 'BEGIN { exit !(v >= 5) }' 2>/dev/null; then
    ok "player seeked to ${SEEK_POS}s inside the torrent stream"
  else
    bad "player did not seek (position '$SEEK_POS')"
  fi
  check "still opaque after seek" "$(sed -n 's/^opaque=//p' "$SEEKS")" "true"

  echo "== 19. the desktop app's own video surface renders the stream =="
# reel-player proves libmpv can decode the stream, and the UI snapshots prove
# the interface renders. This proves the link between them: a decoded frame
# reaching egui as a texture, through the code the desktop app actually runs.
PIPELINE="${PIPELINE:-$HERE/../target/debug/examples/player_pipeline}"
if [ ! -x "$PIPELINE" ]; then
  bad "player_pipeline not built (run: cargo build -p reel-desktop --example player_pipeline)"
else
  PIPE_LOG="$ROOT/logs/pipeline.log"
  "$PIPELINE" "$STREAM_URL" --frames 8 --timeout 90 --size 1280x720 > "$PIPE_LOG" 2>&1
  PIPE_RC=$?
  grep -E "^(backend|frames_uploaded|textures_uploaded|texture_size|video_size)=" "$PIPE_LOG" | sed 's/^/  /'

  check "pipeline exit code" "$PIPE_RC" "0"
  check "pipeline backend" "$(sed -n 's/^backend=//p' "$PIPE_LOG")" "embedded"
  check "decoded video size" "$(sed -n 's/^video_size=//p' "$PIPE_LOG")" "1280x720"
  check "surface texture size" "$(sed -n 's/^texture_size=//p' "$PIPE_LOG")" "1280x720"
  UPLOADED=$(sed -n 's/^textures_uploaded=//p' "$PIPE_LOG")
  if [ "${UPLOADED:-0}" -ge 8 ]; then
    ok "frames reached egui as textures ($UPLOADED uploads)"
  else
    bad "expected >= 8 texture uploads, got ${UPLOADED:-0}"
  fi
fi

echo "== 20. still incomplete while it was being played =="
  STILL=$(curl -sf "http://127.0.0.1:3042/api/torrents/$ID" | jq_ 'd["stats"]["finished"]')
  PROG=$(curl -sf "http://127.0.0.1:3042/api/torrents/$ID" | jq_ 'd["stats"]["progress_bytes"]')
  echo "  downloaded $PROG / $SRC_SIZE (finished=$STILL)"
  if [ "$PROG" -lt "$SRC_SIZE" ]; then
    ok "picture was decoded before the download completed"
  else
    ok "download completed during playback (still correct, less telling)"
  fi
fi

echo "== 21. full download matches the original byte for byte =="
curl -s -o "$ROOT/full.mp4" "http://127.0.0.1:3042/stream/$ID/0/test.mp4"
check "size" "$(stat -c%s "$ROOT/full.mp4")" "$SRC_SIZE"
check "sha256" "$(sha256sum "$ROOT/full.mp4" | cut -d' ' -f1)" "$SRC_SHA"
check "torrent marked finished" "$(curl -sf "http://127.0.0.1:3042/api/torrents/$ID" | jq_ 'd["stats"]["finished"]')" "True"

echo "== 22. CLI client against the running server =="
LS=$("$REEL" ls --api http://127.0.0.1:3042 2>&1)
echo "$LS" | grep -q "test.mp4" && ok "reel ls shows the torrent" || bad "reel ls output unexpected: $LS"
PLAY=$("$REEL" play "$ID" --api http://127.0.0.1:3042 --player "" 2>&1)
check "reel play prints the stream url" "$PLAY" "$STREAM_URL"

echo "== 23. a magnet resolves its metadata and streams =="
# Steps 1-22 all use a .torrent file, so the magnet branch of AddSource - parse
# the URI, fetch metadata from a peer, then stream - was untested. DHT and
# trackers stay off and the seeder is given explicitly, so the only thing under
# test is magnet resolution.
mkdir -p "$ROOT/magnet"
"$REEL" serve --dir "$ROOT/magnet" --api-addr 127.0.0.1:3043 --listen-port 51415 \
  --no-dht --no-trackers --no-persist --peer 127.0.0.1:51413 \
  --add "magnet:?xt=urn:btih:$INFO_HASH" > "$ROOT/logs/magnet.log" 2>&1 &
MAGNET_PID=$!
cleanup_magnet() { kill "$MAGNET_PID" 2>/dev/null || true; }
trap 'cleanup_magnet; cleanup' EXIT

# The API must be up regardless of whether the magnet has resolved yet.
MAGNET_UP=""
for _ in $(seq 1 40); do
  if curl -sf -o /dev/null http://127.0.0.1:3043/api/health; then MAGNET_UP=yes; break; fi
  sleep 0.5
done
[ -n "$MAGNET_UP" ] && ok "the API starts without waiting for the magnet" \
  || bad "the API did not start"

# A bare magnet has no file list until metadata arrives from a peer.
MAGNET_LIST=""
for _ in $(seq 1 120); do
  MAGNET_LIST=$(curl -sf http://127.0.0.1:3043/api/torrents || true)
  if [ -n "$MAGNET_LIST" ] && [ "$MAGNET_LIST" != "[]" ]; then
    COUNT=$(echo "$MAGNET_LIST" | jq_ 'len(d[0]["files"])')
    [ "${COUNT:-0}" -ge 1 ] && break
  fi
  sleep 0.5
done

MAGNET_ID=$(echo "$MAGNET_LIST" | jq_ 'd[0]["id"]')
MAGNET_NAME=$(echo "$MAGNET_LIST" | jq_ 'd[0]["name"]')
MAGNET_SIZE=$(echo "$MAGNET_LIST" | jq_ 'd[0]["stats"]["total_bytes"]')
MAGNET_PRIMARY=$(echo "$MAGNET_LIST" | jq_ 'd[0].get("primary_file_id")')
echo "  resolved: name=$MAGNET_NAME size=$MAGNET_SIZE primary_file=$MAGNET_PRIMARY"

check "metadata resolved from the swarm" "$MAGNET_SIZE" "$SRC_SIZE"
check "a playable file was found from the magnet" "$MAGNET_PRIMARY" "0"

if [ "${MAGNET_SIZE:-0}" = "$SRC_SIZE" ]; then
  curl -s -D "$ROOT/logs/magnet.headers" -r 0-65535 \
    -o "$ROOT/magnet.bin" "http://127.0.0.1:3043/stream/$MAGNET_ID/0/$MAGNET_NAME"
  M_STATUS=$(head -1 "$ROOT/logs/magnet.headers" | tr -d '\r' | awk '{print $2}')
  M_RANGE=$(grep -i '^content-range:' "$ROOT/logs/magnet.headers" | tr -d '\r' | awk '{print $2" "$3}')
  check "status" "$M_STATUS" "206"
  check "content-range" "$M_RANGE" "bytes 0-65535/$SRC_SIZE"
  cmp -s "$ROOT/magnet.bin" "$ROOT/expect1.bin" \
    && ok "magnet-streamed bytes match the source" \
    || bad "magnet-streamed bytes differ"
fi

echo "== 24. the installed launcher points at a real binary =="
# The installer is how both users and packaging place files, so a wrong layout
# or an unsubstituted path should fail here rather than silently when someone
# clicks an icon. A desktop entry whose Exec cannot be found does nothing at
# all: no window, no message, no journal entry.
PREFIX_TREE="$ROOT/install-prefix"
"$HERE/install.sh" --prefix "$PREFIX_TREE" --no-build --quiet >/dev/null 2>&1
ENTRY="$PREFIX_TREE/share/applications/reel.desktop"

if [ -f "$ENTRY" ]; then
  ok "the installer wrote a desktop entry"
  EXEC_PATH=$(sed -n 's/^Exec=//p' "$ENTRY" | head -1 | awk '{print $1}')
  check "Exec names an absolute path" "$EXEC_PATH" "$PREFIX_TREE/bin/reel-desktop"
  [ -x "$EXEC_PATH" ] && ok "the binary the launcher names is executable" \
    || bad "the launcher names a binary that is not there: $EXEC_PATH"
  [ -f "$PREFIX_TREE/share/icons/hicolor/scalable/apps/reel.svg" ] \
    && ok "the icon is installed" || bad "no icon was installed"
  if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$ENTRY" >/dev/null 2>&1 \
      && ok "the installed entry validates" || bad "the installed entry does not validate"
  fi
  # Nothing should be left pointing at a bare command name.
  grep -qE '^(Exec|TryExec)=reel' "$ENTRY" \
    && bad "the entry still contains a PATH-dependent command" \
    || ok "no PATH-dependent command in the entry"
else
  bad "the installer wrote no desktop entry"
fi

echo "== 25. watching one episode fetches only that episode =="
# The season-pack case. Adding with media_only selects every playable file, so
# without per-file selection a three-episode pack downloads all three while you
# watch the first.
#
# Added paused, narrowed, then resumed: that makes the "nothing was spent on the
# others" assertion deterministic. Note what it therefore proves and what it
# does not - it proves the engine fetches only the selected file once the
# selection is made, not that no bytes for other files ever arrive in the window
# between adding and pressing play, which is inherent to the real flow.
PACK="$ROOT/pack"
rm -rf "$PACK"
# The torrent is created from the show directory, so its name is that directory
# and its content resolves to <output>/<name>/<file>. A seeder must therefore
# point --dir at the *parent* of the show directory, which is what `reel create`
# prints as its suggested command.
mkdir -p "$PACK/library/Some.Show.S01" "$PACK/leech"
for n in 1 2 3; do
  ffmpeg -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc=duration=6:size=640x360:rate=25" \
    -c:v libx264 -preset ultrafast -pix_fmt yuv420p -b:v 2500k \
    "$PACK/library/Some.Show.S01/Some.Show.S01E0$n.mp4"
done
# A small piece length on purpose. Selection works at piece granularity, so with
# the 2 MiB default these three 700 KB episodes would share two pieces and
# fetching one would fetch them all. Real episodes are gigabytes against the
# same 2 MiB pieces, where the overlap is a rounding error at each boundary.
PIECE=16384
"$REEL" create "$PACK/library/Some.Show.S01" -o "$PACK/pack.torrent" \
  --piece-length "$PIECE" > "$PACK/create.log" 2>&1
"$REEL" serve --seed --dir "$PACK/library" --api-addr 127.0.0.1:3044 --listen-port 51416 \
  --no-dht --no-trackers --no-persist --overwrite --add "$PACK/pack.torrent" \
  > "$PACK/seed.log" 2>&1 &
PACK_SEED=$!

# A seeder that cannot find its files looks exactly like a broken leecher, and
# that cost an hour of chasing the wrong thing. Check it first.
SEEDED=""
for _ in $(seq 1 60); do
  SEEDED=$(curl -sf --max-time 5 http://127.0.0.1:3044/api/torrents/0 | jq_ 'd["stats"]["finished"]')
  [ "$SEEDED" = "True" ] && break
  sleep 0.5
done
check "the seeder actually has the data to serve" "${SEEDED:-None}" "True"

"$REEL" serve --dir "$PACK/leech" --api-addr 127.0.0.1:3045 --listen-port 51417 \
  --no-dht --no-trackers --no-persist --peer 127.0.0.1:51416 \
  --paused --add "$PACK/pack.torrent" > "$PACK/leech.log" 2>&1 &
PACK_LEECH=$!
cleanup_pack() { kill "$PACK_SEED" "$PACK_LEECH" 2>/dev/null || true; }
trap 'cleanup_pack; cleanup_magnet; cleanup' EXIT

for _ in $(seq 1 40); do
  curl -sf -o /dev/null http://127.0.0.1:3044/api/health &&
    curl -sf -o /dev/null http://127.0.0.1:3045/api/health && break
  sleep 0.5
done

DETAIL=$(curl -sf http://127.0.0.1:3045/api/torrents/0)
COUNT=$(echo "$DETAIL" | jq_ 'len(d["files"])')
check "a three-episode pack" "$COUNT" "3"

# Everything playable is selected to begin with, which is the problem.
ALL=$(echo "$DETAIL" | jq_ 'sum(1 for f in d["files"] if f["included"])')
check "media_only selected every episode" "$ALL" "3"

# Pressing play on one episode narrows the fetch to it. Pick by name: the
# engine's file order is not the directory order, and episode 2 is neither the
# first nor the last, so a pass cannot come from fetching from one end.
TARGET_NAME="Some.Show.S01E02.mp4"
TARGET=$(echo "$DETAIL" | jq_ '[f["id"] for f in d["files"] if f["name"]=="'"$TARGET_NAME"'"][0]')
echo "  watching: $TARGET_NAME (file $TARGET)"
check "found the episode by name" "$(echo "$DETAIL" | jq_ '[f for f in d["files"] if f["name"]=="'"$TARGET_NAME"'"][0]["length"]' | grep -c .)" "1"

curl -sf -X PUT "http://127.0.0.1:3045/api/torrents/0/files" \
  -H 'content-type: application/json' -d "{\"only_files\":[$TARGET]}" > /dev/null
curl -sf -X POST "http://127.0.0.1:3045/api/torrents/0/resume" > /dev/null

for _ in $(seq 1 120); do
  sleep 0.5
  DETAIL=$(curl -sf http://127.0.0.1:3045/api/torrents/0 || true)
  [ -n "$DETAIL" ] || continue
  DONE=$(echo "$DETAIL" | jq_ 'd["files"]['"$TARGET"']["progress_bytes"]')
  [ "${DONE:-0}" -gt 0 ] && break
done

DETAIL=$(curl -sf http://127.0.0.1:3045/api/torrents/0)
echo "$DETAIL" | python3 -c "
import json,sys
d=json.load(sys.stdin)
for f in d['files']:
    print(f\"    {f['name']:<28} selected={str(f['included']):<5} fetched={f['progress_bytes']} of {f['length']}\")
"

WATCHED=$(echo "$DETAIL" | jq_ '[f for f in d["files"] if f["id"]=='"$TARGET"'][0]["progress_bytes"]')
LENGTH=$(echo "$DETAIL" | jq_ '[f for f in d["files"] if f["id"]=='"$TARGET"'][0]["length"]')
OTHERS=$(echo "$DETAIL" | jq_ 'sum(f["progress_bytes"] for f in d["files"] if f["id"]!='"$TARGET"')')

check "the watched episode was fetched in full" "${WATCHED:-0}" "$LENGTH"

# Not zero, and it should not be: a piece that straddles a file boundary belongs
# to both files. Two pieces of slack per neighbour is the honest bound.
OTHER_FILE=$(echo "$DETAIL" | jq_ '[f["length"] for f in d["files"] if f["id"]!='"$TARGET"'][0]')
if [ "${OTHERS:-0}" -le $((PIECE * 2)) ]; then
  ok "the other episodes cost only boundary bleed ($OTHERS bytes, under 2 pieces)"
else
  bad "the other episodes were downloaded: $OTHERS bytes (a full one is $OTHER_FILE)"
fi

SELECTED=$(echo "$DETAIL" | jq_ 'sum(1 for f in d["files"] if f["included"])')
check "exactly one file is selected" "$SELECTED" "1"

# And the selected episode still streams.
URL="http://127.0.0.1:3045/stream/0/$TARGET/$TARGET_NAME"
# --max-time matters: a stream request for pieces that never arrive waits
# forever by design, so an unbounded curl would hang the whole suite.
curl -s --max-time 60 -o "$PACK/chunk.bin" -r 0-32767 "$URL"
check "the episode streams" "$(stat -c%s "$PACK/chunk.bin" 2>/dev/null || echo 0)" "32768"

echo
echo "===== $PASS passed, $FAIL failed ====="
exit $(( FAIL > 0 ))
