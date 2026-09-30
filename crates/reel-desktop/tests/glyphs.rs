//! Guards the set of icon glyphs the app uses.
//!
//! egui renders with a bundled font stack that does **not** cover every symbol:
//! `←`, `↓`, `●`, `▼`, `✓` and `─` all render as empty boxes, while `⬅`, `⬇`,
//! `▶`, `⏸`, `⏮`, `«»` and the emoji do render. That is easy to get wrong and
//! invisible in code review, so this renders a gallery and pins it as a
//! snapshot: a font or egui upgrade that drops coverage fails here instead of
//! shipping boxes to the user.
//!
//! Run `UPDATE_SNAPSHOTS=1 cargo test -p reel-desktop --test glyphs` after
//! intentionally changing the gallery.

use egui_kittest::Harness;

#[test]
fn glyph_gallery() {
    let candidates: &[(&str, &str)] = &[
        ("U+25B6 play", "\u{25b6}"),
        ("U+25C0 back", "\u{25c0}"),
        ("U+25BC down", "\u{25bc}"),
        ("U+25B2 up", "\u{25b2}"),
        ("U+23F8 pause", "\u{23f8}"),
        ("U+23EE prev", "\u{23ee}"),
        ("U+23ED next", "\u{23ed}"),
        ("U+23EA rew", "\u{23ea}"),
        ("U+23E9 ff", "\u{23e9}"),
        ("U+2190 larr", "\u{2190}"),
        ("U+2193 darr", "\u{2193}"),
        ("U+2B05 larr2", "\u{2b05}"),
        ("U+2B07 darr2", "\u{2b07}"),
        ("U+21A9 return", "\u{21a9}"),
        ("U+00AB laquo", "\u{ab}"),
        ("U+00BB raquo", "\u{bb}"),
        ("U+2039 lsaquo", "\u{2039}"),
        ("U+203A rsaquo", "\u{203a}"),
        ("U+2500 rule", "\u{2500}"),
        ("U+00D7 times", "\u{d7}"),
        ("U+2715 x", "\u{2715}"),
        ("U+1F5D1 trash", "\u{1f5d1}"),
        ("U+1F4CB clip", "\u{1f4cb}"),
        ("U+1F50A spk", "\u{1f50a}"),
        ("U+1F3AC film", "\u{1f3ac}"),
        ("U+1F3B5 note", "\u{1f3b5}"),
        ("U+1F4C4 doc", "\u{1f4c4}"),
        ("U+1F4AC say", "\u{1f4ac}"),
        ("U+2022 dot", "\u{2022}"),
        ("U+25CF disc", "\u{25cf}"),
        ("U+2713 check", "\u{2713}"),
        ("U+FF0B plus", "\u{ff0b}"),
        ("U+002B plus2", "+"),
        ("U+21BB reload", "\u{21bb}"),
    ];

    let mut harness = Harness::builder().with_size((560.0, 760.0)).build_ui(|ui| {
        ui.label(egui::RichText::new("glyph gallery").size(18.0).strong());
        ui.add_space(6.0);
        for (name, glyph) in candidates {
            ui.horizontal(|ui| {
                ui.add_sized(
                    egui::Vec2::new(140.0, 18.0),
                    egui::Label::new(egui::RichText::new(*name).monospace().size(12.0)),
                );
                ui.label(egui::RichText::new(format!("{glyph}  {glyph}{glyph}")).size(18.0));
            });
        }
    });
    harness.run();
    harness.snapshot("glyphs");
}
