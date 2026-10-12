//! Interactive end-to-end tests: the real app widget tree, driven the way a
//! person drives it — move the mouse, hover, click — with a screenshot at each
//! step.
//!
//! `ui.rs` checks that individual screens render and respond to accessibility
//! clicks. These tests instead walk whole journeys through the pointer path and
//! render every step with wgpu, so a regression in *interaction* — not just
//! layout — is caught. The app, its widgets and its rendering are exactly the
//! code the shipped `reel-desktop` binary runs; `egui_kittest` only supplies the
//! window, the pointer and the screenshot.
//!
//! Run `UPDATE_SNAPSHOTS=1 cargo test -p reel-desktop --test e2e` to rewrite the
//! reference images after an intentional change.

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use reel_desktop::backend::FakeBackend;
use reel_desktop::testing::sample_library;
use reel_desktop::{App, Screen};

fn harness() -> Harness<'static, App> {
    // Each Harness has its own egui Context, so the image loaders have to be
    // installed per harness, exactly as `ui.rs` does.
    let installed = std::sync::Once::new();
    Harness::builder()
        .with_size((1280.0, 820.0))
        .build_ui_state(
            move |ui, app: &mut App| {
                installed.call_once(|| reel_desktop::install(ui.ctx()));
                let mut frame = eframe::Frame::_new_kittest();
                eframe::App::ui(app, ui, &mut frame);
            },
            App::new(Box::new(FakeBackend::new(sample_library()))),
        )
}

/// The centre of the first widget whose accessible label contains `label`.
fn centre_of(harness: &Harness<'static, App>, label: &str) -> egui::Pos2 {
    harness
        .get_all_by_label_contains(label)
        .next()
        .unwrap_or_else(|| panic!("no widget labelled {label:?}"))
        .rect()
        .center()
}

/// The centre of the first widget whose accessible *value* matches, for controls
/// like a combo box whose current text is a value rather than a label.
fn centre_of_value(harness: &Harness<'static, App>, value: &str) -> egui::Pos2 {
    harness
        .get_all_by_value(value)
        .next()
        .unwrap_or_else(|| panic!("no widget valued {value:?}"))
        .rect()
        .center()
}

/// Move the pointer onto a widget, then press and release the primary button
/// there. Deliberately not `node.click()`: this goes through the same
/// pointer-move/button path the desktop binary receives from the OS, so the
/// hit-testing and hover logic are exercised too.
fn mouse_click(harness: &mut Harness<'static, App>, label: &str) {
    let pos = centre_of(harness, label);
    harness.hover_at(pos);
    harness.run_steps(1);
    harness.drag_at(pos);
    harness.run_steps(1);
    harness.drop_at(pos);
    harness.run_steps(2);
}

/// Hover a widget and let the hover state paint.
fn mouse_hover(harness: &mut Harness<'static, App>, label: &str) {
    let pos = centre_of(harness, label);
    harness.hover_at(pos);
    harness.run_steps(2);
}

/// The same as [`mouse_click`], but targeting a widget by its value.
fn mouse_click_value(harness: &mut Harness<'static, App>, value: &str) {
    let pos = centre_of_value(harness, value);
    harness.hover_at(pos);
    harness.run_steps(1);
    harness.drag_at(pos);
    harness.run_steps(1);
    harness.drop_at(pos);
    harness.run_steps(2);
}

#[test]
fn the_mouse_walks_from_the_library_into_a_detail_page() {
    let mut harness = harness();
    harness.run_steps(3);
    assert_eq!(harness.state().screen(), &Screen::Home);

    // Hovering first must not disturb anything, and then a coordinate click
    // opens the card.
    mouse_hover(&mut harness, "Sintel");
    mouse_click(&mut harness, "Sintel");

    assert_eq!(
        harness.state().screen(),
        &Screen::Work(3),
        "a mouse click on the card should open its detail page"
    );

    harness.remove_cursor();
    harness.run_steps(1);
    harness.snapshot("e2e-detail");
}

#[test]
fn the_search_screen_answers_a_mouse_driven_query() {
    let mut harness = harness();
    harness.run_steps(3);

    mouse_click(&mut harness, "Discover"); // the rail item
    assert_eq!(harness.state().screen(), &Screen::Discover);

    // The demo backend answers from fixtures. The query is set programmatically
    // because two widgets share the "Search" label (the tab and the button);
    // everything else here is mouse-driven.
    harness.state_mut().set_search_query("nosferatu");
    harness.state_mut().submit_search();
    harness.run_steps(2);
    assert_eq!(
        harness.state().search_result_count(),
        1,
        "the demo backend knows one nosferatu"
    );

    // Move the pointer onto the result so the hover state is in the picture.
    mouse_hover(&mut harness, "Nosferatu");
    harness.snapshot("e2e-search");
}

#[test]
fn the_sort_combo_opens_and_selects_with_the_mouse() {
    let mut harness = harness();
    harness.run_steps(3);
    mouse_click(&mut harness, "Discover");

    // "a" matches both fixtures.
    harness.state_mut().set_search_query("a");
    harness.state_mut().submit_search();
    harness.run_steps(2);
    assert_eq!(harness.state().search_result_count(), 2);

    // Open the combo (its current text is an accessible value) and pick an
    // option from the popup by coordinate clicks.
    mouse_click_value(&mut harness, "Best match");
    mouse_click(&mut harness, "Most seeders");
    harness.run_steps(2);

    // The fixtures have unknown seeder counts, which are kept rather than
    // filtered, so both are still listed.
    assert_eq!(harness.state().search_result_count(), 2);

    harness.remove_cursor();
    harness.run_steps(1);
    harness.snapshot("e2e-search-sorted");
}
