//! Test and demo fixtures.

use reel_catalog::model::EpisodeInfo;
use reel_catalog::{
    Artwork, ArtworkRef, CatalogEntry, FileInput, MediaKind, Metadata, SearchHit,
    WatchProgress, analyse,
};
use reel_core::model::{FileView, PeerView, StatsView, StreamTarget, TorrentView};

use crate::backend::LibraryItem;

/// A small, varied library: a partly watched film, a finished one, and a fresh
/// addition — enough for every row to have something in it.
pub fn sample_library() -> Vec<LibraryItem> {
    let now = now_unix();

    vec![
        item(
            1,
            "The.Matrix.1999.1080p.BluRay.x264-GROUP",
            "a3f1c0ffee1234567890abcdef1234567890abcd",
            true,
            100.0,
            vec![
                file(0, "The.Matrix.1999.1080p.mkv", 4_100_000_000, true),
                file(1, "The.Matrix.sample.mkv", 12_000_000, false),
                file(2, "The.Matrix.en.srt", 84_000, false),
            ],
            Catalog {
                metadata: Some(metadata(
                    "603",
                    "The Matrix",
                    1999,
                    "A computer hacker learns that his reality is a simulation.",
                )),
                watch: Some(WatchProgress {
                    position: 3_400.0,
                    duration: Some(8_160.0),
                    updated_at: now - 3_600,
                    file_name: Some("The.Matrix.1999.1080p.mkv".into()),
                    title: Some("The Matrix".into()),
                }),
            },
        ),
        item(
            2,
            "Big.Buck.Bunny.2008.720p.WEB-DL.AAC2.0.H.264",
            "b1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
            false,
            37.5,
            vec![
                file(0, "Big Buck Bunny.mp4", 700_000_000, true),
                file(1, "readme.nfo", 2_048, false),
            ],
            Catalog {
                metadata: Some(metadata(
                    "10378",
                    "Big Buck Bunny",
                    2008,
                    "A large rabbit takes revenge on three bullying rodents.",
                )),
                watch: None,
            },
        ),
        item(
            3,
            "Sintel (2010) [1080p]",
            "c9d8e7f6a5b4c3d2e1f00123456789abcdef0123",
            false,
            0.0,
            vec![file(0, "Sintel.2010.1080p.mkv", 1_900_000_000, true)],
            Catalog {
                metadata: Some(metadata(
                    "45745",
                    "Sintel",
                    2010,
                    "A lonely young woman searches for her lost dragon.",
                )),
                watch: Some(WatchProgress {
                    position: 900.0,
                    duration: Some(888.0),
                    updated_at: now - 90_000,
                    file_name: Some("Sintel.2010.1080p.mkv".into()),
                    title: Some("Sintel".into()),
                }),
            },
        ),
    ]
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn metadata(id: &str, title: &str, year: u16, overview: &str) -> Metadata {
    Metadata {
        kind: MediaKind::Movie,
        season_count: None,
        seasons: Vec::new(),
        episodes: Vec::new(),
        source: "static".into(),
        source_id: id.into(),
        title: title.into(),
        original_title: Some(title.to_string()),
        year: Some(year),
        overview: Some(overview.to_string()),
        tagline: None,
        genres: vec!["Animation".into(), "Fantasy".into()],
        runtime_minutes: Some(112),
        rating: Some(8.2),
        vote_count: Some(12_000),
        popularity: Some(30.0),
        artwork: Artwork {
            // Deliberately no local files: the demo has no downloaded posters,
            // so the UI falls back to generated artwork.
            poster: Some(ArtworkRef::new(format!("/{id}-poster.jpg"))),
            backdrop: Some(ArtworkRef::new(format!("/{id}-backdrop.jpg"))),
        },
    }
}

/// The catalog half of a fixture: what a provider and the watch history know.
struct Catalog {
    metadata: Option<Metadata>,
    watch: Option<WatchProgress>,
}

fn item(
    id: usize,
    name: &str,
    info_hash: &str,
    finished: bool,
    percent: f64,
    files: Vec<FileView>,
    catalog: Catalog,
) -> LibraryItem {
    let total: u64 = files.iter().map(|f| f.length).sum();
    let primary = files
        .iter()
        .filter(|f| f.is_video)
        .max_by_key(|f| f.length)
        .map(|f| f.id);

    let clean = reel_core::title::clean_title(name);

    // Derived from the same names the app would see, so the fixtures exercise
    // the real parser instead of a hand-written answer.
    let release = analyse(
        &files
            .iter()
            .map(|f| FileInput::new(f.id, f.path.clone(), f.length))
            .collect::<Vec<_>>(),
    );

    LibraryItem {
        downloading: false,
        torrent: TorrentView {
            id,
            info_hash: info_hash.to_string(),
            name: Some(name.to_string()),
            output_folder: "/tmp/reel-demo/".to_string(),
            state: "live".to_string(),
            finished,
            primary_file_id: primary,
            files,
            stats: StatsView {
                state: "live".to_string(),
                error: None,
                total_bytes: total,
                progress_bytes: (total as f64 * percent / 100.0) as u64,
                uploaded_bytes: 0,
                finished,
                percent,
                download_bps: if finished { 0 } else { 2_400_000 },
                upload_bps: 0,
                eta_seconds: if finished { None } else { Some(210) },
                peers: PeerView {
                    live: if finished { 0 } else { 12 },
                    connecting: 3,
                    queued: 1,
                    seen: 40,
                    dead: 8,
                },
            },
        },
        entry: CatalogEntry {
            torrent_id: id,
            info_hash: info_hash.to_string(),
            display_title: clean.title,
            year: clean.year,
            metadata: catalog.metadata,
            watch: catalog.watch,
            release,
            watch_by_file: Default::default(),
        },
    }
}

fn file(id: usize, path: &str, length: u64, included: bool) -> FileView {
    let name = path.rsplit('/').next().unwrap_or(path).to_string();
    FileView {
        id,
        path: path.to_string(),
        name: name.clone(),
        length,
        included,
        is_video: reel_core::is_video_file(&name),
        is_audio: reel_core::media::is_audio_file(&name),
        is_subtitle: reel_core::media::is_subtitle_file(&name),
        progress_bytes: if included { length / 3 } else { 0 },
        stream: StreamTarget {
            torrent_id: 0,
            file_id: id,
            name: name.clone(),
            mime: reel_core::mime_for_name(&name).to_string(),
            length,
            path: format!("/stream/0/{id}/{name}"),
            url: Some(format!("http://127.0.0.1:0/stream/0/{id}/{name}")),
        },
    }
}

/// Search results for the demo backend, so the search screen has something to
/// show without touching a network.
pub fn sample_search_hits() -> Vec<SearchHit> {
    let mut nosferatu = SearchHit::new("Nosferatu, eine Symphonie des Grauens", "archive.org");
    nosferatu.year = Some(1922);
    nosferatu.popularity = Some(432_789);
    nosferatu.size_bytes = Some(562_561_084);
    nosferatu.torrent_url =
        Some("https://archive.org/download/nosferatu/nosferatu_archive.torrent".to_string());
    nosferatu.detail = Some("archive.org \u{2022} nosferatu".to_string());

    let mut caligari = SearchHit::new("The Cabinet of Dr. Caligari", "archive.org");
    caligari.year = Some(1920);
    caligari.popularity = Some(95_000);
    caligari.size_bytes = Some(410_000_000);
    caligari.torrent_url =
        Some("https://archive.org/download/caligari/caligari_archive.torrent".to_string());
    caligari.detail = Some("archive.org \u{2022} caligari".to_string());

    vec![nosferatu, caligari]
}

/// A season pack: the case where fetching everything is exactly what you do not
/// want when you sit down to watch one episode.
pub fn sample_season_pack() -> LibraryItem {
    let episodes: Vec<FileView> = (0..3)
        .map(|n| {
            file(
                n,
                &format!("Some.Show.S01E{:02}.1080p.mkv", n + 1),
                1_500_000_000,
                true,
            )
        })
        .collect();

    item(
        9,
        "Some.Show.S01.1080p.WEB-DL.x264-GROUP",
        "d0d1d2d3d4d5d6d7d8d9dadbdcdddedf00112233",
        false,
        12.0,
        episodes,
        Catalog {
            metadata: Some(Metadata {
                kind: MediaKind::Series,
                season_count: Some(1),
                seasons: vec![reel_catalog::model::SeasonSummary {
                    number: 1,
                    air_date: Some("2024-01-08".into()),
                    episode_count: Some(3),
                }],
                episodes: (1..=3)
                    .map(|n| EpisodeInfo {
                        season: 1,
                        number: n,
                        name: Some(format!("Episode {n}")),
                        runtime_minutes: Some(48),
                        ..Default::default()
                    })
                    .collect(),
                source: "static".into(),
                source_id: "season".into(),
                title: "Some Show".into(),
                year: Some(2024),
                overview: Some("A show with three episodes.".into()),
                genres: vec!["Drama".into()],
                runtime_minutes: Some(48),
                rating: Some(7.9),
                vote_count: Some(2_000),
                popularity: Some(20.0),
                artwork: Artwork::default(),
                ..Default::default()
            }),
            watch: None,
        },
    )
}

/// A pack holding two seasons, which is the case a single-season view cannot
/// describe: the file names say which season each episode belongs to, and the
/// interface has to keep them apart.
pub fn sample_two_season_pack() -> LibraryItem {
    let episodes: Vec<FileView> = [(1u32, 1u32), (1, 2), (2, 1), (2, 2)]
        .into_iter()
        .enumerate()
        .map(|(index, (season, episode))| {
            file(
                index,
                &format!("Two.Seasons.S{season:02}E{episode:02}.1080p.mkv"),
                1_400_000_000,
                true,
            )
        })
        .collect();

    item(
        10,
        "Two.Seasons.S01.S02.1080p.WEB-DL.x264-GROUP",
        "e0e1e2e3e4e5e6e7e8e9eaebecedeeef00112233",
        false,
        5.0,
        episodes,
        Catalog {
            metadata: Some(Metadata {
                kind: MediaKind::Series,
                season_count: Some(2),
                seasons: vec![
                    reel_catalog::model::SeasonSummary {
                        number: 1,
                        air_date: Some("2022-09-01".into()),
                        episode_count: Some(2),
                    },
                    reel_catalog::model::SeasonSummary {
                        number: 2,
                        air_date: Some("2023-09-01".into()),
                        episode_count: Some(2),
                    },
                ],
                episodes: [(1u32, 1u32), (1, 2), (2, 1), (2, 2)]
                    .into_iter()
                    .map(|(season, number)| EpisodeInfo {
                        season,
                        number,
                        name: Some(format!("Season {season} Episode {number}")),
                        runtime_minutes: Some(52),
                        ..Default::default()
                    })
                    .collect(),
                source: "static".into(),
                source_id: "two-seasons".into(),
                title: "Two Seasons".into(),
                year: Some(2022),
                overview: Some("A show with two seasons in one torrent.".into()),
                genres: vec!["Drama".into()],
                runtime_minutes: Some(52),
                rating: Some(8.1),
                vote_count: Some(5_000),
                popularity: Some(40.0),
                artwork: Artwork::default(),
                ..Default::default()
            }),
            watch: None,
        },
    )
}
