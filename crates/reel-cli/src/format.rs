//! Small formatting helpers, so the CLI output stays readable.

use reel_core::TorrentView;

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn human_rate(bytes_per_sec: u64) -> String {
    format!("{}/s", human_bytes(bytes_per_sec))
}

pub fn human_eta(seconds: Option<u64>) -> String {
    match seconds {
        None => "-".to_string(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m{}s", s / 60, s % 60),
        Some(s) => format!("{}h{}m", s / 3600, (s % 3600) / 60),
    }
}

/// A one-line-per-torrent table.
pub fn torrent_table(views: &[TorrentView]) -> String {
    if views.is_empty() {
        return "no torrents".to_string();
    }

    let mut out = String::new();
    out.push_str(&format!(
        "{:>3}  {:<44}  {:<12}  {:>7}  {:>10}  {:>9}  {:>5}\n",
        "id", "title", "state", "done", "down", "eta", "peers"
    ));

    for v in views {
        let title = v
            .name
            .clone()
            .unwrap_or_else(|| v.info_hash.clone());
        let title = truncate(&title, 44);
        let playable = match v.primary_file() {
            Some(f) => format!("file {} ({})", f.id, human_bytes(f.length)),
            None => "no playable file".to_string(),
        };

        out.push_str(&format!(
            "{:>3}  {:<44}  {:<12}  {:>6.1}%  {:>10}  {:>9}  {:>5}\n",
            v.id,
            title,
            v.state,
            v.stats.percent,
            human_rate(v.stats.download_bps),
            human_eta(v.stats.eta_seconds),
            v.stats.peers.live,
        ));
        out.push_str(&format!(
            "     {:<44}  {}\n",
            human_bytes(v.stats.progress_bytes) + " / " + &human_bytes(v.stats.total_bytes),
            playable
        ));
    }

    out.trim_end().to_string()
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}
