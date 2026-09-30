#!/usr/bin/env bash
# Regenerate the raster icons from assets/reel.svg.
#
# The PNGs are committed, so this only needs running after the SVG changes.
# Requires rsvg-convert (librsvg) — the same tool desktops use.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

command -v rsvg-convert >/dev/null 2>&1 || {
  printf 'rsvg-convert not found; install librsvg\n' >&2
  exit 1
}

mkdir -p "$ROOT/assets/icons"
for size in 16 32 48 64 128 256; do
  rsvg-convert -w "$size" -h "$size" \
    -o "$ROOT/assets/icons/reel-$size.png" "$ROOT/assets/reel.svg"
  printf '  assets/icons/reel-%s.png\n' "$size"
done
