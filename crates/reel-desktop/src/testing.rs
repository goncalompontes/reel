//! Test and demo fixtures.

use reel_core::model::{FileView, PeerView, StatsView, StreamTarget, TorrentView};

/// A small, varied library: a finished movie, a partially downloaded one, and
/// a torrent with a sample file that should not be chosen as the feature.
pub fn sample_torrents() -> Vec<TorrentView> {
    vec![
        torrent(
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
        ),
        torrent(
            2,
            "Big.Buck.Bunny.2008.720p.WEB-DL.AAC2.0.H.264",
            "b1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
            false,
            37.5,
            vec![
                file(0, "Big Buck Bunny.mp4", 700_000_000, true),
                file(1, "readme.nfo", 2_048, false),
            ],
        ),
        torrent(
            3,
            "Sintel (2010) [1080p]",
            "c9d8e7f6a5b4c3d2e1f00123456789abcdef0123",
            false,
            0.0,
            vec![file(0, "Sintel.2010.1080p.mkv", 1_900_000_000, true)],
        ),
    ]
}

fn torrent(
    id: usize,
    name: &str,
    info_hash: &str,
    finished: bool,
    percent: f64,
    files: Vec<FileView>,
) -> TorrentView {
    let total: u64 = files.iter().map(|f| f.length).sum();
    let primary = files
        .iter()
        .filter(|f| f.is_video)
        .max_by_key(|f| f.length)
        .map(|f| f.id);

    TorrentView {
        id,
        info_hash: info_hash.to_string(),
        name: Some(name.to_string()),
        output_folder: "/tmp/reel-demo/".to_string(),
        // A torrent that has finished still reports itself as live: it is
        // seeding rather than stopped.
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
