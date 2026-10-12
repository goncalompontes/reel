#!/usr/bin/env bash
# Shared helpers for resolving reel's system dependencies.
#
# Sourced by scripts/install.sh and scripts/install-solver.sh. The caller is
# expected to define `say` (quiet-aware) and to set these globals:
#
#   ASSUME_YES  1 to answer every prompt with yes
#   NO_DEPS     1 to never install anything, only report
#
# Everything here is Linux-focused: the shell installer is Linux-only. On other
# platforms reel builds with cargo and the user supplies the tools.

# warn is shared; callers may define their own (identical) version.
warn() { printf 'warning: %s\n' "$*" >&2; }

# The package manager to use, or empty if none was recognised.
detect_package_manager() {
  for manager in pacman apt-get dnf zypper apk; do
    if command -v "$manager" >/dev/null 2>&1; then
      printf '%s' "$manager"
      return
    fi
  done
  printf ''
}

# The package name for a logical dependency, per package manager.
#
# $1 manager, $2 one of: ffmpeg, mpv, browser, xvfb
pkg_for() {
  case "$2" in
    ffmpeg)
      case "$1" in pacman|apt-get|dnf|zypper|apk) printf 'ffmpeg' ;; esac ;;
    mpv)
      case "$1" in pacman|apt-get|dnf|zypper|apk) printf 'mpv' ;; esac ;;
    browser)
      case "$1" in pacman|apt-get|dnf|zypper|apk) printf 'chromium' ;; esac ;;
    xvfb)
      case "$1" in
        pacman) printf 'xorg-server-xvfb' ;;
        apt-get) printf 'xvfb' ;;
        dnf) printf 'xorg-x11-server-Xvfb' ;;
        zypper) printf 'xorg-x11-server-extra' ;;
        apk) printf 'xvfb' ;;
      esac ;;
  esac
}

# Ask a yes/no question on the terminal. No terminal means "no", so a
# non-interactive install never hangs.
confirm() {
  [ "${ASSUME_YES:-0}" -eq 1 ] && return 0
  [ -r /dev/tty ] || return 1
  printf '%s [y/N] ' "$1" > /dev/tty 2>/dev/null || return 1
  local reply=""
  read -r reply < /dev/tty 2>/dev/null || return 1
  case "$reply" in [yY]|[yY][eE][sS]) return 0 ;; *) return 1 ;; esac
}

# Install packages with the detected manager, via sudo unless already root.
install_packages() {
  local sudo=""
  [ "$(id -u)" -eq 0 ] || sudo="sudo"
  case "${PKG_MANAGER:-}" in
    pacman) $sudo pacman -S --needed --noconfirm "$@" ;;
    apt-get) $sudo apt-get update && $sudo apt-get install -y "$@" ;;
    dnf) $sudo dnf install -y "$@" ;;
    zypper) $sudo zypper --non-interactive install "$@" ;;
    apk) $sudo apk add "$@" ;;
    *) return 1 ;;
  esac
}

# Offer to install a missing dependency: `$1` a human name, `$2` the package,
# `$3` what breaks without it.
install_dependency() {
  local what="$1" package="$2" hint="$3"
  if [ "${NO_DEPS:-0}" -eq 1 ] || [ -z "${PKG_MANAGER:-}" ] || [ -z "$package" ]; then
    warn "$what is missing: $hint"
    return
  fi
  if confirm "$what is missing. Install '$package' with $PKG_MANAGER?"; then
    say "  installing $package..."
    install_packages "$package" || warn "  could not install $package: $hint"
  else
    warn "  skipped: $hint"
  fi
}

# The browser names the solver might use.
find_browser() {
  for candidate in google-chrome google-chrome-stable chromium chromium-browser; do
    if command -v "$candidate" >/dev/null 2>&1; then
      printf '%s' "$candidate"
      return
    fi
  done
  printf ''
}

# Whether a shared library is loadable, by name (e.g. libmpv).
have_library() {
  local name="$1"
  if command -v ldconfig >/dev/null 2>&1 && ldconfig -p 2>/dev/null | grep -q "$name"; then
    return 0
  fi
  # macOS (Homebrew) and any odd layout: look on the usual library paths.
  for dir in /usr/lib /usr/local/lib /opt/homebrew/lib "$HOME/.local/lib"; do
    [ -e "$dir/$name".* ] && return 0
  done
  return 1
}
