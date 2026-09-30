#!/usr/bin/env bash
# Install reel: the binaries, the desktop entry and the icon.
#
#   scripts/install.sh                      # into ~/.local, no root needed
#   scripts/install.sh --prefix /usr        # system-wide, needs write access
#   scripts/install.sh --with-service       # also install a systemd user unit
#   scripts/install.sh --uninstall          # remove what it installed
#
# Packaging reuses this script, which is why it supports --destdir:
#
#   scripts/install.sh --prefix /usr --destdir "$pkgdir" --no-build
#
# It refuses to guess: everything it writes is printed, and re-running it is a
# no-op rather than an error.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

PREFIX="${PREFIX:-$HOME/.local}"
DESTDIR="${DESTDIR:-}"
DO_BUILD=1
WITH_SERVICE=0
UNINSTALL=0
QUIET=0

# systemd user units live somewhere else again when the prefix is a user's.
systemd_user_dir() {
  if [ "$PREFIX" = "$HOME/.local" ] || [ "$PREFIX" = "$HOME" ]; then
    printf '%s' "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  else
    printf '%s' "$PREFIX/lib/systemd/user"
  fi
}

usage() {
  cat <<'EOF'
Install reel.

Options:
  --prefix DIR      Where to install. Default: ~/.local
  --destdir DIR     Staging root, for packaging. Default: empty
  --no-build        Do not run cargo; use existing release binaries
  --with-service    Also install a systemd user unit for `reel serve`
  --uninstall       Remove everything this script installs
  -q, --quiet       Only print problems
  -h, --help        This message

Environment: PREFIX and DESTDIR are honoured if set.
EOF
}

say() { [ "$QUIET" -eq 1 ] || printf '%s\n' "$*"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX="${2:?--prefix needs a directory}"; shift 2 ;;
    --destdir) DESTDIR="${2:?--destdir needs a directory}"; shift 2 ;;
    --no-build) DO_BUILD=0; shift ;;
    --with-service) WITH_SERVICE=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -q|--quiet) QUIET=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'unknown option: %s\n\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

