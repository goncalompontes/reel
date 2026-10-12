# Setup, platforms and dependencies

This is the operator's view of reel: what it needs, how to install it on each
platform, and the sharp edges that are easy to trip over. The engineering
rationale lives in [`ARCHITECTURE.md`](ARCHITECTURE.md); the cast story lives in
[`CASTING.md`](CASTING.md).

## Platform support

| Platform | Build | CI | Notes |
| --- | --- | --- | --- |
| Linux x86_64 | yes | build + test + e2e | the reference platform; the shell installer and the bundled solver are here |
| Linux aarch64 | yes | — | builds; the installer works, the bundled solver needs an `uv` for aarch64 (no pinned checksum) |
| macOS (arm64, x86_64) | yes | build + test | no shell installer and no bundled solver; build with cargo |
| Windows x86_64 | yes | build + test | no shell installer and no bundled solver; build with cargo |

The whole workspace (including the wgpu desktop app) is cross-checked to compile
for Windows from Linux with `cargo check --target x86_64-pc-windows-gnu`; macOS
needs a real macOS runner because a C dependency (`aws-lc-sys`, via rustls) has
to be compiled with the macOS SDK.

## Runtime dependencies

reel **builds and starts without any of these** and tells you what is missing at
the moment it is needed. They are not build dependencies.

| Tool | Unlocks | Without it |
| --- | --- | --- |
| `ffmpeg` + `ffprobe` | casting (DLNA/Google Cast) and transcoding | playback works; casting and odd-codec transcoding are unavailable |
| `mpv` / libmpv | embedded playback (the picture inside the app window) | reel hands playback to an external `mpv`; if that is missing too, video cannot play |
| Chrome/Chromium + `Xvfb` | the bundled Cloudflare solver, so the ext.to search source works | ext.to is listed as needing setup and skipped; every other source still works |
| a GPU driver (Vulkan/OpenGL) | the desktop app's wgpu renderer | the app needs a software renderer (Mesa lavapipe/llvmpipe) or it will not open a window |

`libmpv` is loaded at runtime with `dlopen`, never linked, so a machine with no
mpv still builds and runs.

## Installing

```bash
scripts/install.sh              # ~/.local, no root; builds and installs
scripts/install.sh --check      # the "doctor": report dependencies, change nothing
scripts/install.sh --yes        # answer yes to every dependency prompt
scripts/install.sh --no-deps    # never install a package; only report
scripts/install.sh --uninstall  # remove what it installed (keeps your library)
```

`--check` is the fastest way to see what a machine has and what each missing
tool would break:

```
$ scripts/install.sh --check
reel dependency check (nothing will be installed or written)

Runtime dependencies (optional — reel runs without them):
  ffmpeg/ffprobe   found: /usr/bin/ffmpeg
  mpv/libmpv       found
  Chrome/Chromium  found: google-chrome-stable
  Xvfb             found
```

The installer resolves missing dependencies through the detected package manager
(`pacman`, `apt-get`, `dnf`, `zypper`, `apk`), asking before each one unless
`--yes` is given. The bundled-solver installer shares the same code
(`scripts/lib/deps.sh`), so Chrome and Xvfb are resolved the same way.

## Releases and packaging

`packaging/` holds the pieces, and `.github/workflows/release.yml` drives them:

* `packaging/make-archive.sh` — a portable archive (`tar.gz`/`zip`) of the two
  binaries plus licences and `INSTALL.md`.
* `packaging/make-package.sh` — a `.deb` and/or `.rpm`, staged through the same
  `scripts/install.sh`, so a package and a manual install cannot drift.
* `packaging/PKGBUILD` — the Arch package, also driven by `scripts/install.sh`.

Pushing a `vX.Y.Z` tag builds and attaches: `reel-<v>-linux-x86_64.tar.gz`,
`reel-<v>-macos-universal.tar.gz` (one `lipo`-fused binary), 
`reel-<v>-windows-x86_64.zip`, `reel_<v>_amd64.deb`, `reel-<v>-*.rpm`, and a
`SHA256SUMS`.

