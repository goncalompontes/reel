//! Scripted, headless UI scenarios.
//!
//! These drive the **real** widget tree — the same `App`, the same `egui`
//! layout — through `egui_kittest`, which exposes it over AccessKit. A test can
//! find a button by its label, click it, type into a field, press keys, step
//! frames and render to a PNG, all without a display server. That is what makes
//! an interaction bug reproducible instead of "it didn't work on my machine".
//!
//! The fake backend is used so playback never touches a real device; the flows
//! asserted here are the ones that go wrong in the engine too (selection must
//! not start a torrent, a paused pack must not look like a download, and the
//! download toggle must reverse).

use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use reel_desktop::backend::FakeBackend;
use reel_desktop::testing::{sample_library, sample_paused_season_pack};
use reel_desktop::{App, Screen};

/// A harness around the real app with the given library.
fn harness(items: Vec<reel_desktop::LibraryItem>) -> Harness<'static, App> {
    Harness::builder().with_size((1280.0, 1000.0)).build_ui_state(
        move |ui, app: &mut App| {
            let mut frame = eframe::Frame::_new_kittest();
            eframe::App::ui(app, ui, &mut frame);
        },
        App::new(Box::new(FakeBackend::new(items))),
    )
}

/// A library with the sample titles plus one paused season pack (torrent 9).
fn with_paused_pack() -> Harness<'static, App> {
    let mut items = sample_library();
    items.push(sample_paused_season_pack());
    let mut harness = harness(items);
    harness.run_steps(3);
    harness.state_mut().navigate(Screen::Detail(9));
    harness.run_steps(3);
    harness
}

#[test]
fn a_paused_pack_does_not_look_like_it_is_downloading() {
    let harness = with_paused_pack();

    assert!(
        harness
            .query_all_by_label_contains("nothing is being fetched")
            .next()
            .is_some(),
        "a paused pack should say so"
    );
    assert!(
        harness
            .query_all_by_label_contains("Stop download")
            .next()
            .is_none(),
        "nothing is being kept, so there is nothing to stop"
    );
    // Every row still offers Download, which is how you would keep one.
    assert!(
        harness
            .get_all_by_label("Download")
            .next()
            .is_some(),
        "episodes should be downloadable"
    );
}

#[test]
fn download_toggles_to_stop_download_and_back() {
    let mut harness = with_paused_pack();

    {
        harness
            .get_all_by_label("Download")
            .next()
            .expect("a Download button")
            .click();
    }
    harness.run_steps(4);
    assert!(
        harness.state().item(9).expect("the pack").downloading,
        "Download should mark the title as kept"
    );
    assert!(
        harness
            .get_all_by_label("Stop download")
            .next()
            .is_some(),
        "once kept, the button reverses"
    );

    {
        harness
            .get_all_by_label("Stop download")
            .next()
            .expect("a Stop download button")
            .click();
    }
    harness.run_steps(4);
    assert!(
        !harness.state().item(9).expect("the pack").downloading,
        "Stop download should release it again"
    );
}

#[test]
fn playing_an_episode_narrows_the_fetch_and_starts_the_torrent() {
    let mut harness = with_paused_pack();

    {
        // The top Play acts on the first episode; either way it should narrow.
        harness
            .get_all_by_label("Play")
            .next()
            .expect("a Play button")
            .click();
    }
    harness.run_steps(4);

    let item = harness.state().item(9).expect("the pack");
    assert!(
        !item.torrent.stats.is_paused(),
        "playing has to resume the torrent"
    );
    let included: Vec<usize> = item
        .torrent
        .files
        .iter()
        .filter(|file| file.included)
        .map(|file| file.id)
        .collect();
    assert_eq!(included, vec![0], "only the played episode is fetched");
}

#[test]
fn a_magnet_can_be_typed_into_the_add_screen() {
    let mut harness = harness(sample_library());
    harness.run_steps(3);

    {
        harness
            .get_all_by_label_contains("Add")
            .next()
            .expect("the Add tab")
            .click();
    }
    harness.run_steps(3);

    {
        let field = harness.get_by_role(Role::MultilineTextInput);
        field.focus();
        field.type_text("magnet:?xt=urn:btih:abc");
    }
    harness.run_steps(2);

    {
        harness.get_by_label("Add torrent").click();
    }
    harness.run_steps(4);

    assert!(
        harness
            .query_all_by_label_contains("add magnet")
            .next()
            .is_some(),
        "the typed source should have reached the backend"
    );
}

/// The reported flow: keep a title, stop keeping it, then stream it again. The
/// second play must still work.
#[test]
fn a_title_streams_again_after_stopping_a_download() {
    let mut harness = with_paused_pack();

    {
        harness
            .get_all_by_label("Download")
            .next()
            .expect("a Download button")
            .click();
    }
    harness.run_steps(4);
    {
        harness
            .get_all_by_label("Stop download")
            .next()
            .expect("a Stop download button")
            .click();
    }
    harness.run_steps(4);
    assert!(!harness.state().item(9).expect("the pack").downloading);

    {
        harness
            .get_all_by_label("Play")
            .next()
            .expect("a Play button")
            .click();
    }
    harness.run_steps(4);

    let item = harness.state().item(9).expect("the pack");
    assert!(
        !item.torrent.stats.is_paused(),
        "the second play must resume the torrent"
    );
    assert_eq!(
        item.torrent
            .files
            .iter()
            .filter(|file| file.included)
            .count(),
        1,
        "only the played episode is fetched"
    );
}

/// Renders the paused-pack detail page so the "not downloading" state can be
/// reviewed by eye.
#[test]
fn snapshot_paused_pack() {
    let mut harness = with_paused_pack();
    harness.snapshot("paused-pack");
}
