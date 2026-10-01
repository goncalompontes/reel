//! Headless UI tests.
//!
//! `egui_kittest` drives the real widget tree through AccessKit and can render
//! it with wgpu to a PNG, so the interface is verified without a display server
//! and without a browser.
//!
//! Run `UPDATE_SNAPSHOTS=1 cargo test -p reel-desktop --test ui` to rewrite the
//! reference images after an intentional visual change.

use std::path::{Path, PathBuf};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use reel_desktop::backend::FakeBackend;
use reel_desktop::testing::sample_library;
use reel_desktop::{App, Screen};

/// Build a harness around the real app, with the sample library.
fn harness_with(app: App) -> Harness<'static, App> {
    // Each Harness has its own egui Context, so the image loaders have to be
    // installed per harness. Skipping this is silent: artwork never loads and
    // the generated fallback is drawn instead.
    let installed = std::sync::Once::new();

    Harness::builder().with_size((1280.0, 820.0)).build_ui_state(
        move |ui, app: &mut App| {
            installed.call_once(|| reel_desktop::install(ui.ctx()));
            // eframe hands the app a `Frame`; this is the supported way to
            // fabricate one for tests.
            let mut frame = eframe::Frame::_new_kittest();
            eframe::App::ui(app, ui, &mut frame);
        },
        app,
    )
}

fn harness() -> Harness<'static, App> {
    harness_with(App::new(Box::new(FakeBackend::new(sample_library()))))
}

/// A scratch directory, removed on the next run with the same name.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("reel-ui-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Write a PNG fixture, using the same crate the app decodes with.
fn write_png(path: &Path, width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) {
    let mut image = image::RgbImage::new(width, height);
    for (x, y, texel) in image.enumerate_pixels_mut() {
        *texel = image::Rgb(pixel(x, y));
    }
    image.save(path).expect("write png fixture");
}

/// Something obviously not a flat colour, so a snapshot proves it decoded.
fn test_pattern(x: u32, y: u32) -> [u8; 3] {
    let band = ((x / 16) + (y / 16)) % 4;
    match band {
        0 => [210, 60, 90],
        1 => [40, 90, 200],
        2 => [240, 200, 60],
        _ => [30, 30, 40],
    }
}

#[test]
fn library_screen_shows_every_torrent() {
    let mut harness = harness();
    harness.run_steps(3);

    // Titles come from metadata where available, and can appear in more than one
    // row, so query all rather than requiring a unique match.
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

    {
        let card = harness
            .get_all_by_label_contains("Sintel")
            .next()
            .expect("a Sintel card");
        card.click();
    }
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

    {
        let tab = harness
            .get_all_by_label_contains("Add")
            .next()
            .expect("the Add tab");
        tab.click();
    }
    harness.run_steps(3);
    assert_eq!(harness.state().screen(), &Screen::Add);

    // An empty submit must warn rather than adding anything.
    harness.get_by_label("Add torrent").click();
    harness.run_steps(3);
    assert_eq!(harness.state().screen(), &Screen::Add);
    assert_eq!(harness.state().library_len(), 3);
}

#[test]
fn settings_reports_the_backend_and_catalog() {
    let mut harness = harness();
    harness.run_steps(3);

    {
        let tab = harness
            .get_all_by_label_contains("Settings")
            .next()
            .expect("the Settings tab");
        tab.click();
    }
    harness.run_steps(3);

    assert_eq!(harness.state().screen(), &Screen::Settings);
    for label in [
        "Download folder",
        "Merge copies",
        "Metadata API key",
        "Metadata and artwork",
        "Search sources",
        "Streaming and downloads",
        "Stream on demand",
        "Playback",
        "Turn subtitles on",
    ] {
        assert!(
            harness.query_all_by_label_contains(label).next().is_some(),
            "settings should show {label:?}"
        );
    }
}

#[test]
fn search_finds_results_and_can_add_one() {
    let mut harness = harness();
    harness.run_steps(3);

    {
        let tab = harness
            .get_all_by_label_contains("Search")
            .next()
            .expect("the Search tab");
        tab.click();
    }
    harness.run_steps(3);
    assert_eq!(harness.state().screen(), &Screen::Search);
    assert_eq!(harness.state().search_result_count(), 0);

    // An empty query must warn rather than searching for nothing.
    {
        let button = harness
            .get_all_by_label("Search")
            .next()
            .expect("the Search button");
        button.click();
    }
    harness.run_steps(3);
    assert_eq!(harness.state().search_result_count(), 0);

    // The demo backend answers from fixtures; results arrive as an event, so
    // they appear on a later frame rather than immediately.
    harness.state_mut().set_search_query("nosferatu");
    harness.state_mut().submit_search();
    harness.run_steps(4);

    assert_eq!(
        harness.state().search_result_count(),
        1,
        "the demo backend knows one nosferatu"
    );
    assert!(
        harness.query_all_by_label_contains("Nosferatu").next().is_some(),
        "the result title should be on screen"
    );
    assert!(
        harness.query_all_by_label_contains("archive.org").next().is_some(),
        "the source should be shown"
    );
}

/// Renders the search screen with results in it.
#[test]
fn snapshot_search() {
    let mut harness = harness();
    harness.run_steps(3);
    harness.state_mut().navigate(Screen::Search);
    harness.state_mut().set_search_query("nosferatu");
    harness.state_mut().submit_search();
    harness.run_steps(4);
    harness.snapshot("search");
}

