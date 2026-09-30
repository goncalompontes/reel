//! Human-facing formatting helpers, shared by the CLI and the desktop app.

/// Format a byte count with binary units.
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

/// Bytes per second.
pub fn human_rate(bytes_per_sec: u64) -> String {
    format!("{}/s", human_bytes(bytes_per_sec))
}

/// `1:23:45`, `12:34` or `0:07`.
pub fn human_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "--:--".to_string();
    }
    let total = seconds.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// An estimate like `2m10s`, or `-` when unknown.
pub fn human_eta(seconds: Option<u64>) -> String {
    match seconds {
        None => "-".to_string(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m{}s", s / 60, s % 60),
        Some(s) => format!("{}h{}m", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(7_570_718), "7.2 MiB");
    }

    #[test]
    fn durations() {
        assert_eq!(human_duration(7.0), "0:07");
        assert_eq!(human_duration(754.0), "12:34");
        assert_eq!(human_duration(5025.0), "1:23:45");
        assert_eq!(human_duration(f64::NAN), "--:--");
    }

    #[test]
    fn etas() {
        assert_eq!(human_eta(None), "-");
        assert_eq!(human_eta(Some(30)), "30s");
        assert_eq!(human_eta(Some(130)), "2m10s");
        assert_eq!(human_eta(Some(7300)), "2h1m");
    }
}
