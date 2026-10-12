#!/usr/bin/env bash
# Install the Cloudflare solver that ships alongside reel.
#
# ext.to sits behind a Cloudflare *interactive* challenge, which needs a real
# browser to clear. reel talks to FlareSolverr over HTTP; this script puts a
# pinned, self-contained FlareSolverr under <prefix>/lib/reel/solver so a fresh
# install works with no Docker and no manual setup. reel starts it on demand.
#
#   scripts/install-solver.sh                    # into ~/.local, no root
#   scripts/install-solver.sh --prefix /usr      # system-wide
#   scripts/install-solver.sh --uninstall        # remove it again
#
# It needs `uv` (to provide Python 3.11); if `uv` is not on PATH, a pinned copy
# is downloaded into the solver directory. It uses the system's Chrome/Chromium
# and Xvfb, and reports clearly if either is missing.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

# Shared dependency helpers: package-manager detection, prompts, tool lookups.
# shellcheck source=lib/deps.sh
. "$HERE/lib/deps.sh"

PREFIX="${PREFIX:-$HOME/.local}"
UNINSTALL=0
FORCE=0
QUIET=0
ASSUME_YES=0
NO_DEPS=0
PORT="${REEL_SOLVER_PORT:-8391}"

# Pinned versions, with the SHA-256 of each download.
FLARESOLVERR_VERSION="v3.5.2"
FLARESOLVERR_SHA256="e83ffb2e7f8b95769d33fad7848ea31c3a84d2e0a7771f7536209457ea03661c"
UV_VERSION="0.13.0"
UV_SHA256_X86_64="1468ebd5a5541121837c5a2817b9972ba6090fa6caa3d142620850a47fb75154"

say() { [ "$QUIET" -eq 1 ] || printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }

usage() {
  cat <<'EOF'
Install the bundled Cloudflare solver (FlareSolverr) for reel.

Options:
  --prefix DIR   Where reel is installed. Default: ~/.local
  --port PORT    Port the solver listens on. Default: 8391
  --force        Reinstall even if it is already present
  -y, --yes      Install missing system packages without asking
  --no-deps      Never install system packages; only report what is missing
  --uninstall    Remove the solver
  -q, --quiet    Only print problems
  -h, --help     This message

The solver drives a real Chrome, so it needs Chrome/Chromium and Xvfb. If either
is missing this script offers to install it with your package manager.

Environment: PREFIX and REEL_SOLVER_PORT are honoured if set.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX="${2:?--prefix needs a directory}"; shift 2 ;;
    --port) PORT="${2:?--port needs a number}"; shift 2 ;;
    --force) FORCE=1; shift ;;
    --yes|-y) ASSUME_YES=1; shift ;;
    --no-deps) NO_DEPS=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -q|--quiet) QUIET=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'unknown option: %s\n\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

