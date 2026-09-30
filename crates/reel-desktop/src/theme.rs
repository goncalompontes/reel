//! Visual identity: palette, generated artwork, and egui style setup.

use egui::{Color32, CornerRadius, Stroke};

pub const BG: Color32 = Color32::from_rgb(0x0b, 0x0c, 0x11);
pub const SURFACE: Color32 = Color32::from_rgb(0x15, 0x17, 0x1f);
pub const SURFACE_RAISED: Color32 = Color32::from_rgb(0x1e, 0x21, 0x2c);
pub const BORDER: Color32 = Color32::from_rgb(0x2a, 0x2e, 0x3c);
pub const TEXT: Color32 = Color32::from_rgb(0xe9, 0xea, 0xf0);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x9a, 0xa0, 0xb4);
pub const ACCENT: Color32 = Color32::from_rgb(0x5b, 0x9d, 0xff);
pub const ACCENT_DEEP: Color32 = Color32::from_rgb(0x2b, 0x5c, 0xbf);
pub const OK: Color32 = Color32::from_rgb(0x54, 0xd1, 0x8a);
pub const WARN: Color32 = Color32::from_rgb(0xf2, 0xb5, 0x4b);
pub const DANGER: Color32 = Color32::from_rgb(0xf2, 0x6d, 0x6d);

pub const CARD_RADIUS: CornerRadius = CornerRadius::same(10);
pub const PANEL_RADIUS: CornerRadius = CornerRadius::same(14);

/// Install the dark theme.
pub fn apply(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = SURFACE;
    visuals.extreme_bg_color = Color32::from_rgb(0x08, 0x09, 0x0d);
    visuals.faint_bg_color = SURFACE;
    visuals.override_text_color = Some(TEXT);
    visuals.selection.bg_fill = ACCENT_DEEP;
    visuals.widgets.noninteractive.bg_fill = SURFACE;
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);
    visuals.widgets.inactive.bg_fill = SURFACE_RAISED;
    visuals.widgets.inactive.weak_bg_fill = SURFACE_RAISED;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(0x28, 0x2d, 0x3c);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x28, 0x2d, 0x3c);
    visuals.widgets.active.bg_fill = ACCENT_DEEP;
    visuals.widgets.active.weak_bg_fill = ACCENT_DEEP;
    ctx.set_visuals(visuals);

    // Apply the spacing tweaks to every theme variant so switching the system
    // light/dark preference cannot quietly produce a different layout.
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(10.0, 10.0);
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style.spacing.window_margin = egui::Margin::same(14);
    });
}

/// Deterministic artwork colour for a title.
///
/// Real poster art arrives with the catalogue/metadata layer; until then a
/// stable hue per title keeps the grid readable and obviously intentional.
pub fn poster_color(seed: &str) -> Color32 {
    let hue = (hash(seed) % 360) as f32;
    // Saturate slightly differently per title so neighbours do not look cloned.
    let saturation = 0.34 + ((hash(&format!("{seed}s")) % 12) as f32) / 100.0;
    let value = 0.36 + ((hash(&format!("{seed}v")) % 10) as f32) / 100.0;
    hsv_to_rgb(hue, saturation, value)
}

/// A darker companion shade, for gradients and borders.
pub fn poster_shade(seed: &str) -> Color32 {
    let hue = (hash(seed) % 360) as f32;
    hsv_to_rgb(hue, 0.45, 0.16)
}

fn hash(input: &str) -> u32 {
    // FNV-1a: small, deterministic, good enough for colour picking.
    let mut hash: u32 = 0x811c_9dc5;
    for byte in input.as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn hsv_to_rgb(hue: f32, saturation: f32, value: f32) -> Color32 {
    let c = value * saturation;
    let h = (hue % 360.0) / 60.0;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = value - c;
    Color32::from_rgb(
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

/// Colour for a torrent's state badge.
pub fn state_color(state: &str) -> Color32 {
    match state {
        "live" => OK,
        "paused" => WARN,
        "error" => DANGER,
        "initializing" => TEXT_DIM,
        _ => TEXT_DIM,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poster_colours_are_deterministic_and_distinct() {
        assert_eq!(poster_color("The Matrix"), poster_color("The Matrix"));
        assert_ne!(poster_color("The Matrix"), poster_color("Sintel"));
        // Always fully opaque and reasonably dark, so white text stays readable.
        for title in ["a", "Big Buck Bunny", "Sintel", "Elephants Dream"] {
            let c = poster_color(title);
            assert_eq!(c.a(), 255);
            let luma = (c.r() as u32 + c.g() as u32 + c.b() as u32) / 3;
            assert!(luma < 160, "{title} too bright: {c:?}");
        }
    }

    #[test]
    fn hsv_endpoints() {
        assert_eq!(hsv_to_rgb(0.0, 1.0, 1.0), Color32::from_rgb(255, 0, 0));
        assert_eq!(hsv_to_rgb(120.0, 1.0, 1.0), Color32::from_rgb(0, 255, 0));
        assert_eq!(hsv_to_rgb(240.0, 1.0, 1.0), Color32::from_rgb(0, 0, 255));
    }
}
