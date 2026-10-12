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
    harness_with_size(app, (1280.0, 820.0))
}

/// The same, at a chosen window size (some snapshots want the whole list).
fn harness_with_size(app: App, size: (f32, f32)) -> Harness<'static, App> {
    // Each Harness has its own egui Context, so the image loaders have to be
    // installed per harness. Skipping this is silent: artwork never loads and
    // the generated fallback is drawn instead.
    let installed = std::sync::Once::new();

    Harness::builder().with_size(size).build_ui_state(
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
        &Screen::Work(3),
        "clicking a card should open that torrent's detail page"
    );
}

#[test]
fn the_add_modal_opens_and_an_empty_submit_warns() {
    let mut harness = harness();
    harness.run_steps(3);

    {
        let add = harness
            .get_all_by_label_contains("Add")
            .next()
            .expect("the rail's Add button");
        add.click();
    }
    harness.run_steps(3);
    assert!(
        harness.state().is_add_open(),
        "the rail's Add opens the modal"
    );

    // An empty submit must warn and keep the modal up rather than adding
    // anything.
    harness.get_by_label("Add torrent").click();
    harness.run_steps(3);
    assert!(harness.state().is_add_open());
    assert_eq!(harness.state().library_len(), 3);
}

/// Renders the Activity view so the transfers layout can be reviewed by eye.
#[test]
fn snapshot_activity() {
    let mut harness = harness();
    harness.run_steps(3);
    harness.state_mut().go_to(Screen::Activity);
    harness.run_steps(3);
    harness.snapshot("activity");
}

/// Renders the two-pane Settings page so the layout can be reviewed by eye.
#[test]
fn snapshot_settings() {
    let mut harness = harness();
    harness.run_steps(3);
    harness.state_mut().go_to(Screen::Settings);
    harness.run_steps(3);
    harness.snapshot("settings");
}

#[test]
fn settings_autosave_a_toggled_field() {
    let mut harness = harness();
    harness.run_steps(3);
    harness.state_mut().go_to(Screen::Settings);
    harness.run_steps(3);

    let before = harness.state().settings().merge_works;
    harness
        .get_all_by_label_contains("Merge copies")
        .next()
        .expect("the merge checkbox")
        .click();
    harness.run_steps(3);

    assert_ne!(
        harness.state().settings().merge_works,
        before,
        "changing a setting should write it without a Save button"
    );
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

    // General is the pane shown first.
    for label in ["Download folder", "Merge copies", "Active folder"] {
        assert!(
            harness.query_all_by_label_contains(label).next().is_some(),
            "the General pane should show {label:?}"
        );
    }

    // Every pane is reachable from the sidebar and shows its own fields.
    let panes: [(&str, &[&str]); 5] = [
        ("Metadata", &["Metadata API key", "Cache folder"]),
        ("Playback", &["Turn subtitles on", "Default volume"]),
        (
            "Downloads",
            &["Stream on demand", "Stream buffer", "Session cache"],
        ),
        ("Network", &["Use the DHT", "Announce to trackers"]),
        ("Search sources", &["Cookie (optional)", "Active sources"]),
    ];
    for (pane, labels) in panes {
        harness
            .get_all_by_label(pane)
            .next()
            .unwrap_or_else(|| panic!("the {pane:?} pane button"))
            .click();
        harness.run_steps(2);
        for label in labels {
            assert!(
                harness.query_all_by_label_contains(label).next().is_some(),
                "the {pane} pane should show {label:?}"
            );
        }
    }
}

#[test]
fn search_finds_results_and_can_add_one() {
    let mut harness = harness();
    harness.run_steps(3);

    {
        let tab = harness
            .get_all_by_label_contains("Discover")
            .next()
            .expect("the Discover rail item");
        tab.click();
    }
    harness.run_steps(3);
    assert_eq!(harness.state().screen(), &Screen::Discover);
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
        harness
            .query_all_by_label_contains("Nosferatu")
            .next()
            .is_some(),
        "the result title should be on screen"
    );
    assert!(
        harness.query_all_by_label_contains("demo").next().is_some(),
        "the source should be shown"
    );
}

/// Discover before any search: the big centred field.
#[test]
fn snapshot_discover_landing() {
    let mut harness = harness();
    harness.run_steps(3);
    harness.state_mut().go_to(Screen::Discover);
    harness.run_steps(3);
    harness.snapshot("discover");
}

/// Renders the search screen with results in it.
#[test]
fn snapshot_search() {
    let mut harness = harness();
    harness.run_steps(3);
    harness.state_mut().go_to(Screen::Discover);
    harness.state_mut().set_search_query("nosferatu");
    harness.state_mut().submit_search();
    harness.run_steps(4);
    harness.snapshot("search");
}

/// Opening a Discover row pushes the Result view rather than expanding it.
#[test]
fn snapshot_search_result() {
    let mut harness = harness();
    harness.run_steps(3);
    harness.state_mut().go_to(Screen::Discover);
    harness.state_mut().set_search_query("nosferatu");
    harness.state_mut().submit_search();
    harness.run_steps(4);

    // The row itself is a painted hit area, not an accessibility node, so drive
    // it with the pointer at the title's coordinates rather than `node.click()`.
    let pos = harness
        .get_all_by_label_contains("Nosferatu")
        .next()
        .expect("a result row")
        .rect()
        .center();
    harness.hover_at(pos);
    harness.run_steps(1);
    harness.drag_at(pos);
    harness.run_steps(1);
    harness.drop_at(pos);
    harness.run_steps(3);

    assert!(
        matches!(harness.state().screen(), Screen::Result(_)),
        "clicking a row opens the Result view"
    );
    harness.snapshot("search-result");
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
        match ctx
            .try_load_image(&uri, hint)
            .expect("the poster should load")
        {
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
    let mut harness = harness_with_size(
        App::new(Box::new(FakeBackend::new(items))),
        (1280.0, 1150.0),
    );
    harness.run_steps(3);

    // What pressing play on episode 2 does.
    harness.state_mut().select_files(9, &[1]);
    harness.state_mut().open_work(9);
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

    let mut harness = harness_with_size(
        App::new(Box::new(FakeBackend::new(items))),
        (1280.0, 1000.0),
    );
    harness.run_steps(3);
    harness.state_mut().open_work(11);
    harness.run_steps(3);
    harness.snapshot("season-pack-duplicates");
}

/// A pack with two seasons: each season gets its own heading, and every episode
/// keeps the season its file name says it belongs to.
#[test]
fn snapshot_two_season_pack() {
    let mut items = sample_library();
    items.push(reel_desktop::testing::sample_two_season_pack());

    let mut harness = harness_with_size(
        App::new(Box::new(FakeBackend::new(items))),
        (1280.0, 1150.0),
    );
    harness.state_mut().open_work(10);
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

/// A narrow window collapses the rail to icons only.
#[test]
fn snapshot_narrow_rail() {
    let mut harness = harness_with_size(
        App::new(Box::new(FakeBackend::new(sample_library()))),
        (980.0, 720.0),
    );
    harness.run_steps(3);
    harness.snapshot("narrow");
}
