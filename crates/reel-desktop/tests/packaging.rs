//! Guards on the packaging metadata.
//!
//! These are cheap string checks on files outside the crate, and they exist
//! because the failure they catch is *silent*: a desktop entry whose `Exec`
//! cannot be found does nothing at all when clicked. No window, no message, no
//! journal entry. The binary was fine; the launcher simply could not resolve a
//! command name against a `PATH` that does not contain `~/.local/bin`, because
//! a GUI session never reads shell rc files.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate is two levels below the repo root")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Values of `key` in a desktop entry, for every occurrence.
fn desktop_values(entry: &str, key: &str) -> Vec<String> {
    entry
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix(key)?.strip_prefix('=').map(str::to_string)
        })
        .collect()
}

#[test]
fn the_launcher_entry_names_an_absolute_binary() {
    let entry = read("packaging/reel.desktop.in");

    for key in ["Exec", "TryExec"] {
        let values = desktop_values(&entry, key);
        assert_eq!(values.len(), 1, "expected exactly one {key} line");
        assert!(
            values[0].starts_with("@BINDIR@/"),
            "{key} must be absolute and substituted by the installer, got {:?}",
            values[0]
        );
    }
}

#[test]
fn the_installer_substitutes_the_bindir_placeholder() {
    let script = read("scripts/install.sh");

    // Both files that bake in a path must be run through sed.
    for template in ["packaging/reel.desktop.in", "packaging/reel.service.in"] {
        assert!(
            script.contains(template),
            "install.sh should install {template}"
        );
    }
    assert!(
        script.contains(r#"sed "s|@BINDIR@|$BIN_DIR|g""#),
        "install.sh should substitute @BINDIR@ with the real bindir"
    );
}

#[test]
fn the_desktop_entry_agrees_with_the_app() {
    let entry = read("packaging/reel.desktop.in");
    let main = read("crates/reel-desktop/src/main.rs");

    // A mismatch here does not break launching, but it does break taskbar
    // grouping and the icon on Wayland, which is confusing in a subtler way.
    let wm_class = desktop_values(&entry, "StartupWMClass");
    assert_eq!(wm_class.len(), 1, "expected a StartupWMClass");
    assert!(
        main.contains(&format!(r#".with_app_id("{}")"#, wm_class[0])),
        "main.rs should set the Wayland app_id to {:?} to match the entry",
        wm_class[0]
    );

    // The icon is looked up by name, so it must match what is installed.
    assert_eq!(desktop_values(&entry, "Icon"), ["reel"]);
    assert!(
        repo_root().join("assets/reel.svg").is_file(),
        "the icon the entry names should exist"
    );
}

#[test]
fn every_shipped_icon_size_exists() {
    let script = read("scripts/install.sh");

    // The installer enumerates sizes and fails if a PNG is missing; make sure
    // the assets agree before anyone runs it.
    let sizes_line = script
        .lines()
        .find(|line| line.starts_with("ICON_SIZES="))
        .expect("install.sh should list its icon sizes");
    let sizes: Vec<&str> = sizes_line
        .trim_start_matches("ICON_SIZES=")
        .trim_matches('"')
        .split_whitespace()
        .collect();

    assert!(sizes.len() >= 4, "expected several sizes, got {sizes:?}");
    for size in sizes {
        let path = repo_root().join(format!("assets/icons/reel-{size}.png"));
        assert!(path.is_file(), "missing icon asset: {}", path.display());
    }
}