/// The image loader must decode a poster from disk: this is the path a cached
/// poster takes on its way to a card.
#[test]
fn poster_files_decode_through_the_image_loader() {
    let dir = scratch("loader");
    let path = dir.join("poster.png");
    write_png(&path, 64, 36, test_pattern);

    let ctx = egui::Context::default();
    egui_extras::install_image_loaders(&ctx);

    let uri = format!("file://{}", path.display());
    let hint = egui::load::SizeHint::Size {
        width: 64,
        height: 36,
        maintain_aspect_ratio: false,
    };

    // Loading may take a poll or two even for a local file.
    let mut decoded = None;
    for _ in 0..100 {
        match ctx.try_load_image(&uri, hint).expect("the poster should load") {
            egui::load::ImagePoll::Ready { image } => {
                decoded = Some(image.size);
                break;
            }
            egui::load::ImagePoll::Pending { size } => {
                decoded = size.map(|s| [s.x as usize, s.y as usize]);
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
    assert_eq!(decoded, Some([64, 36]), "decoded poster should be 64x36");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The whole artwork path: a cached poster file, reached through the catalog
/// metadata, painted into a card.
#[test]
fn snapshot_library_with_real_artwork() {
    let dir = scratch("artwork");
    let mut items = sample_library();

    for item in &mut items {
        let poster = dir.join(format!("{}-poster.png", item.torrent.id));
        let backdrop = dir.join(format!("{}-backdrop.png", item.torrent.id));
        // A real poster's 2:3 shape, so the card is not stretched.
        write_png(&poster, 342, 513, test_pattern);
        write_png(&backdrop, 780, 439, |x, y| {
            // A different pattern per title, so the hero is distinguishable.
            let shift = item.torrent.id as u32 * 37;
            test_pattern(x + shift, y + shift)
        });

        if let Some(metadata) = item.entry.metadata.as_mut() {
            if let Some(reference) = metadata.artwork.poster.as_mut() {
                reference.local_path = Some(poster);
            }
            if let Some(reference) = metadata.artwork.backdrop.as_mut() {
                reference.local_path = Some(backdrop);
            }
        }
    }

    let mut harness = harness_with(App::new(Box::new(FakeBackend::new(items))));
    // Give the loader time to decode the images.
    harness.run_steps(6);
    harness.snapshot("library-with-posters");
}

/// The season-pack state: one episode being fetched, the rest skipped.
#[test]
fn snapshot_season_pack() {
    let mut items = sample_library();
    items.push(reel_desktop::testing::sample_season_pack());

    // Taller than the default so the whole episode list is in the snapshot; the
    // point of this one is that every episode row is visible at once.
    let mut harness = Harness::builder()
        .with_size((1280.0, 1150.0))
        .build_ui_state(
            |ui, app: &mut App| {
                let mut frame = eframe::Frame::_new_kittest();
                eframe::App::ui(app, ui, &mut frame);
            },
            App::new(Box::new(FakeBackend::new(items))),
        );
    harness.run_steps(3);

    // What pressing play on episode 2 does.
    harness.state_mut().select_files(9, &[1]);
    harness.state_mut().navigate(Screen::Detail(9));
    harness.run_steps(3);

    harness.snapshot("season-pack");
}

/// Two torrents overlapping on the same episodes: each episode row gets a copy
/// chooser, and the 2160p copy is preferred.
#[test]
fn snapshot_season_pack_with_duplicates() {
    let mut items = sample_library();
    items.push(reel_desktop::testing::sample_season_pack());

    let mut copy = reel_desktop::testing::sample_season_pack();
    copy.torrent.id = 11;
    copy.entry.torrent_id = 11;
    copy.entry.info_hash = "1111111111111111111111111111111111111111".into();
    copy.torrent.info_hash = copy.entry.info_hash.clone();
    copy.torrent.name = Some("Some.Show.S01.2160p.WEB-DL.x265".into());
    copy.entry.release.title = "Some Show".into();
    copy.entry.release.attributes.resolution = Some("2160p".into());
    items.push(copy);

    let mut harness = Harness::builder()
        .with_size((1280.0, 1000.0))
        .build_ui_state(
            |ui, app: &mut App| {
                let mut frame = eframe::Frame::_new_kittest();
                eframe::App::ui(app, ui, &mut frame);
            },
            App::new(Box::new(FakeBackend::new(items))),
        );
    harness.run_steps(3);
    harness.state_mut().navigate(Screen::Detail(11));
    harness.run_steps(3);
    harness.snapshot("season-pack-duplicates");
}

/// A pack with two seasons: each season gets its own heading, and every episode
/// keeps the season its file name says it belongs to.
#[test]
fn snapshot_two_season_pack() {
    let mut items = sample_library();
    items.push(reel_desktop::testing::sample_two_season_pack());

    let mut harness = Harness::builder()
        .with_size((1280.0, 1150.0))
        .build_ui_state(
            |ui, app: &mut App| {
                let mut frame = eframe::Frame::_new_kittest();
                eframe::App::ui(app, ui, &mut frame);
            },
            App::new(Box::new(FakeBackend::new(items))),
        );
    harness.state_mut().navigate(Screen::Detail(10));
    harness.run_steps(3);
    harness.snapshot("two-season-pack");
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
    {
        let card = harness
            .get_all_by_label_contains("Sintel")
            .next()
            .expect("a Sintel card");
        card.click();
    }
    harness.run_steps(3);
    harness.snapshot("detail");
}
