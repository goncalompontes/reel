//! Guards that the app's Phosphor icons actually render.
//!
//! The icon font is installed as a *fallback* behind Inter for text, and Inter
//! ships hundreds of Private Use Area codepoints of its own. If Inter were not
//! stripped of them, it would shadow the icons and several would silently render
//! as text glyphs. This renders the icon set used across the app and pins it as a
//! snapshot, so that regression fails here instead of shipping as boxes.
//!
//! Run `UPDATE_SNAPSHOTS=1 cargo test -p reel-desktop --test icons` after
//! intentionally changing the set.

use egui_kittest::Harness;

#[test]
fn icon_gallery() {
    let installed = std::sync::Once::new();
    let items: &[(&str, &str)] = &[
        ("house", reel_desktop::icons::HOUSE),
        ("search", reel_desktop::icons::MAGNIFYING_GLASS),
        ("download", reel_desktop::icons::DOWNLOAD),
        ("gear", reel_desktop::icons::GEAR),
        ("plus", reel_desktop::icons::PLUS),
        ("play", reel_desktop::icons::PLAY),
        ("pause", reel_desktop::icons::PAUSE),
        ("star", reel_desktop::icons::STAR),
        ("film", reel_desktop::icons::FILM_STRIP),
        ("tv", reel_desktop::icons::TELEVISION),
        ("list", reel_desktop::icons::LIST),
        ("grid", reel_desktop::icons::SQUARES_FOUR),
        ("filter", reel_desktop::icons::FUNNEL),
        ("caret", reel_desktop::icons::CARET_DOWN),
        ("back", reel_desktop::icons::ARROW_LEFT),
        ("forward", reel_desktop::icons::ARROW_RIGHT),
    ];

    let mut harness = Harness::builder()
        .with_size((860.0, 260.0))
        .build_ui(move |ui| {
            installed.call_once(|| reel_desktop::install(ui.ctx()));
            ui.horizontal_wrapped(|ui| {
                for (name, glyph) in items {
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(*glyph).size(44.0));
                        ui.label(egui::RichText::new(*name).size(11.0));
                    });
                    ui.add_space(18.0);
                }
            });
        });
    harness.run_steps(3);
    harness.snapshot("icons");
}
