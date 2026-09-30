//! Headless UI tests.
//!
//! `egui_kittest` drives the real widget tree through AccessKit and can render
//! it with wgpu to a PNG, so the interface is verified without a display
//! server and without a browser.
//!
//! Run `UPDATE_SNAPSHOTS=1 cargo test -p reel-desktop --test ui` to rewrite the
//! reference images after an intentional visual change.

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use reel_desktop::backend::FakeBackend;
use reel_desktop::testing::sample_torrents;
use reel_desktop::{App, Screen};

/// Build a harness around the real app, with the sample library.
fn harness<'a>() -> Harness<'a, App> {
    Harness::builder()
        .with_size((1280.0, 820.0))
        .build_ui_state(
            |ui, app: &mut App| {
                // eframe hands the app a `Frame`; this is the supported way to
                // fabricate one for tests.
                let mut frame = eframe::Frame::_new_kittest();
                eframe::App::ui(app, ui, &mut frame);
            },
            App::new(Box::new(FakeBackend::new(sample_torrents()))),
        )
}

#[test]
fn library_screen_shows_every_torrent() {
    let mut harness = harness();
    harness.run_steps(3);

    // Titles are cleaned from release names, and appear on the cards. A title
    // can legitimately appear twice (hero banner and card), so query all.
    for title in ["The Matrix", "Big Buck Bunny", "Sintel"] {
        assert!(
            harness.query_all_by_label_contains(title).next().is_some(),
            "expected {title:?} to be visible in the library"
        );
    }
}

#[test]
fn clicking_a_card_opens_the_detail_page() {
    let mut harness = harness();
    harness.run_steps(3);

    harness.get_by_label_contains("Sintel").click();
    harness.run_steps(3);

    assert_eq!(
        harness.state().screen(),
        &Screen::Detail(3),
        "clicking a card should open that torrent's detail page"
    );
}

#[test]
fn navigation_reaches_the_add_screen_and_submits() {
    let mut harness = harness();
    harness.run_steps(3);

    harness.get_by_label_contains("Add").click();
    harness.run_steps(3);
    assert_eq!(harness.state().screen(), &Screen::Add);

    // An empty submit must warn rather than adding anything.
    harness.get_by_label("Add torrent").click();
    harness.run_steps(3);
    assert_eq!(harness.state().screen(), &Screen::Add);
    assert_eq!(harness.state().library_len(), 3);
}

#[test]
fn settings_reports_the_backend() {
    let mut harness = harness();
    harness.run_steps(3);

    harness.get_by_label_contains("Settings").click();
    harness.run_steps(3);

    assert_eq!(harness.state().screen(), &Screen::Settings);
    assert!(
        harness
            .query_by_label_contains("Download folder")
            .is_some(),
        "settings should show the download folder"
    );
}

/// Renders the library to a PNG so the layout can be reviewed by eye.
#[test]
fn snapshot_library() {
    let mut harness = harness();
    harness.run_steps(3);
    harness.snapshot("library");
}

/// Renders the detail page.
#[test]
fn snapshot_detail() {
    let mut harness = harness();
    harness.run_steps(3);
    harness.get_by_label_contains("Sintel").click();
    harness.run_steps(3);
    harness.snapshot("detail");
}
