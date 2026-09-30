//! Catalog rows: the shapes the library page is built from.
//!
//! Rows are derived from [`CatalogEntry`] values that already carry their watch
//! state, which keeps this module pure — no engine, no history file, no network.

use crate::model::CatalogEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// Partly watched; the most useful row, so it goes first.
    ContinueWatching,
    /// Everything in the library, newest first.
    RecentlyAdded,
    /// Never opened.
    Unwatched,
    /// Watched to the end.
    Finished,
}

impl RowKind {
    pub fn title(&self) -> &'static str {
        match self {
            RowKind::ContinueWatching => "Continue watching",
            RowKind::RecentlyAdded => "Recently added",
            RowKind::Unwatched => "Not started",
            RowKind::Finished => "Watched",
        }
    }

    /// Order rows are shown in.
    pub fn order(&self) -> u8 {
        match self {
            RowKind::ContinueWatching => 0,
            RowKind::RecentlyAdded => 1,
            RowKind::Unwatched => 2,
            RowKind::Finished => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub kind: RowKind,
    pub title: String,
    pub entries: Vec<CatalogEntry>,
}

impl Row {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Build the rows for a library page, each capped at `limit` entries.
///
/// Empty rows are dropped, so a fresh library shows only "Recently added"
/// rather than a wall of placeholder headings.
pub fn build_rows(entries: &[CatalogEntry], limit: usize) -> Vec<Row> {
    let mut continue_watching: Vec<CatalogEntry> = Vec::new();
    let mut finished: Vec<CatalogEntry> = Vec::new();
    let mut unwatched: Vec<CatalogEntry> = Vec::new();

    for entry in entries {
        match entry.watch.as_ref() {
            Some(progress) if progress.is_finished() => finished.push(entry.clone()),
            Some(progress) if progress.is_resumable() => continue_watching.push(entry.clone()),
            // Started but barely: treat as unwatched so it does not clutter
            // the continue-watching row.
            Some(_) => unwatched.push(entry.clone()),
            None => unwatched.push(entry.clone()),
        }
    }

    // Most recently watched first.
    continue_watching.sort_by(|a, b| {
        let at = a.watch.as_ref().map(|w| w.updated_at).unwrap_or(0);
        let bt = b.watch.as_ref().map(|w| w.updated_at).unwrap_or(0);
        bt.cmp(&at).then_with(|| a.torrent_id.cmp(&b.torrent_id))
    });
    finished.sort_by(|a, b| {
        let at = a.watch.as_ref().map(|w| w.updated_at).unwrap_or(0);
        let bt = b.watch.as_ref().map(|w| w.updated_at).unwrap_or(0);
        bt.cmp(&at).then_with(|| a.torrent_id.cmp(&b.torrent_id))
    });

    // Ids increase as torrents are added, so this is "newest first".
    let mut recent = entries.to_vec();
    recent.sort_by_key(|entry| std::cmp::Reverse(entry.torrent_id));

    unwatched.sort_by_key(|entry| std::cmp::Reverse(entry.torrent_id));

    // With nothing watched at all, "not started" would be a carbon copy of
    // "recently added", so it is left out until it says something new.
    let anything_watched = entries.iter().any(|entry| entry.watch.is_some());

    let mut rows = vec![
        Row {
            kind: RowKind::ContinueWatching,
            title: RowKind::ContinueWatching.title().to_string(),
            entries: continue_watching,
        },
        Row {
            kind: RowKind::RecentlyAdded,
            title: RowKind::RecentlyAdded.title().to_string(),
            entries: recent,
        },
        Row {
            kind: RowKind::Unwatched,
            title: RowKind::Unwatched.title().to_string(),
            entries: if anything_watched { unwatched } else { Vec::new() },
        },
        Row {
            kind: RowKind::Finished,
            title: RowKind::Finished.title().to_string(),
            entries: finished,
        },
    ];

    for row in &mut rows {
        row.entries.truncate(limit);
    }
    rows.retain(|row| !row.is_empty());
    rows.sort_by_key(|row| row.kind.order());
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Metadata, WatchProgress};

    fn entry(id: usize, hash: &str, watch: Option<(f64, f64, i64)>) -> CatalogEntry {
        CatalogEntry {
            torrent_id: id,
            info_hash: hash.to_string(),
            display_title: format!("Title {id}"),
            year: None,
            metadata: Some(Metadata {
                source: "test".into(),
                source_id: id.to_string(),
                title: format!("Title {id}"),
                ..Default::default()
            }),
            watch: watch.map(|(position, duration, updated_at)| WatchProgress {
                position,
                duration: Some(duration),
                updated_at,
                file_name: None,
                title: None,
            }),
        }
    }

    #[test]
    fn a_fresh_library_only_has_recently_added() {
        let entries = vec![entry(1, "a", None), entry(2, "b", None)];
        let rows = build_rows(&entries, 10);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, RowKind::RecentlyAdded);
        // Newest first.
        assert_eq!(rows[0].entries[0].torrent_id, 2);
    }

    #[test]
    fn continue_watching_comes_first_and_is_newest_first() {
        let entries = vec![
            entry(1, "a", Some((300.0, 1000.0, 100))),
            entry(2, "b", Some((400.0, 1000.0, 900))),
            entry(3, "c", Some((200.0, 1000.0, 500))),
        ];
        let rows = build_rows(&entries, 10);
        assert_eq!(rows[0].kind, RowKind::ContinueWatching);
        let ids: Vec<usize> = rows[0].entries.iter().map(|e| e.torrent_id).collect();
        assert_eq!(ids, [2, 3, 1]);
    }

    #[test]
    fn finished_and_unstarted_entries_are_separated() {
        let entries = vec![
            entry(1, "a", Some((950.0, 1000.0, 100))), // finished
            entry(2, "b", None),                       // never started
            entry(3, "c", Some((5.0, 1000.0, 100))),   // barely started
            entry(4, "d", Some((500.0, 1000.0, 100))), // resumable
        ];
        let rows = build_rows(&entries, 10);

        let kinds: Vec<RowKind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                RowKind::ContinueWatching,
                RowKind::RecentlyAdded,
                RowKind::Unwatched,
                RowKind::Finished
            ]
        );

        let row = |kind: RowKind| rows.iter().find(|r| r.kind == kind).unwrap();
        assert_eq!(row(RowKind::ContinueWatching).entries.len(), 1);
        assert_eq!(row(RowKind::ContinueWatching).entries[0].torrent_id, 4);
        // The barely-started one joins the never-started one.
        let unwatched: Vec<usize> = row(RowKind::Unwatched)
            .entries
            .iter()
            .map(|e| e.torrent_id)
            .collect();
        assert_eq!(unwatched, [3, 2]);
        assert_eq!(row(RowKind::Finished).entries[0].torrent_id, 1);
    }

    #[test]
    fn rows_are_capped_but_keep_the_best_entries() {
        let entries: Vec<CatalogEntry> = (1..=5)
            .map(|id| entry(id, &format!("h{id}"), Some((300.0, 1000.0, id as i64))))
            .collect();
        let rows = build_rows(&entries, 2);

        let continue_watching = rows
            .iter()
            .find(|r| r.kind == RowKind::ContinueWatching)
            .unwrap();
        assert_eq!(continue_watching.entries.len(), 2);
        // The two most recent.
        let ids: Vec<usize> = continue_watching.entries.iter().map(|e| e.torrent_id).collect();
        assert_eq!(ids, [5, 4]);
    }

    #[test]
    fn an_empty_library_produces_no_rows() {
        assert!(build_rows(&[], 10).is_empty());
    }
}
