#!/usr/bin/env bash
# Stage the release binaries and archive them for a GitHub release.
#
#   packaging/make-archive.sh \
#     --bin-dir target/release \
#     --version 0.1.0 \
#     --name reel-0.1.0-linux-x86_64 \
#     --ext tar.gz \
#     --out dist
#
# Produces `dist/<name>.<ext>` containing:
#
#   <name>/reel, reel-desktop        (reel.exe, reel-desktop.exe on Windows)
#   <name>/LICENSE-MIT, LICENSE-APACHE
#   <name>/README.md, INSTALL.md
#
# The binaries are self-contained apart from the *runtime* tools reel shells out
# to (ffmpeg/ffprobe for casting, mpv/libmpv for playback); INSTALL.md lists
# them. Run this from the repository root.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

BIN_DIR=""
VERSION=""
NAME=""
EXT="tar.gz"
OUT="dist"

usage() {
  cat <<'EOF'
Archive the built reel binaries for release.

Options:
  --bin-dir DIR   Directory holding the built binaries (required)
  --version VER   Version stamped into INSTALL.md (required)
  --name NAME     Archive base name, e.g. reel-0.1.0-linux-x86_64 (required)
  --ext EXT       tar.gz (default) or zip
  --out DIR       Output directory. Default: dist
  -h, --help      This message
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --bin-dir) BIN_DIR="${2:?}"; shift 2 ;;
    --version) VERSION="${2:?}"; shift 2 ;;
    --name) NAME="${2:?}"; shift 2 ;;
    --ext) EXT="${2:?}"; shift 2 ;;
    --out) OUT="${2:?}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'unknown option: %s\n\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

[ -n "$BIN_DIR" ] || { printf 'missing --bin-dir\n' >&2; exit 2; }
[ -n "$VERSION" ] || { printf 'missing --version\n' >&2; exit 2; }
[ -n "$NAME" ] || { printf 'missing --name\n' >&2; exit 2; }

# Windows ships `.exe`; everything else is bare.
EXE=""
case "$EXT" in
  zip) EXE=".exe" ;;
  tar.gz|tgz) ;;
  *) printf 'unsupported --ext %s (use tar.gz or zip)\n' "$EXT" >&2; exit 2 ;;
esac

for bin in reel reel-desktop; do
  [ -x "$BIN_DIR/$bin$EXE" ] || { printf 'missing %s\n' "$BIN_DIR/$bin$EXE" >&2; exit 1; }
done

mkdir -p "$OUT"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
PKG="$STAGE/$NAME"
mkdir -p "$PKG"

install -m755 "$BIN_DIR/reel$EXE" "$PKG/reel$EXE"
install -m755 "$BIN_DIR/reel-desktop$EXE" "$PKG/reel-desktop$EXE"
install -m644 "$ROOT/LICENSE-MIT" "$PKG/LICENSE-MIT"
install -m644 "$ROOT/LICENSE-APACHE" "$PKG/LICENSE-APACHE"
install -m644 "$ROOT/README.md" "$PKG/README.md"

cat > "$PKG/INSTALL.md" <<EOF
# reel $VERSION

Two binaries:

* \`reel\` — the CLI and headless daemon (\`reel serve\`, \`add\`, \`ls\`, \`play\`, ...).
* \`reel-desktop\` — the native desktop app (catalogue, library, player).

Nothing needs to be "installed": run them from anywhere. Put this directory on
your \`PATH\` (or symlink the two binaries into one) to use \`reel\` from a shell.

## Runtime tools (optional, but they unlock features)

reel builds and starts without any of these and tells you what is missing:

| Tool | Needed for | Without it |
| --- | --- | --- |
| \`mpv\` / libmpv | embedded playback | playback is handed to an external \`mpv\`; if that is missing too, video cannot play |
| \`ffmpeg\` + \`ffprobe\` | casting (DLNA/Chromecast), odd-codec transcoding | playback still works, but casting and transcoding are unavailable |

On Linux, a desktop launcher and icons are not bundled here; use the installer
(\`scripts/install.sh\`) from the source tree for that.

## Verify the download

\`\`\`sh
sha256sum -c SHA256SUMS      # a sibling file on the release page
\`\`\`
EOF

ARCHIVE="$OUT/$NAME.$EXT"
rm -f "$ARCHIVE"
case "$EXT" in
  tar.gz)
    tar -czf "$ARCHIVE" -C "$STAGE" "$NAME"
    ;;
  zip)
    # `zip` is present on Linux/macOS runners; the Windows runner has 7-Zip.
    if command -v zip >/dev/null 2>&1; then
      ( cd "$STAGE" && zip -qr "$ROOT/$ARCHIVE" "$NAME" )
    elif command -v 7z >/dev/null 2>&1; then
      ( cd "$STAGE" && 7z a -tzip "$ROOT/$ARCHIVE" "$NAME" >/dev/null )
    else
      printf 'no zip tool found (need zip or 7z)\n' >&2; exit 1
    fi
    ;;
esac

printf '%s\n' "$ARCHIVE"
