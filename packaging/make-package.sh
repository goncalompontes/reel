#!/usr/bin/env bash
# Build a Debian (.deb) and/or RPM (.rpm) package from the built binaries.
#
#   packaging/make-package.sh --version 0.1.0 --out dist --deb --rpm
#
# The file layout is produced by scripts/install.sh — the same script users run
# — staged into a temporary root, so a package and a manual install cannot
# drift apart. Binaries are read from target/release (build first with
# `cargo build --release --locked`).
#
# A .deb is built with `dpkg-deb` when present and otherwise assembled with
# `ar` + `tar` (the format is exactly that), so it works without dpkg.
# An .rpm needs `rpmbuild` (the `rpm` / `rpm-tools` package).

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

VERSION=""
OUT="dist"
DO_DEB=0
DO_RPM=0

usage() {
  cat <<'EOF'
Build reel .deb / .rpm packages.

Options:
  --version VER   Package version (required)
  --out DIR       Output directory. Default: dist
  --deb           Build a .deb
  --rpm           Build a .rpm
  -h, --help      This message

Environment: PREFIX is not used; the package always installs to /usr.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:?}"; shift 2 ;;
    --out) OUT="${2:?}"; shift 2 ;;
    --deb) DO_DEB=1; shift ;;
    --rpm) DO_RPM=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'unknown option: %s\n\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

[ -n "$VERSION" ] || { printf 'missing --version\n' >&2; exit 2; }
[ "$DO_DEB" -eq 1 ] || [ "$DO_RPM" -eq 1 ] || { printf 'nothing to do (pass --deb and/or --rpm)\n' >&2; exit 2; }

mkdir -p "$OUT"
# Absolute, so tools that run after a `cd` (ar, rpmbuild) still find it.
OUT="$(cd "$OUT" && pwd)"

# Architecture names differ per packaging system.
MACHINE="$(uname -m)"
case "$MACHINE" in
  x86_64|amd64) DEB_ARCH="amd64"; RPM_ARCH="x86_64" ;;
  aarch64|arm64) DEB_ARCH="arm64"; RPM_ARCH="aarch64" ;;
  *) printf 'unsupported architecture: %s\n' "$MACHINE" >&2; exit 1 ;;
esac

# The shared description, reused by both packages.
DESC_SHORT="Native desktop client for streaming torrents"
DESC_LONG="reel streams torrents with a Netflix-shaped catalogue. It runs the
torrent engine, an HTTP range-streaming server, a libmpv-backed player and the
catalogue in one native window. Playback starts after a few pieces rather than
after the whole file. ffmpeg/ffprobe and mpv/libmpv are runtime tools, not
build dependencies."

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# One layout, one source of truth: the same installer users run. The bundled
# Cloudflare solver is skipped (it needs network and a Python toolchain, and
# does not belong in a package); point Settings at an external FlareSolverr.
"$ROOT/scripts/install.sh" --prefix /usr --destdir "$STAGE" --no-build --without-solver --quiet

if [ "$DO_DEB" -eq 1 ]; then
  DEB_ROOT="$(mktemp -d)"
  trap 'rm -rf "$STAGE" "$DEB_ROOT"' EXIT
  cp -a "$STAGE/." "$DEB_ROOT/"
  mkdir -p "$DEB_ROOT/DEBIAN"
  cat > "$DEB_ROOT/DEBIAN/control" <<EOF
Package: reel
Version: $VERSION
Section: video
Priority: optional
Architecture: $DEB_ARCH
Maintainer: reel contributors <noreply@github.com>
Homepage: https://github.com/goncalompontes/reel
Recommends: ffmpeg, mpv
Description: $DESC_SHORT
$(printf '%s\n' "$DESC_LONG" | sed 's/^/ /')
EOF
  # A checksum manifest is expected by many tools; harmless if absent.
  ( cd "$DEB_ROOT" && find usr -type f -exec md5sum {} + > DEBIAN/md5sums )

  DEB="$OUT/reel_${VERSION}_${DEB_ARCH}.deb"
  rm -f "$DEB"
  if command -v dpkg-deb >/dev/null 2>&1; then
    dpkg-deb --build --root-owner-group "$DEB_ROOT" "$DEB" >/dev/null
  else
    # Assemble the deb by hand: an `ar` archive of debian-binary,
    # control.tar.gz and data.tar.gz. `ar` from GNU binutils is what dpkg
    # itself uses.
    ASM="$(mktemp -d)"
    printf '2.0\n' > "$ASM/debian-binary"
    ( cd "$DEB_ROOT/DEBIAN" && tar -czf "$ASM/control.tar.gz" --owner=0 --group=0 . )
    ( cd "$DEB_ROOT" && tar -czf "$ASM/data.tar.gz" --owner=0 --group=0 --exclude=./DEBIAN . )
    ( cd "$ASM" && ar rc "$DEB" debian-binary control.tar.gz data.tar.gz )
    rm -rf "$ASM"
  fi
  printf '%s\n' "$DEB"
fi

if [ "$DO_RPM" -eq 1 ]; then
  command -v rpmbuild >/dev/null 2>&1 || {
    printf 'rpmbuild is required for --rpm (install the rpm / rpm-tools package)\n' >&2
    exit 1
  }
  TOP="$(mktemp -d)"
  trap 'rm -rf "$STAGE" "$TOP"' EXIT
  mkdir -p "$TOP"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
  cat > "$TOP/SPECS/reel.spec" <<EOF
Name:           reel
Version:        $VERSION
Release:        1%{?dist}
Summary:        $DESC_SHORT
License:        MIT OR Apache-2.0
URL:            https://github.com/goncalompontes/reel
BuildArch:      $RPM_ARCH
Requires:       glibc
Recommends:     ffmpeg
Recommends:     mpv
%global debug_package %{nil}
%global _build_id_links none

%description
$DESC_LONG

%install
rm -rf %{buildroot}
mkdir -p %{buildroot}
cp -a %{_reel_stage}/. %{buildroot}/

%files
/usr/bin/reel
/usr/bin/reel-desktop
/usr/share/applications/reel.desktop
/usr/share/icons/hicolor/*/apps/reel.png
/usr/share/icons/hicolor/scalable/apps/reel.svg
/usr/share/licenses/reel/LICENSE-MIT
/usr/share/licenses/reel/LICENSE-APACHE

%changelog
EOF
  rpmbuild -bb \
    --define "_topdir $TOP" \
    --define "_reel_stage $STAGE" \
    --define "version $VERSION" \
    "$TOP/SPECS/reel.spec" >/dev/null
  RPM="$(find "$TOP/RPMS" -name '*.rpm' -print -quit)"
  cp "$RPM" "$OUT/"
  printf '%s\n' "$OUT/$(basename "$RPM")"
fi
