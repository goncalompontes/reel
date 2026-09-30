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

echo "== 3. start seeder (api 3041, peer port 51413, upload capped at 400 KiB/s) =="
"$REEL" serve \
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

echo "== 17. full download matches the original byte for byte =="
curl -s -o "$ROOT/full.mp4" "http://127.0.0.1:3042/stream/$ID/0/test.mp4"
check "size" "$(stat -c%s "$ROOT/full.mp4")" "$SRC_SIZE"
check "sha256" "$(sha256sum "$ROOT/full.mp4" | cut -d' ' -f1)" "$SRC_SHA"
check "torrent marked finished" "$(curl -sf "http://127.0.0.1:3042/api/torrents/$ID" | jq_ 'd["stats"]["finished"]')" "True"

echo "== 18. CLI client against the running server =="
LS=$("$REEL" ls --api http://127.0.0.1:3042 2>&1)
echo "$LS" | grep -q "test.mp4" && ok "reel ls shows the torrent" || bad "reel ls output unexpected: $LS"
PLAY=$("$REEL" play "$ID" --api http://127.0.0.1:3042 --player "" 2>&1)
check "reel play prints the stream url" "$PLAY" "$STREAM_URL"

echo
echo "===== $PASS passed, $FAIL failed ====="
exit $(( FAIL > 0 ))