# The prefix is baked into files, so keep it absolute and unrooted; DESTDIR is
# the thing that gets prepended to every write.
case "$PREFIX" in
  /*) ;;
  *) printf 'prefix must be an absolute path, got %s\n' "$PREFIX" >&2; exit 2 ;;
esac
PREFIX="${PREFIX%/}"

BIN_DIR="$PREFIX/bin"
APP_DIR="$PREFIX/share/applications"
ICON_SCALABLE_DIR="$PREFIX/share/icons/hicolor/scalable/apps"
LICENSE_DIR="$PREFIX/share/licenses/reel"
SERVICE_DIR="$(systemd_user_dir)"

# Raster sizes installed alongside the scalable icon: not every launcher and
# taskbar reads SVG.
ICON_SIZES="16 32 48 64 128 256"

icon_png_dir() { printf '%s/share/icons/hicolor/%sx%s/apps' "$PREFIX" "$1" "$1"; }

# Paths as they will be seen by the *files*, not by the staging tree: a desktop
# entry must not contain a DESTDIR.
DEST() { printf '%s%s' "$DESTDIR" "$1"; }

installed_files() {
  printf '%s\n' \
    "$BIN_DIR/reel" \
    "$BIN_DIR/reel-desktop" \
    "$APP_DIR/reel.desktop" \
    "$ICON_SCALABLE_DIR/reel.svg" \
    "$LICENSE_DIR/LICENSE-MIT" \
    "$LICENSE_DIR/LICENSE-APACHE"

  local size
  for size in $ICON_SIZES; do
    printf '%s\n' "$(icon_png_dir "$size")/reel.png"
  done

  if [ "$WITH_SERVICE" -eq 1 ]; then
    printf '%s\n' "$SERVICE_DIR/reel.service"
  fi
  return 0
}

# Refresh the desktop and icon caches, but only on a real install: inside a
# package staging tree these tools would run against the build machine's paths.
refresh_caches() {
  [ -n "$DESTDIR" ] && return 0
  command -v update-desktop-database >/dev/null 2>&1 &&
    update-desktop-database "$(DEST "$APP_DIR")" 2>/dev/null || true
  command -v gtk-update-icon-cache >/dev/null 2>&1 &&
    gtk-update-icon-cache -qtf "$(DEST "$PREFIX/share/icons/hicolor")" 2>/dev/null || true
}

do_uninstall() {
  say "Removing reel from $PREFIX"
  # `installed_files` with the service flag only covers what this run would
  # install, so also look for a unit left by an earlier --with-service run.
  {
    installed_files
    printf '%s\n' "$SERVICE_DIR/reel.service"
  } | sort -u | while read -r path; do
    if [ -e "$(DEST "$path")" ]; then
      rm -f "$(DEST "$path")"
      say "  removed $path"
    fi
  done

  # Tidy up now-empty directories, deepest first, ignoring failures.
  local size
  for size in $ICON_SIZES; do
    local dir
    dir="$(icon_png_dir "$size")"
    if [ -d "$(DEST "$dir")" ]; then
      rmdir --ignore-fail-on-non-empty "$(DEST "$dir")" 2>/dev/null || true
    fi
  done
  for dir in "$ICON_SCALABLE_DIR" "$APP_DIR" "$LICENSE_DIR" "$SERVICE_DIR"; do
    [ -d "$(DEST "$dir")" ] && rmdir --ignore-fail-on-non-empty -p "$(DEST "$dir")" 2>/dev/null || true
  done

  refresh_caches
  say "Done. Your data in ~/.local/share/reel was left alone."
}

do_install() {
  if [ "$DO_BUILD" -eq 1 ]; then
    say "Building release binaries..."
    ( cd "$ROOT" && cargo build --release --locked )
  fi

  local bin
  for bin in reel reel-desktop; do
    if [ ! -x "$ROOT/target/release/$bin" ]; then
      printf 'missing %s; run without --no-build\n' "$ROOT/target/release/$bin" >&2
      exit 1
    fi
  done

  say "Installing to $PREFIX"
  install -d "$(DEST "$BIN_DIR")" "$(DEST "$APP_DIR")" "$(DEST "$ICON_SCALABLE_DIR")" "$(DEST "$LICENSE_DIR")"

  install -m755 "$ROOT/target/release/reel" "$(DEST "$BIN_DIR/reel")"
  say "  $BIN_DIR/reel"
  install -m755 "$ROOT/target/release/reel-desktop" "$(DEST "$BIN_DIR/reel-desktop")"
  say "  $BIN_DIR/reel-desktop"

  # The launcher gets an absolute Exec: a GUI session does not read shell rc
  # files, so it cannot be relied on to have ~/.local/bin on PATH, and a desktop
  # entry whose Exec is not found fails silently.
  sed "s|@BINDIR@|$BIN_DIR|g" "$ROOT/packaging/reel.desktop.in" \
    > "$(DEST "$APP_DIR/reel.desktop")"
  chmod 644 "$(DEST "$APP_DIR/reel.desktop")"
  say "  $APP_DIR/reel.desktop  (Exec=$BIN_DIR/reel-desktop)"

  install -m644 "$ROOT/assets/reel.svg" "$(DEST "$ICON_SCALABLE_DIR/reel.svg")"
  say "  $ICON_SCALABLE_DIR/reel.svg"

  local size png dir
  for size in $ICON_SIZES; do
    png="$ROOT/assets/icons/reel-$size.png"
    if [ ! -f "$png" ]; then
      printf 'missing icon %s (run scripts/make-icons.sh)\n' "$png" >&2
      exit 1
    fi
    dir="$(icon_png_dir "$size")"
    install -d "$(DEST "$dir")"
    install -m644 "$png" "$(DEST "$dir/reel.png")"
  done
  say "  $PREFIX/share/icons/hicolor/<size>/apps/reel.png  (sizes: $ICON_SIZES)"

  install -m644 "$ROOT/LICENSE-MIT" "$(DEST "$LICENSE_DIR/LICENSE-MIT")"
  install -m644 "$ROOT/LICENSE-APACHE" "$(DEST "$LICENSE_DIR/LICENSE-APACHE")"
  say "  $LICENSE_DIR/{LICENSE-MIT,LICENSE-APACHE}"

  if [ "$WITH_SERVICE" -eq 1 ]; then
    install -d "$(DEST "$SERVICE_DIR")"
    # The unit needs the real install path, not the staging path.
    sed "s|@BINDIR@|$BIN_DIR|g" "$ROOT/packaging/reel.service.in" \
      > "$(DEST "$SERVICE_DIR/reel.service")"
    chmod 644 "$(DEST "$SERVICE_DIR/reel.service")"
    say "  $SERVICE_DIR/reel.service"
  fi

  refresh_caches

  # Validate what we wrote, so a broken desktop entry is caught here and not by
  # the user wondering why there is no launcher.
  if command -v desktop-file-validate >/dev/null 2>&1; then
    if ! desktop-file-validate "$(DEST "$APP_DIR/reel.desktop")"; then
      printf 'warning: the installed desktop entry did not validate\n' >&2
    fi
  fi

  # The failure this guards against is silent, so check it here instead.
  if [ -z "$DESTDIR" ] && ! command -v "$BIN_DIR/reel-desktop" >/dev/null 2>&1; then
    printf 'warning: %s/reel-desktop is not executable; the launcher will not work\n' "$BIN_DIR" >&2
  fi

  say ""
  say "Installed."
  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) say "Note: $BIN_DIR is not on your PATH." ;;
  esac
  if [ "$WITH_SERVICE" -eq 1 ]; then
    say "Enable the daemon with: systemctl --user enable --now reel.service"
  fi
}

if [ "$UNINSTALL" -eq 1 ]; then
  do_uninstall
else
  do_install
fi
