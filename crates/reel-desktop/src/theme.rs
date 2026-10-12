//! Visual identity: palette, typography, generated artwork, and egui style
//! setup.

use egui::{Color32, CornerRadius, Rect, Shape, Stroke, epaint::Mesh, epaint::Vertex, pos2};

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

// --- radii ---------------------------------------------------------------

pub const CARD_RADIUS: CornerRadius = CornerRadius::same(10);
pub const PANEL_RADIUS: CornerRadius = CornerRadius::same(14);
/// A small pill, for chips and badges.
pub const CHIP_RADIUS: CornerRadius = CornerRadius::same(255);
/// Small controls (buttons inside rows).
pub const CONTROL_RADIUS: CornerRadius = CornerRadius::same(7);

// --- typography ----------------------------------------------------------

/// The name of the semibold family (`Montserrat-SemiBold`), used for titles.
pub const SEMIBOLD: &str = "montserrat-semibold";

/// The family a heading should use for real weight.
///
/// Fonts take effect at the start of the *next* pass after `set_fonts`, so a
/// view drawn before [`apply`] has run falls back to the proportional family
/// rather than panicking on an unbound family.
pub fn semibold(ctx: &egui::Context) -> egui::FontFamily {
    let name = egui::FontFamily::Name(SEMIBOLD.into());
    if ctx.fonts(|fonts| fonts.definitions().families.contains_key(&name)) {
        name
    } else {
        egui::FontFamily::Proportional
    }
}

/// Install the dark theme, the fonts, and the icon set.
pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);

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

/// The font family name for ordinary text.
const TEXT_FONT: &str = "montserrat";

fn install_fonts(ctx: &egui::Context) {
    ctx.set_fonts(font_definitions());
}

/// Build the font stack: Montserrat for text, Phosphor for icons, egui's
/// defaults as a last-resort fallback.
fn font_definitions() -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();

    // Montserrat first, so it wins for ordinary text; egui's defaults stay after
    // it as a fallback for anything it lacks.
    fonts.font_data.insert(
        TEXT_FONT.to_owned(),
        egui::FontData::from_static(include_bytes!(
            "../assets/fonts/Montserrat-Regular.ttf"
        ))
        .into(),
    );
    if let Some(keys) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        keys.insert(0, TEXT_FONT.to_owned());
    } else {
        fonts
            .families
            .insert(egui::FontFamily::Proportional, vec![TEXT_FONT.to_owned()]);
    }

    // Icons ride along as a proportional fallback, ahead of egui's own symbol
    // and emoji fonts (which also claim Private Use codepoints).
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    if let Some(keys) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        if let Some(position) = keys.iter().position(|key| key == "phosphor") {
            let phosphor = keys.remove(position);
            keys.insert(1, phosphor);
        }
    }

    // The whole app is Montserrat, including the spots that ask for a monospace
    // font, so nothing renders in a different typeface.
    if let Some(keys) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
        keys.retain(|key| key != TEXT_FONT && key != "phosphor");
        keys.insert(0, TEXT_FONT.to_owned());
        keys.insert(1, "phosphor".to_owned());
    }

    // A real semibold family for headings, built from the final proportional
    // order so icons resolve inside headings too.
    fonts.font_data.insert(
        SEMIBOLD.to_owned(),
        egui::FontData::from_static(include_bytes!(
            "../assets/fonts/Montserrat-SemiBold.ttf"
        ))
        .into(),
    );
    let mut semibold_keys = fonts
        .families
        .get(&egui::FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    semibold_keys.remove(0); // drop the regular; SemiBold replaces it
    semibold_keys.insert(0, SEMIBOLD.to_owned());
    fonts
        .families
        .insert(egui::FontFamily::Name(SEMIBOLD.into()), semibold_keys);

    fonts
}

/// A vertical gradient scrim over the lower part of a poster, with the bottom
/// corners rounded to match the card.
///
/// Painting a mesh keeps the falloff smooth; the mesh's left/right edge follows
/// the corner arc over the last `radius` pixels so the gradient ends in the
/// same rounded corners as the artwork rather than square ones.
pub fn paint_scrim(painter: &egui::Painter, rect: Rect, radius: CornerRadius, strength: u8) {
    let radius = radius.nw as f32;
    let bottom_color = Color32::from_black_alpha(strength);
    let top_color = Color32::TRANSPARENT;

    // Sample densely through the corner arc and coarsely above it, so the mesh
    // edge follows the rounded corner instead of cutting a straight chord (which
    // would leave a sliver of artwork showing at each bottom corner).
    let corner = radius.min(rect.height());
    let straight = (rect.height() - corner).max(0.0);
    let mut samples: Vec<f32> = vec![rect.top()];
    let coarse = ((straight / 24.0).ceil() as usize).max(1);
    for i in 1..=coarse {
        samples.push(rect.top() + straight * i as f32 / coarse as f32);
    }
    let mut y = rect.bottom() - corner;
    while y < rect.bottom() {
        y = (y + 1.0).min(rect.bottom());
        if samples.last().is_none_or(|last| y - last >= 0.5) {
            samples.push(y);
        }
    }
    if samples.last().copied() != Some(rect.bottom()) {
        samples.push(rect.bottom());
    }

    let uv = egui::epaint::WHITE_UV;
    let mut mesh = Mesh::default();
    for y in &samples {
        let dy = (rect.bottom() - y).max(0.0);
        let inset = if dy < radius {
            radius
                - (radius * radius - (radius - dy) * (radius - dy))
                    .max(0.0)
                    .sqrt()
        } else {
            0.0
        };
        let t = (y - rect.top()) / rect.height().max(1.0);
        let color = lerp_color(top_color, bottom_color, t);
        mesh.vertices.push(Vertex {
            pos: pos2(rect.left() + inset, *y),
            uv,
            color,
        });
        mesh.vertices.push(Vertex {
            pos: pos2(rect.right() - inset, *y),
            uv,
            color,
        });
    }
    for i in 0..samples.len().saturating_sub(1) {
        let a = (i * 2) as u32;
        let b = a + 1;
        let c = a + 2;
        let d = a + 3;
        mesh.indices.extend_from_slice(&[a, b, c, c, b, d]);
    }

    painter.add(Shape::mesh(mesh));
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        lerp(a.r(), b.r()),
        lerp(a.g(), b.g()),
        lerp(a.b(), b.b()),
        lerp(a.a(), b.a()),
    )
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

    #[test]
    fn montserrat_precedes_the_icon_font_and_is_used_for_monospace_too() {
        let fonts = font_definitions();
        let keys = &fonts.families[&egui::FontFamily::Proportional];
        assert_eq!(keys.first().map(String::as_str), Some("montserrat"));
        let phosphor = keys.iter().position(|key| key == "phosphor").unwrap();
        let emoji = keys
            .iter()
            .position(|key| key == "emoji-icon-font")
            .unwrap();
        assert!(
            phosphor < emoji,
            "Phosphor must win Private Use codepoints: {keys:?}"
        );

        // The monospace slots use Montserrat too.
        let mono = &fonts.families[&egui::FontFamily::Monospace];
        assert_eq!(mono.first().map(String::as_str), Some("montserrat"));
        assert!(mono.iter().any(|key| key == "phosphor"));
    }
}
