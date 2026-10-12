//! An in-memory backend for UI tests: no engine, network or HTTP.

use super::*;
use crate::cast::{CastAction, CastStatus, DeviceInfo};

// --------------------------------------------------------- casting handle

/// Shared casting state for [`FakeBackend`], so a test can drive and observe it
/// after the backend has been boxed into the [`App`](crate::App).
#[derive(Clone)]
pub struct FakeCast {
    /// Why casting is off; `None` means available.
    available: Arc<Mutex<Option<String>>>,
    status: Arc<Mutex<Option<CastStatus>>>,
    devices: Arc<Mutex<Vec<DeviceInfo>>>,
    actions: Arc<Mutex<Vec<CastAction>>>,
}

impl Default for FakeCast {
    fn default() -> Self {
        // Off by default, so the demo and every existing test keep seeing the
        // "casting is not available" path unless they opt in.
        Self {
            available: Arc::new(Mutex::new(Some(
                "casting is not available (demo backend)".to_string(),
            ))),
            status: Arc::new(Mutex::new(None)),
            devices: Arc::new(Mutex::new(Vec::new())),
            actions: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl FakeCast {
    /// Make casting available, as the real backend does when it is configured.
    pub fn enable(&self) {
        *self.available.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Put a cast on screen, or clear it.
    pub fn set_status(&self, status: Option<CastStatus>) {
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = status;
    }

    /// Advertise renderers in the picker.
    pub fn set_devices(&self, devices: Vec<DeviceInfo>) {
        *self.devices.lock().unwrap_or_else(|e| e.into_inner()) = devices;
    }

    /// The control actions the UI has sent, in order.
    pub fn actions(&self) -> Vec<CastAction> {
        self.actions.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

// ------------------------------------------------------------ fake backend

/// An in-memory backend for tests and for `--demo` runs.
pub struct FakeBackend {
    items: Mutex<Vec<LibraryItem>>,
    /// Media key -> the files that back it, mirroring the real store.
    downloads: Mutex<HashMap<String, Vec<(usize, usize)>>>,
    /// Media key -> the copy the user chose.
    chosen: Mutex<HashMap<String, (usize, usize)>>,
    events: Mutex<Vec<BackendEvent>>,
    capabilities: BackendCapabilities,
    base_url: String,
    catalog_status: CatalogStatus,
    settings: Mutex<CatalogSettings>,
    cast: FakeCast,
}

impl FakeBackend {
    pub fn new(items: Vec<LibraryItem>) -> Self {
        Self {
            items: Mutex::new(items),
            downloads: Mutex::new(HashMap::new()),
            chosen: Mutex::new(HashMap::new()),
            events: Mutex::new(Vec::new()),
            settings: Mutex::new(CatalogSettings::default()),
            capabilities: BackendCapabilities {
                download_dir: "/tmp/reel-demo".to_string(),
                client_name: "reel-demo".to_string(),
                // Tests must never try to open a real video device.
                player: PlayerCapability::Unavailable {
                    reason: "demo backend".to_string(),
                },
                mpv_api: None,
                player_note: Some("demo backend".to_string()),
            },
            base_url: "http://127.0.0.1:0".to_string(),
            catalog_status: CatalogStatus {
                provider: "static".to_string(),
                configured: true,
                note: None,
                ..Default::default()
            },
            cast: FakeCast::default(),
        }
    }

    /// Replace the whole library, as a test would between assertions.
    pub fn set_items(&self, items: Vec<LibraryItem>) {
        *self.items.lock().unwrap_or_else(|e| e.into_inner()) = items;
    }

    /// A shared handle to the casting state, for tests that need to drive it
    /// after the backend is boxed into an [`App`](crate::App).
    pub fn cast_handle(&self) -> FakeCast {
        self.cast.clone()
    }
}

impl Backend for FakeBackend {
    fn capabilities(&self) -> &BackendCapabilities {
        &self.capabilities
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn library(&self) -> Vec<LibraryItem> {
        self.items.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn item(&self, id: usize) -> Option<LibraryItem> {
        self.library()
            .into_iter()
            .find(|item| item.torrent.id == id)
    }

    fn add(&self, source: &str, media_only: bool) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(BackendEvent::Info(format!(
                "add {source} (media_only={media_only})"
            )));
    }

    fn add_search_hit(&self, hit: reel_catalog::SearchHit, media_only: bool) {
        let key = hit.key();
        match hit.magnet.or(hit.torrent_url) {
            Some(source) => self.add(&source, media_only),
            None => {
                self.events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(BackendEvent::Error(
                        "This result has nothing to add".to_string(),
                    ))
            }
        }
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(BackendEvent::SearchActionFinished { key });
    }

    fn play_search_hit(&self, hit: reel_catalog::SearchHit, _media_only: bool) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(BackendEvent::SearchActionFinished { key: hit.key() });
    }

    fn set_paused(&self, id: usize, paused: bool) {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(item) = items.iter_mut().find(|i| i.torrent.id == id) {
            item.torrent.stats.state = if paused { "paused" } else { "live" }.to_string();
            item.torrent.state = item.torrent.stats.state.clone();
        }
    }

    fn remove(&self, id: usize, _delete_files: bool) {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        items.retain(|item| item.torrent.id != id);
    }

    fn set_only_files(&self, id: usize, files: &[usize]) {
        if files.is_empty() {
            return;
        }
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(item) = items.iter_mut().find(|i| i.torrent.id == id) {
            for file in &mut item.torrent.files {
                file.included = files.contains(&file.id);
            }
        }
    }

    fn start_files(&self, id: usize, files: &[usize]) {
        if files.is_empty() {
            return;
        }
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(item) = items.iter_mut().find(|i| i.torrent.id == id) {
            for file in &mut item.torrent.files {
                file.included = files.contains(&file.id);
            }
            // The real backend resumes the torrent; mirror that so the UI can
            // be tested against the paused-on-add behaviour.
            item.torrent.stats.state = "live".to_string();
            item.torrent.state = "live".to_string();
        }
    }

    fn download_media(&self, items: Vec<MediaDownload>) {
        let mut downloads = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
        let mut library = self.items.lock().unwrap_or_else(|e| e.into_inner());
        let mut dirty: Vec<usize> = Vec::new();
        for request in items {
            let mut files = Vec::new();
            for id in &request.files {
                files.push((request.torrent_id, *id));
            }
            if files.is_empty() {
                continue;
            }
            downloads.insert(request.key.clone(), files);
            dirty.push(request.torrent_id);
        }
        dirty.sort_unstable();
        dirty.dedup();
        for id in dirty {
            let wanted: HashSet<usize> = downloads
                .values()
                .filter(|files| files.iter().any(|(torrent, _)| *torrent == id))
                .flat_map(|files| files.iter())
                .filter(|(torrent, _)| *torrent == id)
                .map(|(_, file)| *file)
                .collect();
            if let Some(item) = library.iter_mut().find(|i| i.torrent.id == id) {
                item.kept_files = wanted.iter().copied().collect();
                item.kept_files.sort_unstable();
                item.downloading = !item.kept_files.is_empty();
                item.torrent.stats.state = "live".to_string();
                item.torrent.state = "live".to_string();
                for file in &mut item.torrent.files {
                    file.included = item.kept_files.contains(&file.id);
                }
            }
        }
    }

    fn stop_media_download(&self, items: Vec<MediaDownload>) {
        let mut downloads = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
        let mut affected: HashSet<usize> = HashSet::new();
        for item in &items {
            affected.insert(item.torrent_id);
            if let Some(files) = downloads.get_mut(&item.key) {
                files.retain(|(torrent, file)| {
                    !(*torrent == item.torrent_id && item.files.contains(file))
                });
                if files.is_empty() {
                    downloads.remove(&item.key);
                }
            }
        }
        let mut library = self.items.lock().unwrap_or_else(|e| e.into_inner());
        for id in affected {
            let wanted: HashSet<usize> = downloads
                .values()
                .flat_map(|files| files.iter())
                .filter(|(torrent, _)| *torrent == id)
                .map(|(_, file)| *file)
                .collect();
            if let Some(item) = library.iter_mut().find(|i| i.torrent.id == id) {
                item.kept_files = wanted.iter().copied().collect();
                item.kept_files.sort_unstable();
                item.downloading = !item.kept_files.is_empty();
                if !item.downloading {
                    item.torrent.stats.state = "paused".to_string();
                    item.torrent.state = "paused".to_string();
                }
                for file in &mut item.torrent.files {
                    file.included = item.kept_files.contains(&file.id);
                }
            }
        }
    }

    fn choose_source(&self, key: String, torrent_id: usize, file_id: usize) {
        self.chosen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, (torrent_id, file_id));
    }

    fn chosen_source(&self, key: &str) -> Option<(usize, usize)> {
        self.chosen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .copied()
    }

    fn stop_streaming(&self, id: usize) {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(item) = items.iter_mut().find(|i| i.torrent.id == id) {
            item.torrent.stats.state = "paused".to_string();
            item.torrent.state = "paused".to_string();
        }
    }

    fn restart_stream(&self, id: usize, files: &[usize]) {
        self.start_files(id, files);
    }

    fn is_live(&self, _id: usize) -> bool {
        // Fixtures are always present; the demo never re-adds torrents.
        true
    }

    fn note_playback(&self, _id: usize, _file_id: usize, _position: f64, _duration: Option<f64>) {
        // The demo has no engine to throttle.
    }

    fn record_watch(
        &self,
        info_hash: &str,
        file_id: usize,
        file_name: Option<String>,
        _title: Option<String>,
        position: f64,
        duration: Option<f64>,
    ) {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(item) = items.iter_mut().find(|i| i.entry.info_hash == info_hash) {
            let progress = WatchProgress {
                position,
                duration,
                updated_at: now_unix(),
                file_name,
                title: None,
            };
            item.entry.watch_by_file.insert(file_id, progress.clone());
            item.entry.watch = Some(progress);
        }
    }

    fn mark_finished(&self, info_hash: &str, file_id: usize, _title: Option<String>) {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(item) = items.iter_mut().find(|i| i.entry.info_hash == info_hash) {
            let duration = item
                .entry
                .watch_by_file
                .get(&file_id)
                .and_then(|w| w.duration)
                .or_else(|| item.entry.watch.as_ref().and_then(|w| w.duration))
                .unwrap_or(100.0);
            let progress = WatchProgress {
                position: duration,
                duration: Some(duration),
                updated_at: now_unix(),
                file_name: None,
                title: None,
            };
            item.entry.watch_by_file.insert(file_id, progress.clone());
            // The torrent-level record is what "continue watching" reads, and
            // the real backend derives it from the newest per-file entry.
            item.entry.watch = Some(progress);
        }
    }

    fn forget_watch(&self, info_hash: &str, file_id: usize) {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(item) = items.iter_mut().find(|i| i.entry.info_hash == info_hash) {
            item.entry.watch_by_file.remove(&file_id);
            item.entry.watch = None;
        }
    }

    fn search(&self, query: &reel_catalog::SearchQuery) {
        // Answer with fixtures, so the UI can be driven without a network.
        let text = query.text.to_lowercase();
        let mut results = SearchResults::default();
        for hit in crate::testing::sample_search_hits() {
            if hit.title.to_lowercase().contains(&text) || query.text.trim().is_empty() {
                results.hits.push(hit);
            }
        }
        results
            .hits
            .sort_by_key(|hit| std::cmp::Reverse(hit.sort_key()));

        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(BackendEvent::SearchResults {
                query: query.describe(),
                results,
            });
        // The demo answers in one batch, so the stream is already over.
        self.events.lock().unwrap_or_else(|e| e.into_inner()).push(
            BackendEvent::SearchStreamEnded {
                query: query.describe(),
            },
        );
    }

    fn search_sources(&self) -> Vec<SourceInfo> {
        vec![SourceInfo {
            name: "demo".to_string(),
            configured: true,
        }]
    }

    fn catalog_status(&self) -> CatalogStatus {
        let mut status = self.catalog_status.clone();
        let items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        status.enriched = items.iter().filter(|i| i.entry.metadata.is_some()).count();
        status
    }

    fn settings(&self) -> CatalogSettings {
        self.settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn save_settings(&self, mut settings: CatalogSettings) {
        settings.normalise();
        *self.settings.lock().unwrap_or_else(|e| e.into_inner()) = settings;
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(BackendEvent::Info("settings saved (demo)".to_string()));
    }

    fn set_api_key(&self, key: Option<String>) {
        let mut settings = self.settings();
        settings.tmdb_api_key = key;
        self.save_settings(settings);
    }

    fn test_api_key(&self, _key: String) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(BackendEvent::ApiKeyChecked {
                ok: true,
                message: "The key works (demo).".to_string(),
            });
    }

    fn clear_catalog_cache(&self) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(BackendEvent::Info("cache cleared (demo)".to_string()));
    }

    fn refresh_metadata(&self) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(BackendEvent::Info("refreshing metadata (demo)".to_string()));
    }

    fn take_events(&self) -> Vec<BackendEvent> {
        std::mem::take(&mut *self.events.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn cast_unavailable(&self) -> Option<String> {
        self.cast
            .available
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn cast_devices(&self) -> Vec<DeviceInfo> {
        self.cast
            .devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn cast_status(&self) -> Option<CastStatus> {
        self.cast
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn start_cast(
        &self,
        _udn: String,
        _torrent_id: usize,
        _info_hash: String,
        _file_id: usize,
        _title: String,
        _start_at: Option<Duration>,
        _subtitles: bool,
        _audio_index: Option<u32>,
    ) {
    }

    fn cast_action(&self, action: CastAction) {
        self.cast
            .actions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(action);
    }

    fn dismiss_cast(&self) {
        *self.cast.status.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn remember_cast_device(&self, name: &str, udn: &str) {
        let mut settings = self.settings.lock().unwrap_or_else(|e| e.into_inner());
        settings.cast_last_device_udn = Some(udn.to_string());
        settings.cast_last_device_name = Some(name.to_string());
    }
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