## Pain points and quirks

Things that cost time when setting reel up. The first two were fixed in the
cross-platform pass; the rest are inherent and are documented here so they are
not rediscovered.

1. **The declared MSRV was wrong (fixed).** `rust-version` said `1.86`, but the
   desktop app pulls egui/eframe 0.36, which require **1.95** (`wgpu` needs
   1.87). cargo refuses to build those packages on anything older, so `1.86`
   was never buildable. The workspace now declares `1.95`. Note this activated
   MSRV-aware clippy lints, so clippy reports ~53 suggestions
   (mostly `collapsible_if`); CI runs clippy for visibility but does not gate on
   it.

2. **Stale scratch could delete a live stream off Linux (fixed).** Stream
   scratch directories were swept by checking `/proc/<pid>`, which is Linux-only.
   Elsewhere the check returned "dead" for every PID, so a second instance could
   delete a *running* instance's scratch (Unix lets you unlink open files). The
   liveness test is now a portable OS file lock, so it is correct on macOS and
   Windows too.

3. **Casting needs a firewall hole.** Casting serves the media over the LAN on a
   fixed port (default **41337**). A host firewall (ufw/firewalld) blocks the
   incoming request and the TV sits at "loading" forever with no error in the
   app. Allow the port from your LAN: `sudo ufw allow from 192.168.0.0/16 to any
   port 41337 proto tcp`. The app does not touch the firewall for you.

4. **The bundled solver is heavy and Linux-only.** A first install downloads
   FlareSolverr and a pinned Python 3.11 (via `uv`), and needs Chrome and Xvfb.
   It is a real browser driving a real display; there is no way around it for
   Cloudflare's interactive challenge. On macOS/Windows there is no bundled
   solver — point Settings at a FlareSolverr you run yourself.

5. **The ext.to cookie expires.** Cloudflare ties the clearance cookie to your
   IP and user agent, so it goes stale; searches then report the challenge
   rather than returning nothing. Refresh it from a browser when that happens.

6. **A launcher does not read your shell rc files.** Two consequences: the
   desktop entry uses an **absolute** `Exec` (because `~/.local/bin` is usually
   not on a GUI session's `PATH`), and environment variables exported from
   `.zshrc`/`.bashrc` are invisible to the app. `settings.json` is the canonical
   configuration; env vars are first-run fallbacks only.

7. **libmpv is version-gated.** The build script reads mpv's headers when they
   are present and otherwise falls back to the values verified against mpv 0.41,
   refusing embedded playback on an unverified client API (< 2.5). If your
   distro's mpv is much newer or older, install the `mpv` development headers so
   the layout is read from them rather than guessed.

8. **The desktop app needs a working GPU stack.** eframe renders with wgpu
   (Vulkan on Linux). On a headless server, a VM, or a machine with only a
   proprietary driver that does not expose Vulkan, you need Mesa's software
   renderer (`lavapipe`/`llvmpipe`) or the window will not open.

9. **Vendored, patched crates must be re-patched on bumps.** `librqbit`,
   `fcast-sender-sdk` and `google-cast-protocol` are vendored under `vendor/`
   with behaviour patches reel depends on (streaming window, Cast protocol
   fixes). See `vendor/*/REEL_PATCHES.md`. Bumping any of them means porting the
   patch.

10. **The tree is not rustfmt-clean and clippy is not clean.** Do not run
    `cargo fmt` repo-wide; match the surrounding style. CI therefore checks
    build, tests and (non-gating) clippy, not formatting.

11. **`cargo build --locked` is assumed.** The committed `Cargo.lock` is the
    source of truth; the installer and every workflow build `--locked`.

12. **The Arch PKGBUILD builds from a local checkout.** Its `source=()` is empty
    and it `cd`s to `$startdir`; it is not yet wired to a release tarball.