case "$PREFIX" in
  /*) ;;
  *) printf 'prefix must be an absolute path, got %s\n' "$PREFIX" >&2; exit 2 ;;
esac
PREFIX="${PREFIX%/}"
SOLVER_DIR="$PREFIX/lib/reel/solver"
URL="http://127.0.0.1:$PORT"

if [ "$UNINSTALL" -eq 1 ]; then
  if [ -d "$SOLVER_DIR" ]; then
    rm -rf "$SOLVER_DIR"
    say "Removed $SOLVER_DIR"
  else
    say "Nothing to remove at $SOLVER_DIR"
  fi
  exit 0
fi

if [ -x "$SOLVER_DIR/run.sh" ] && [ -d "$SOLVER_DIR/.venv" ] && [ "$FORCE" -eq 0 ]; then
  say "The bundled solver is already installed in $SOLVER_DIR (use --force to reinstall)."
  exit 0
fi

# --- helpers ---------------------------------------------------------------

# download URL DEST SHA256: fetch and verify, or fail loudly.
download() {
  local url="$1" dest="$2" want="$3"
  say "  fetching $(basename "$url")"
  curl -fsSL --retry 3 -A "reel-installer" -o "$dest" "$url" ||
    { printf 'could not download %s\n' "$url" >&2; exit 1; }
  local got
  got="$(sha256sum "$dest" | awk '{print $1}')"
  if [ "$got" != "$want" ]; then
    rm -f "$dest"
    printf 'checksum mismatch for %s\n  expected %s\n  got      %s\n' "$url" "$want" "$got" >&2
    exit 1
  fi
}

# A pinned uv, used only if there is none on PATH.
resolve_uv() {
  if command -v uv >/dev/null 2>&1; then
    UV="$(command -v uv)"
    return
  fi
  local arch triple sha
  case "$(uname -m)" in
    x86_64) triple="x86_64-unknown-linux-gnu"; sha="$UV_SHA256_X86_64" ;;
    aarch64|arm64) triple="aarch64-unknown-linux-gnu"; sha="" ;;
    *) printf 'unsupported architecture: %s\n' "$(uname -m)" >&2; exit 1 ;;
  esac
  local dir="$SOLVER_DIR/.uv"
  mkdir -p "$dir"
  if [ ! -x "$dir/uv" ]; then
    local tarball="$dir/uv.tar.gz"
    if [ -n "$sha" ]; then
      download "https://github.com/astral-sh/uv/releases/download/$UV_VERSION/uv-$triple.tar.gz" "$tarball" "$sha"
    else
      # No pinned checksum for this architecture: verify against uv's own file.
      say "  fetching uv $UV_VERSION (no pinned checksum for $triple)"
      curl -fsSL --retry 3 -A "reel-installer" -o "$tarball" \
        "https://github.com/astral-sh/uv/releases/download/$UV_VERSION/uv-$triple.tar.gz" ||
        { printf 'could not download uv\n' >&2; exit 1; }
    fi
    tar -xzf "$tarball" -C "$dir" --strip-components=1
    rm -f "$tarball"
  fi
  UV="$dir/uv"
}

# --- system dependencies: Chrome/Chromium and Xvfb -------------------------

# Package-manager detection, the prompts, `install_packages` and
# `install_dependency` live in lib/deps.sh, shared with install.sh.

# Make sure the browser and Xvfb are present, offering to install them.
ensure_dependencies() {
  PKG_MANAGER="$(detect_package_manager)"

  local browser
  browser="$(find_browser)"
  if [ -n "$browser" ]; then
    say "  browser: $browser"
  else
    install_dependency "Chrome/Chromium" "$(pkg_for "$PKG_MANAGER" browser)" \
      "the solver needs a browser to clear Cloudflare, so ext.to will not work"
    browser="$(find_browser)"
    [ -n "$browser" ] && say "  browser: $browser"
  fi

  if command -v Xvfb >/dev/null 2>&1; then
    say "  virtual display: Xvfb"
  else
    install_dependency "Xvfb" "$(pkg_for "$PKG_MANAGER" xvfb)" \
      "the solver runs the browser on a virtual display, so ext.to will not work"
    command -v Xvfb >/dev/null 2>&1 && say "  virtual display: Xvfb"
  fi
}

# --- install ---------------------------------------------------------------

say "Installing the bundled solver into $SOLVER_DIR"
mkdir -p "$SOLVER_DIR"

# Ask about (and install) the browser and Xvfb first, so the user answers the
# prompt before the long download.
ensure_dependencies

resolve_uv

say "Fetching FlareSolverr $FLARESOLVERR_VERSION"
src_tarball="$SOLVER_DIR/flaresolverr-src.tar.gz"
download "https://github.com/FlareSolverr/FlareSolverr/archive/refs/tags/$FLARESOLVERR_VERSION.tar.gz" \
  "$src_tarball" "$FLARESOLVERR_SHA256"

tmp="$(mktemp -d "$SOLVER_DIR/.extract.XXXXXX")"
tar -xzf "$src_tarball" -C "$tmp" --strip-components=1
rm -rf "$SOLVER_DIR/src" "$SOLVER_DIR/requirements.txt" "$SOLVER_DIR/package.json"
cp -R "$tmp/src" "$SOLVER_DIR/src"
cp "$tmp/requirements.txt" "$SOLVER_DIR/requirements.txt"
# FlareSolverr reads its version from package.json one level above src/.
cp "$tmp/package.json" "$SOLVER_DIR/package.json"
rm -rf "$tmp" "$src_tarball"

say "Creating the Python 3.11 environment (this downloads Python and the solver's dependencies)"
rm -rf "$SOLVER_DIR/.venv"
"$UV" venv --python 3.11 "$SOLVER_DIR/.venv"
"$UV" pip install --python "$SOLVER_DIR/.venv/bin/python" -r "$SOLVER_DIR/requirements.txt"

cat > "$SOLVER_DIR/run.sh" <<'EOF'
#!/bin/sh
# Start the FlareSolverr that ships with reel. reel runs this on demand; it is
# safe to run by hand too.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
PORT="${REEL_SOLVER_PORT:-8391}"
HOST="${REEL_SOLVER_HOST:-127.0.0.1}"
export PORT HOST
exec "$here/.venv/bin/python" "$here/src/flaresolverr.py"
EOF
chmod 755 "$SOLVER_DIR/run.sh"

# The URL reel should talk to. Written next to run.sh, which reel finds relative
# to its own binary.
printf '%s\n' "$URL" > "$SOLVER_DIR/url"

say ""
say "Solver installed: $SOLVER_DIR"
say "  URL: $URL (reel starts it on demand)"
