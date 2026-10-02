//! The seam between the UI and everything below it.
//!
//! The UI only ever talks to [`Backend`], which keeps three useful properties:
//!
//! * the whole interface can be driven in tests by [`FakeBackend`], with no
//!   engine, no network and no HTTP server;
//! * the engine stays off the UI thread for anything slow, because the async
//!   methods are spawned onto a runtime and report back through events;
//! * metadata enrichment happens in the background, so a library of two hundred
//!   titles does not stall the first frame.
//!
//! The real implementation mounts the engine, the streaming HTTP server and the
//! metadata provider in one process. The player is handed URLs on `127.0.0.1`,
//! so playback uses the same range-request path the CLI and the integration
//! tests exercise.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reel_catalog::{
    ArchiveOrgBackend, CatalogCache, CatalogEntry, CatalogSettings, FileInput, LookupQuery,
    MediaRef, MediaStore, Metadata, MetadataProvider, Release, SearchAggregator, SearchResults,
    TmdbClient, WatchHistory, WatchProgress, Work, WorkMember, analyse, default_data_dir,
    episode_key_parts, provider_from_key,
};
use reel_core::model::{FileView, StatsView, StreamTarget, TorrentView};
use reel_core::title::clean_title;
use reel_core::{
    AddOptions, AddSource, Engine, EngineConfig, LibraryEntry, LibraryStore, StoredFile,
};

/// How many metadata lookups may be in flight at once. Providers rate-limit, and
/// a library can be large.
const MAX_CONCURRENT_LOOKUPS: usize = 2;

/// Minimum gap between writing watch positions to disk.
const HISTORY_WRITE_INTERVAL: Duration = Duration::from_secs(3);

/// How video will be shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerCapability {
    /// libmpv renders frames into the app window.
    Embedded,
    /// A separate player process; reel cannot draw on top of it.
    External { program: String },
    /// Neither worked.
    Unavailable { reason: String },
}

#[derive(Debug, Clone)]
pub struct BackendCapabilities {
    pub download_dir: String,
    pub client_name: String,
    pub player: PlayerCapability,
    pub mpv_api: Option<(u64, u64)>,
    /// Why embedded playback is unavailable, when it is.
    pub player_note: Option<String>,
}

/// How metadata enrichment is doing, for the settings page.
#[derive(Debug, Clone, Default)]
pub struct CatalogStatus {
    pub provider: String,
    pub configured: bool,
    /// Why there is no provider, when there is not.
    pub note: Option<String>,
    pub cached_metadata: usize,
    pub cache_bytes: u64,
    pub cache_dir: String,
    /// How many library entries have metadata.
    pub enriched: usize,
    /// How many library entries are still being looked up.
    pub pending: usize,
    /// Where the key in use came from, in words.
    pub key_source: String,
    /// False when the key comes from the environment and cannot be changed here.
    pub can_set_key: bool,
}

/// One configured place to look for torrents.
#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub name: String,
    pub configured: bool,
}

/// A torrent together with what the catalogue knows about it.
#[derive(Debug, Clone)]
pub struct LibraryItem {
    /// Engine state: files, progress, peers.
    pub torrent: TorrentView,
    /// Catalogue state: metadata, artwork, watch position.
    pub entry: CatalogEntry,
    /// The user asked to keep this title on disk. `false` means it is only ever
    /// streamed.
    pub downloading: bool,
    /// Which files the user asked to keep, so a per-episode/season/film row can
    /// show its own Download/Stop toggle.
    pub kept_files: Vec<usize>,
}

impl LibraryItem {
    /// Heading for the UI, preferring real metadata.
    pub fn heading(&self) -> String {
        self.entry.heading()
    }

    pub fn info_hash(&self) -> &str {
        &self.entry.info_hash
    }

    /// Position to resume from, if any.
    pub fn resume_position(&self) -> Option<f64> {
        self.entry.resume_position()
    }
}

/// Something the backend wants the UI to know about.
#[derive(Debug, Clone)]
pub enum BackendEvent {
    Info(String),
    Error(String),
    Added { id: usize, title: String },
    /// Metadata arrived for a torrent, so the UI should refresh.
    Metadata { info_hash: String },
    /// A title that had to be brought back into the session is now live and can
    /// be streamed. Payback may have been requested before that.
    Ready { info_hash: String },
    /// Watch positions changed.
    WatchUpdated,
    /// The result of checking an API key.
    ApiKeyChecked { ok: bool, message: String },
    /// Results for a search. Carries the query so a stale answer for an older
    /// query can be recognised and dropped.
    SearchResults { query: String, results: SearchResults },
    /// A search that could not run at all.
    SearchFailed { query: String, message: String },
}

/// One media's download request: the logical media key plus the file(s) that
/// currently back it. The backend resolves this to a stable
/// [`reel_catalog::MediaRef`] and persists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDownload {
    /// e.g. `meta:tmdb:603#movie` or `meta:tmdb:1396#s01e02`.
    pub key: String,
    /// The live torrent holding the file right now.
    pub torrent_id: usize,
    /// File ids within that torrent.
    pub files: Vec<usize>,
}

pub trait Backend {
    fn capabilities(&self) -> &BackendCapabilities;
    fn base_url(&self) -> &str;

    /// The whole library, torrents joined to catalogue metadata.
    fn library(&self) -> Vec<LibraryItem>;
    fn item(&self, id: usize) -> Option<LibraryItem>;

    fn add(&self, source: &str, media_only: bool);
    fn set_paused(&self, id: usize, paused: bool);
    fn remove(&self, id: usize, delete_files: bool);
    /// Choose exactly which files of a torrent are fetched.
    ///
    /// This is what makes "watch episode 3 of a season pack" fetch only episode
    /// 3 rather than the whole season. Files left out keep whatever they had and
    /// stop being requested.
    fn set_only_files(&self, id: usize, files: &[usize]);

    /// Fetch exactly these files *and* make sure the torrent is running.
    ///
    /// This is what pressing Play uses: the copy is temporary, so it is fetched
    /// with the streaming storage and thrown away when playback stops. A
    /// multi-file torrent is added paused, so narrowing the selection alone
    /// would leave the stream request waiting forever; the torrent is unpaused
    /// too.
    fn start_files(&self, id: usize, files: &[usize]);

    /// Keep this media, backed by these files. The record is keyed by the
    /// media (`work.key#s01e02`, `work.key#movie`), not the torrent, and
    /// replaces any previous copy — so switching the chosen release moves the
    /// download rather than duplicating it.
    fn download_media(&self, items: Vec<MediaDownload>);

    /// Stop keeping these files. When a media's last file goes, the media is no
    /// longer a download; its source deletes the files and goes back to
    /// streaming.
    fn stop_media_download(&self, items: Vec<MediaDownload>);

    /// Remember which source the user chose for a media, so the copy chooser
    /// survives a restart.
    fn choose_source(&self, key: String, torrent_id: usize, file_id: usize);

    /// The user's chosen source for a media, as `(torrent id, file id)`.
    fn chosen_source(&self, key: &str) -> Option<(usize, usize)>;

    /// Playback has stopped. A title that is not being downloaded is released
    /// so its temporary storage goes away; a download is left alone.
    fn stop_streaming(&self, id: usize);

    /// Re-add a stream from scratch and start these files. Used when a cached
    /// resume stalls: a fresh add is the path that is known to play.
    fn restart_stream(&self, id: usize, files: &[usize]);

    /// Whether the title is currently in the session, so it can stream now.
    /// A title that is not live has to be brought back first, which reports
    /// [`BackendEvent::Ready`] when it is done.
    fn is_live(&self, id: usize) -> bool;

    /// Report the player's position so the backend can keep streaming from
    /// running away: once too much is buffered ahead of playback it pauses the
    /// fetch, and resumes when playback catches up.
    fn note_playback(&self, id: usize, file_id: usize, position: f64, duration: Option<f64>);

    /// Remember how far through one file of a torrent playback got.
    ///
    /// Per file, not per torrent: a series is one torrent with many episodes,
    /// and one position for all of them would resume episode one at episode
    /// two's timestamp.
    fn record_watch(
        &self,
        info_hash: &str,
        file_id: usize,
        file_name: Option<String>,
        title: Option<String>,
        position: f64,
        duration: Option<f64>,
    );
    fn mark_finished(&self, info_hash: &str, file_id: usize, title: Option<String>);
    fn forget_watch(&self, info_hash: &str, file_id: usize);

    /// Start a search. Results arrive as [`BackendEvent::SearchResults`].
    fn search(&self, query: &str);
    /// The configured search sources, for the settings page.
    fn search_sources(&self) -> Vec<SourceInfo>;

    fn catalog_status(&self) -> CatalogStatus;
    /// The persisted settings, the canonical configuration.
    fn settings(&self) -> CatalogSettings;
    /// Persist settings and apply what can be applied immediately. A changed
    /// download directory takes effect on the next start.
    fn save_settings(&self, settings: CatalogSettings);
    /// Store a TMDB key and use it immediately. `None` clears it.
    fn set_api_key(&self, key: Option<String>);
    /// Check a key against the provider and report back as an event.
    fn test_api_key(&self, key: String);
    /// Clear cached metadata and artwork, then enrich again.
    fn clear_catalog_cache(&self);
    /// Forget what we know and look everything up again.
    fn refresh_metadata(&self);

    fn take_events(&self) -> Vec<BackendEvent>;
}

// ------------------------------------------------------------ real backend

struct CatalogState {
    /// Swappable: setting a key in Settings has to take effect without a restart.
    provider: Mutex<Arc<dyn MetadataProvider>>,
    settings: Mutex<CatalogSettings>,
    data_dir: std::path::PathBuf,
    cache: Arc<CatalogCache>,
    metadata: Mutex<HashMap<String, Metadata>>,
    in_flight: Mutex<HashSet<String>>,
    attempted: Mutex<HashSet<String>>,
    history: Mutex<WatchHistory>,
    last_history_write: Mutex<Instant>,
    search: SearchAggregator,
}

impl CatalogState {
    fn provider(&self) -> Arc<dyn MetadataProvider> {
        self.provider
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn note(&self) -> Option<String> {
        if self.provider().is_configured() {
            None
        } else {
            Some(
                "No metadata provider is configured. Add a TMDB API key in Settings to get \
                 posters and synopses; until then artwork is generated from each title."
                    .to_string(),
            )
        }
    }
}

pub struct EngineBackend {
    runtime: tokio::runtime::Runtime,
    engine: Arc<Engine>,
    base_url: String,
    events: Arc<Mutex<Vec<BackendEvent>>>,
    capabilities: BackendCapabilities,
    shutdown: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    catalog: Arc<CatalogState>,
    /// The durable library, independent of which torrents are in the session.
    library: Arc<Mutex<LibraryStore>>,
    /// Media-keyed download state. This is the source of truth for what is
    /// being kept; `LibraryEntry::selected_files` is derived from it.
    media: Arc<Mutex<MediaStore>>,
    /// Stream-only titles whose buffer is being kept for a fast replay, with
    /// the last time each was watched (oldest is evicted first).
    stream_cache: Mutex<HashMap<usize, Instant>>,
    /// The title currently playing, which is never evicted.
    playing: Mutex<Option<usize>>,
}

impl EngineBackend {
    /// Start the engine, serve the streaming API on an ephemeral localhost port,
    /// and build the metadata provider.
    pub fn start(config: EngineConfig) -> anyhow::Result<Self> {
        Self::start_with_options(config, CatalogOptions::from_settings())
    }

    pub fn start_with_options(
        mut config: EngineConfig,
        catalog_options: CatalogOptions,
    ) -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("reel-backend")
            .build()?;

        // The desktop keeps its own durable library, so a streamed torrent can
        // be removed without the title disappearing. librqbit's session
        // persistence would re-add every torrent with filesystem storage, which
        // is exactly what streaming must not do.
        config.persist_session = false;

        let download_dir = config.download_dir.display().to_string();
        let client_name = config.client_name.clone();
        let data_dir = catalog_options.data_dir.clone();

        // The user's "Stream buffer (MB)" is the look-ahead the vendored piece
        // picker is allowed; `0` means "no cap", so a stream may fetch the rest
        // of the file.
        let buffer_mb = catalog_options.settings.stream_buffer_mb as usize;
        config.stream_lookahead_bytes = if buffer_mb == 0 {
            usize::MAX / 2
        } else {
            buffer_mb.saturating_mul(1024 * 1024)
        };

        let engine = runtime.block_on(Engine::new(config))?;

        let listener = runtime.block_on(reel_http::bind(([127, 0, 0, 1], 0).into()))?;
        let port = listener.local_addr()?.port();
        let base_url = format!("http://127.0.0.1:{port}");

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let router = reel_http::build_router(engine.clone(), base_url.clone());
        runtime.spawn(async move {
            let _ = reel_http::serve_router_with_shutdown(listener, router, async move {
                let _ = shutdown_rx.await;
            })
            .await;
        });

        tracing::info!(%base_url, "in-process streaming server ready");

        let (player, player_note) = match reel_player::embedded_available() {
            Ok(()) => (PlayerCapability::Embedded, None),
            Err(e) => {
                let note = e.to_string();
                match reel_player::Player::new(reel_player::PlayerConfig {
                    prefer_embedded: false,
                    ..Default::default()
                }) {
                    Ok(_) => (
                        PlayerCapability::External {
                            program: "mpv".to_string(),
                        },
                        Some(note),
                    ),
                    Err(_) => (
                        PlayerCapability::Unavailable { reason: note.clone() },
                        Some(note),
                    ),
                }
            }
        };

        let cache = Arc::new(CatalogCache::new(catalog_options.data_dir.clone()));
        let provider = provider_from_key(catalog_options.api_key.as_deref(), cache.clone());

        let history = WatchHistory::load(catalog_options.data_dir.join("history.json"));

        let capabilities = BackendCapabilities {
            download_dir,
            client_name,
            player,
            mpv_api: reel_player::mpv_api_version(),
            player_note,
        };

        // The one backend we ship: the Internet Archive, which serves
        // public-domain and Creative Commons film. Anything else is the
        // operator's to add — see docs/ADDING_A_SOURCE.md.
        let mut backends: Vec<Box<dyn reel_catalog::search::SearchBackend>> = Vec::new();
        if !catalog_options.disable_bundled_sources {
            backends.push(Box::new(ArchiveOrgBackend::new()));
        }
        let search = SearchAggregator::new(backends);

        let catalog = Arc::new(CatalogState {
            provider: Mutex::new(provider),
            settings: Mutex::new(catalog_options.settings.clone()),
            data_dir: catalog_options.data_dir.clone(),
            cache,
            metadata: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
            attempted: Mutex::new(HashSet::new()),
            history: Mutex::new(history),
            last_history_write: Mutex::new(Instant::now() - HISTORY_WRITE_INTERVAL),
            search,
        });

        let library = Arc::new(Mutex::new(LibraryStore::load(&data_dir.join("library.json"))));
        let media = Arc::new(Mutex::new(MediaStore::load(&MediaStore::path(&data_dir))));

        let backend = Self {
            runtime,
            engine,
            base_url,
            events: Arc::new(Mutex::new(Vec::new())),
            capabilities,
            shutdown: Mutex::new(Some(shutdown_tx)),
            catalog,
            library,
            media,
            stream_cache: Mutex::new(HashMap::new()),
            playing: Mutex::new(None),
        };

        // The media store owns what is being kept. Adopt any old per-torrent
        // state once, then derive `selected_files`/`downloading` from it before
        // re-adding the downloads.
        backend.adopt_legacy_downloads();
        backend.rehydrate_downloads();
        // Bring back the titles the user asked to keep, then enrich.
        backend.restore_downloads();
        backend.enrich_library();
        Ok(backend)
    }

    fn push(&self, event: BackendEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
    }

    /// Spawn a fallible engine operation and report the outcome as an event.
    fn spawn_result<F, E>(&self, what: &'static str, future: F)
    where
        F: std::future::Future<Output = Result<(), E>> + Send + 'static,
        E: std::fmt::Display,
    {
        let events = self.events.clone();
        self.runtime.spawn(async move {
            if let Err(e) = future.await {
                let message = format!("{what}: {e}");
                tracing::warn!("{message}");
                events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(BackendEvent::Error(message));
            }
        });
    }

    // ------------------------------------------------------------ library

    /// The live engine id for each torrent in the session, by info hash.
    fn live_map(&self) -> HashMap<String, usize> {
        self.engine
            .list()
            .into_iter()
            .map(|view| (view.info_hash.to_ascii_lowercase(), view.id))
            .collect()
    }

    fn library_path(&self) -> std::path::PathBuf {
        self.catalog.data_dir.join("library.json")
    }

    fn save_library(&self) {
        let library = self.library.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = library.save(&self.library_path()) {
            tracing::warn!(error = %e, "could not save the library");
        }
    }

    fn snapshot(&self) -> LibraryStore {
        self.library
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Persist the `.torrent` bytes so the title can be re-added offline.
    fn save_torrent_bytes(&self, info_hash: &str, bytes: &[u8]) {
        let dir = self.catalog.data_dir.join("torrents");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(error = %e, "could not create the torrent cache");
            return;
        }
        let path = dir.join(format!("{}.torrent", info_hash.to_ascii_lowercase()));
        if let Err(e) = std::fs::write(&path, bytes) {
            tracing::warn!(error = %e, path = %path.display(), "could not save torrent bytes");
        }
    }

    /// Remove a live torrent and bring it back, optionally with different
    /// storage and a file selection.
    ///
    /// This is how a title switches between streaming (temporary storage,
    /// paused when not playing) and downloading (filesystem storage, running),
    /// and how a stopped stream frees its temporary data. It runs as one task
    /// so the remove always completes before the re-add, and the library's
    /// stable id is untouched while the engine id changes.
    ///
    /// The engine id is preserved across the swap (`preferred_id`), so a stream
    /// URL a player already holds keeps resolving.
    fn recycle(
        &self,
        entry: LibraryEntry,
        downloading: bool,
        start: Option<Vec<usize>>,
        delete_files: bool,
    ) {
        let engine = self.engine.clone();
        let events = self.events.clone();
        let data_dir = self.catalog.data_dir.clone();

        self.runtime.spawn(async move {
            // Re-add under the same engine id so any stream URL a player
            // already holds stays valid across the swap.
            let mut preferred_id = None;
            if let Some(engine_id) = engine.id_for_hash(&entry.info_hash) {
                preferred_id = Some(engine_id);
                if let Err(e) = engine.remove(engine_id, delete_files).await {
                    tracing::warn!(error = %e, "removing before re-add failed");
                }
            }

            let source = match std::fs::read(LibraryStore::torrent_path(&data_dir, &entry)) {
                Ok(bytes) => AddSource::File(bytes),
                Err(_) if !entry.source.is_empty() => match AddSource::detect(&entry.source) {
                    Ok(source) => source,
                    Err(e) => {
                        events
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(BackendEvent::Error(format!("{e}")));
                        return;
                    }
                },
                Err(_) => return,
            };

            // The selection is applied as the torrent is added, so the whole
            // pack is never briefly selected. It starts paused because nothing
            // should download before that; only a download (or a play) resumes.
            // Add it already in the state it should end in. Trying to resume a
            // just-added torrent does not work (it is still initialising), and
            // with the selection applied at add there is no burst to guard
            // against anyway.
            let running = start.is_some() || downloading;
            let selection = start
                .clone()
                .filter(|files| !files.is_empty())
                .or_else(|| {
                    downloading
                        .then(|| entry.selected_files.clone())
                        .filter(|files| !files.is_empty())
                });
            let options = AddOptions {
                media_only: selection.is_none(),
                only_files: selection,
                paused: !running,
                allow_overwrite: downloading,
                ephemeral: !downloading,
                pause_multi_file: false,
                preferred_id,
                ..Default::default()
            };

            let engine_id = match engine.add(source, options).await {
                Ok(outcome) => outcome.torrent.id,
                Err(e) => {
                    tracing::warn!(info_hash = %entry.info_hash, error = %e, "could not re-add");
                    return;
                }
            };

            if let Some(bytes) = engine.torrent_bytes(engine_id) {
                let dir = data_dir.join("torrents");
                let _ = std::fs::create_dir_all(&dir);
                let path = dir.join(format!("{}.torrent", entry.info_hash.to_ascii_lowercase()));
                let _ = std::fs::write(path, bytes);
            }

            let mut queue = events.lock().unwrap_or_else(|e| e.into_inner());
            queue.push(BackendEvent::Metadata {
                info_hash: entry.info_hash.clone(),
            });
            queue.push(BackendEvent::Ready {
                info_hash: entry.info_hash.clone(),
            });
        });
    }

    /// Re-add every title the user asked to keep.
    fn restore_downloads(&self) {
        let entries: Vec<LibraryEntry> = self
            .library
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|entry| entry.downloading)
            .cloned()
            .collect();
        for entry in entries {
            self.recycle(entry, true, None, false);
        }
    }

    /// Persist the media-keyed download store.
    fn save_media(&self) {
        let media = self.media.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = media.save(&MediaStore::path(&self.catalog.data_dir)) {
            tracing::warn!(error = %e, "could not save the media store");
        }
    }

    /// The media key a file of a torrent belongs to, or `None` if the torrent
    /// is not in the library.
    fn media_watch_key(&self, info_hash: &str, file_id: usize) -> Option<String> {
        let entry = self.snapshot().by_hash(info_hash)?.clone();
        let release = analyse_entry(&entry);
        let work_key = self.work_key_for(&entry, &release);
        Some(Self::media_key_for_release(&work_key, &release, file_id))
    }

    /// Adopt state written before downloads were media-keyed. Runs once, when
    /// the media store has no downloads yet.
    fn adopt_legacy_downloads(&self) {
        let entries: Vec<LibraryEntry> = self.snapshot().iter().cloned().collect();
        let mut media = self.media.lock().unwrap_or_else(|e| e.into_inner());
        if media.has_downloads() {
            return;
        }
        let mut adopted = false;
        for entry in &entries {
            if !entry.downloading || entry.selected_files.is_empty() {
                continue;
            }
            let files: Vec<MediaRef> = entry
                .files
                .iter()
                .filter(|file| entry.selected_files.contains(&file.id))
                .map(|file| MediaRef::new(entry.info_hash.clone(), file.path.clone()))
                .collect();
            if files.is_empty() {
                continue;
            }
            media.adopt_legacy(&entry.info_hash, files, now_unix());
            adopted = true;
        }
        drop(media);
        if adopted {
            self.save_media();
        }
    }

    /// Recompute each library entry's file selection from the media store.
    ///
    /// `selected_files` and `downloading` are a cache for the UI and the
    /// engine; the media store is the source of truth. A reference resolves by
    /// path, so a torrent that was re-added or reordered still finds its files.
    fn rehydrate_downloads(&self) {
        let media = self
            .media
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut library = self.library.lock().unwrap_or_else(|e| e.into_inner());
        let mut changed = false;
        for entry in library.entries.iter_mut() {
            let selected = Self::selected_from_store(entry, &media);
            if entry.selected_files != selected {
                entry.selected_files = selected;
                changed = true;
            }
            let downloading = !entry.selected_files.is_empty();
            if entry.downloading != downloading {
                entry.downloading = downloading;
                changed = true;
            }
        }
        if changed {
            if let Err(e) = library.save(&self.library_path()) {
                tracing::warn!(error = %e, "could not save the library");
            }
        }
    }

    /// The file ids of `entry` wanted by the media store.
    fn selected_from_store(entry: &LibraryEntry, media: &MediaStore) -> Vec<usize> {
        let wanted: HashSet<&str> = media
            .refs_for_info_hash(&entry.info_hash)
            .map(|file| file.path.as_str())
            .collect();
        if wanted.is_empty() {
            return Vec::new();
        }
        let mut ids: Vec<usize> = entry
            .files
            .iter()
            .filter(|file| wanted.contains(file.path.as_str()))
            .map(|file| file.id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Apply the derived download state to the engine for the affected sources.
    ///
    /// A source that changes between stream and download has to be re-added
    /// (the storage type is fixed at add time). A source that is already a
    /// download and just gained or lost a file only needs its selection
    /// narrowed — recycling it would drop the peer connections and the buffer
    /// for no reason.
    fn apply_download_state(&self, modes: &[(String, bool)]) {
        for (info_hash, was_downloading) in modes {
            let Some(entry) = self.snapshot().by_hash(info_hash).cloned() else {
                continue;
            };
            let keep = entry.selected_files.clone();
            let now_downloading = !keep.is_empty();
            if *was_downloading == now_downloading {
                if now_downloading {
                    // Already filesystem storage: narrow in place if live.
                    let live = self
                        .live_map()
                        .get(&entry.info_hash.to_ascii_lowercase())
                        .copied();
                    match live {
                        Some(engine_id) => {
                            let engine = self.engine.clone();
                            self.spawn_result("select download files", async move {
                                engine.set_only_files(engine_id, &keep).await
                            });
                        }
                        None => self.recycle(entry, true, Some(keep), false),
                    }
                }
                continue;
            }
            if now_downloading {
                self.recycle(entry, true, Some(keep), false);
            } else {
                self.recycle(entry, false, None, true);
            }
        }
    }

    /// Remember that a stream was watched, for cache eviction order.
    fn touch_cache(&self, id: usize) {
        self.stream_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Instant::now());
    }

    /// Release the least recently watched stream buffers once the session cache
    /// is over its limit. Streams with downloads attached are left alone; the
    /// title currently playing is never evicted.
    fn enforce_stream_cache(&self, keep: Option<usize>) {
        let cap_mb = self.settings().stream_cache_mb;
        let cap = cap_mb as u64 * 1024 * 1024;
        let store = self.snapshot();
        let playing = *self.playing.lock().unwrap_or_else(|e| e.into_inner());

        let mut used = 0u64;
        let mut candidates: Vec<(Instant, u64, usize, String)> = Vec::new();
        let oldest = Instant::now() - Duration::from_secs(86_400);
        for view in self.engine.list() {
            let Some(entry) = store.by_hash(&view.info_hash) else {
                continue;
            };
            // A download runs to completion; it is not cache-managed.
            if !entry.selected_files.is_empty() {
                continue;
            }
            let bytes = view.stats.progress_bytes;
            used += bytes;
            // Never evict what is playing, nor what was just watched: a large
            // episode may exceed the cap on its own, and evicting the title you
            // are about to resume is exactly the wrong choice. Older streams go
            // first.
            if Some(entry.id) != playing && Some(entry.id) != keep {
                let last = self
                    .stream_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&entry.id)
                    .copied()
                    .unwrap_or(oldest);
                candidates.push((last, bytes, entry.id, entry.info_hash.clone()));
            }
        }

        if cap_mb != 0 && used <= cap {
            return;
        }
        candidates.sort_by_key(|(last, ..)| *last);
        for (_, bytes, store_id, hash) in candidates {
            if cap_mb != 0 && used <= cap {
                break;
            }
            let engine = self.engine.clone();
            self.runtime.spawn(async move {
                if let Some(engine_id) = engine.id_for_hash(&hash) {
                    let _ = engine.remove(engine_id, false).await;
                }
            });
            self.stream_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&store_id);
            used = used.saturating_sub(bytes);
        }
    }

    /// Write back the file list the engine knows, once a magnet has resolved.
    fn sync_entry(&self, id: usize, view: &TorrentView) {
        let mut library = self.library.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = library.entry_mut(id) else {
            return;
        };
        entry.name = view.name.clone().or_else(|| entry.name.clone());
        entry.primary_file_id = view.primary_file_id.or(entry.primary_file_id);
        if !view.files.is_empty() {
            entry.files = view.files.iter().map(StoredFile::from_view).collect();
        }
    }

    /// Look up metadata for every stored title we have not tried yet.
    fn enrich_library(&self) {
        if !self.catalog.provider().is_configured() {
            return;
        }

        let entries = self.snapshot();
        let mut to_start = Vec::new();

        for entry in entries.iter() {
            if entry.name.as_deref().is_none_or(str::is_empty) && entry.files.is_empty() {
                continue;
            }
            let key = entry.info_hash.clone();
            if self
                .catalog
                .metadata
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&key)
            {
                continue;
            }
            {
                let in_flight = self.catalog.in_flight.lock().unwrap_or_else(|e| e.into_inner());
                if in_flight.contains(&key) || in_flight.len() >= MAX_CONCURRENT_LOOKUPS {
                    continue;
                }
            }
            {
                let mut attempted = self.catalog.attempted.lock().unwrap_or_else(|e| e.into_inner());
                if !attempted.insert(key.clone()) {
                    continue;
                }
            }
            self.catalog
                .in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone());
            to_start.push((key, analyse_entry(&entry)));
        }

        for (info_hash, release) in to_start {
            self.spawn_lookup(info_hash, release);
        }
    }

    fn spawn_lookup(&self, info_hash: String, release: Release) {
        let provider = self.catalog.provider();
        let state = self.catalog.clone();
        let events = self.events.clone();

        self.runtime.spawn(async move {
            let query = if release.title.is_empty() {
                LookupQuery::from_release_name(&info_hash)
            } else {
                LookupQuery::from_release(&release)
            };

            let result = provider.lookup(query).await;

            state
                .in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&info_hash);

            match result {
                Ok(Some(metadata)) => {
                    tracing::info!(%info_hash, title = %metadata.title, "matched metadata");
                    state
                        .metadata
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(info_hash.clone(), metadata);
                    events
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(BackendEvent::Metadata { info_hash });
                }
                Ok(None) => tracing::debug!(%info_hash, "no metadata match"),
                Err(e) => tracing::debug!(%info_hash, error = %e, "metadata lookup failed"),
            }
        });
    }

    /// A view for a title that is not currently in the session, built from what
    /// the store remembers so the library can still draw it.
    fn synth_view(&self, entry: &LibraryEntry) -> TorrentView {
        let files: Vec<FileView> = entry
            .files
            .iter()
            .map(|file| FileView {
                id: file.id,
                path: file.path.clone(),
                name: file.name.clone(),
                length: file.length,
                progress_bytes: 0,
                included: entry.selected_files.contains(&file.id),
                is_video: file.is_video,
                is_audio: file.is_audio,
                is_subtitle: file.is_subtitle,
                stream: StreamTarget {
                    torrent_id: entry.id,
                    file_id: file.id,
                    name: file.name.clone(),
                    mime: reel_core::mime_for_name(&file.name).to_string(),
                    length: file.length,
                    path: stream_path(entry.id, file.id, &file.name),
                    url: None,
                },
            })
            .collect();

        TorrentView {
            id: entry.id,
            info_hash: entry.info_hash.clone(),
            name: entry.name.clone(),
            output_folder: String::new(),
            state: "paused".to_string(),
            finished: false,
            primary_file_id: entry.primary_file_id,
            files,
            stats: StatsView {
                state: "paused".to_string(),
                ..Default::default()
            },
        }
    }

    fn entry_for(&self, entry: &LibraryEntry, live: Option<&TorrentView>) -> LibraryItem {
        let release = match live {
            Some(view) => analyse_torrent(view),
            None => analyse_entry(entry),
        };
        let (display_title, year) = if release.title.is_empty() {
            display_title_for_entry(entry)
        } else {
            (release.title.clone(), release.year)
        };

        let metadata = self
            .catalog
            .metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&entry.info_hash)
            .cloned();

        let history = self.catalog.history.lock().unwrap_or_else(|e| e.into_inner());
        let mut watch_by_file = history.per_file(&entry.info_hash);
        let watch = history.get(&entry.info_hash).cloned();
        drop(history);

        // A media-keyed position wins over the per-file one: it follows the
        // episode to whichever release now holds it.
        let work_key = self.work_key_for(entry, &release);
        {
            let media = self.media.lock().unwrap_or_else(|e| e.into_inner());
            for file in &entry.files {
                let key = Self::media_key_for_release(&work_key, &release, file.id);
                if let Some(progress) = media.watch(&key) {
                    watch_by_file.insert(file.id, progress.clone());
                }
            }
        }

        let mut torrent = match live {
            Some(view) => {
                let mut view = view.clone();
                view.id = entry.id;
                view
            }
            None => self.synth_view(entry),
        };
        torrent.with_base_url(&self.base_url);

        LibraryItem {
            torrent,
            downloading: entry.downloading,
            kept_files: entry.selected_files.clone(),
            entry: CatalogEntry {
                torrent_id: entry.id,
                info_hash: entry.info_hash.clone(),
                display_title,
                year,
                metadata,
                watch,
                release,
                watch_by_file,
            },
        }
    }

    /// The work key the UI groups this entry under.
    ///
    /// `Work::single` applies the same identity a merged group does, so the
    /// media key computed here matches the one the UI computes from its `Work`.
    fn work_key_for(&self, entry: &LibraryEntry, release: &Release) -> String {
        let (display_title, year) = if release.title.is_empty() {
            display_title_for_entry(entry)
        } else {
            (release.title.clone(), release.year)
        };
        let metadata = self
            .catalog
            .metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&entry.info_hash)
            .cloned();
        let catalog_entry = CatalogEntry {
            torrent_id: entry.id,
            info_hash: entry.info_hash.clone(),
            display_title,
            year,
            metadata,
            watch: None,
            release: release.clone(),
            watch_by_file: Default::default(),
        };
        Work::single(WorkMember::new(catalog_entry, entry.primary_file_id)).key
    }

    /// The media key one file belongs to.
    fn media_key_for_release(work_key: &str, release: &Release, file_id: usize) -> String {
        match release
            .episodes
            .iter()
            .find(|episode| episode.file_id == file_id)
        {
            Some(episode) => episode_key_parts(
                work_key,
                episode.season,
                episode.episode,
                episode.air_date.as_deref(),
            ),
            None => format!("{work_key}#movie"),
        }
    }
}

/// Parse a torrent's file names into a film-or-series judgement.
///
/// Only video files are considered: a subtitle or an `.nfo` should not be able
/// to change what the torrent is.
pub(crate) fn analyse_torrent(torrent: &TorrentView) -> Release {
    let files: Vec<FileInput> = torrent
        .files
        .iter()
        .map(|file| FileInput::new(file.id, file.path.clone(), file.length))
        .collect();
    analyse(&files)
}

/// The same judgement, from the library's stored file list, so metadata can be
/// looked up for a title that is not currently in the session.
pub(crate) fn analyse_entry(entry: &LibraryEntry) -> Release {
    let files: Vec<FileInput> = entry
        .files
        .iter()
        .map(|file| FileInput::new(file.id, file.path.clone(), file.length))
        .collect();
    analyse(&files)
}

/// What to call a stored title in the UI.
pub(crate) fn display_title_for_entry(entry: &LibraryEntry) -> (String, Option<u16>) {
    match entry.name.as_deref().filter(|name| !name.trim().is_empty()) {
        Some(name) => {
            let clean = clean_title(name);
            (clean.title, clean.year)
        }
        None => ("Resolving magnet\u{2026}".to_string(), None),
    }
}

/// The stream URL path for one file. It is always three segments — torrent id,
/// file id, name — because a two-segment path makes the router parse the file
/// name as a file id and answer `400`.
pub(crate) fn stream_path(torrent_id: usize, file_id: usize, name: &str) -> String {
    format!(
        "/stream/{torrent_id}/{file_id}/{}",
        reel_core::model::percent_encode_path_segment(name)
    )
}

/// Where the metadata key and cache live. Read once at startup.
#[derive(Debug, Clone)]
pub struct CatalogOptions {
    /// The key to use, already resolved from settings (the environment is only
    /// a first-run fallback).
    pub api_key: Option<String>,
    /// The stored settings, so the app can show and change them.
    pub settings: CatalogSettings,
    pub data_dir: std::path::PathBuf,
    /// Leave the bundled search source out entirely.
    pub disable_bundled_sources: bool,
}

impl CatalogOptions {
    /// Load settings from the default data directory.
    pub fn from_settings() -> Self {
        Self::load(default_data_dir())
    }

    /// Read the settings file. This is the canonical configuration: there is no
    /// environment override here any more, because a launcher does not read
    /// shell rc files and a saved setting must always take effect.
    pub fn load(data_dir: std::path::PathBuf) -> Self {
        let settings = CatalogSettings::load(&data_dir);
        Self {
            api_key: settings.api_key(),
            disable_bundled_sources: !settings.enable_bundled_sources,
            settings,
            data_dir,
        }
    }
}

impl Backend for EngineBackend {
    fn capabilities(&self) -> &BackendCapabilities {
        &self.capabilities
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn library(&self) -> Vec<LibraryItem> {
        // Kick off lookups for anything new each time the UI asks, which is what
        // makes metadata appear shortly after a torrent is added.
        self.enrich_library();

        let views = self.engine.list();
        let mut live: HashMap<String, &TorrentView> = HashMap::new();
        for view in &views {
            let key = view.info_hash.to_ascii_lowercase();
            live.insert(key.clone(), view);
            // A magnet's files only exist once it has resolved; write them (and
            // the .torrent) back into the store the first time we see them.
            if self
                .library
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .by_hash(&view.info_hash)
                .is_some_and(|entry| entry.files.is_empty() && !view.files.is_empty())
            {
                if let Some(entry_id) = self
                    .library
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .by_hash(&view.info_hash)
                    .map(|entry| entry.id)
                {
                    self.sync_entry(entry_id, view);
                    self.save_library();
                }
                if let Some(bytes) = self.engine.torrent_bytes(view.id) {
                    self.save_torrent_bytes(&view.info_hash, &bytes);
                }
            }
        }

        let entries = self.snapshot();
        entries
            .iter()
            .map(|entry| {
                let view = live.get(&entry.info_hash.to_ascii_lowercase()).copied();
                self.entry_for(entry, view)
            })
            .collect()
    }

    fn item(&self, id: usize) -> Option<LibraryItem> {
        let entry = self.snapshot().entry(id).cloned()?;
        let view = self
            .engine
            .list()
            .into_iter()
            .find(|view| view.info_hash.eq_ignore_ascii_case(&entry.info_hash));
        Some(self.entry_for(&entry, view.as_ref()))
    }

    fn add(&self, source: &str, media_only: bool) {
        let engine = self.engine.clone();
        let events = self.events.clone();
        let library = self.library.clone();
        let data_dir = self.catalog.data_dir.clone();
        let source = source.to_string();
        // Streaming is the primary way to use the app: a new title is temporary
        // unless the user has turned that off, in which case it is a download.
        let downloading = !self.settings().stream_only;
        let re_add_source = source.clone();

        self.runtime.spawn(async move {
            let options = AddOptions {
                media_only,
                pause_multi_file: !downloading,
                ephemeral: !downloading,
                ..Default::default()
            };
            let parsed = match AddSource::detect(&source) {
                Ok(parsed) => parsed,
                Err(e) => {
                    events
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(BackendEvent::Error(format!("{e}")));
                    return;
                }
            };
            let outcome = match engine.add(parsed, options).await {
                Ok(outcome) => outcome,
                Err(e) => {
                    events
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(BackendEvent::Error(format!("could not add torrent: {e}")));
                    return;
                }
            };
            let view = outcome.torrent;

            if let Some(bytes) = engine.torrent_bytes(view.id) {
                let dir = data_dir.join("torrents");
                let _ = std::fs::create_dir_all(&dir);
                let _ = std::fs::write(
                    dir.join(format!("{}.torrent", view.info_hash.to_ascii_lowercase())),
                    bytes,
                );
            }

            let (id, snapshot) = {
                let mut store = library.lock().unwrap_or_else(|e| e.into_inner());
                let id = store.upsert(
                    &view.info_hash,
                    &re_add_source,
                    view.name.clone(),
                    now_unix(),
                );
                if let Some(entry) = store.entry_mut(id) {
                    entry.files = view.files.iter().map(StoredFile::from_view).collect();
                    entry.primary_file_id = view.primary_file_id;
                    // A stream keeps nothing; a download keeps what the add
                    // selected (the media files).
                    entry.selected_files = if downloading {
                        view.files
                            .iter()
                            .filter(|file| file.included)
                            .map(|file| file.id)
                            .collect()
                    } else {
                        Vec::new()
                    };
                    entry.downloading = downloading;
                }
                (id, store.clone())
            };
            let _ = snapshot.save(&data_dir.join("library.json"));

            let title = view.name.clone().unwrap_or_else(|| view.info_hash.clone());
            events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(BackendEvent::Added { id, title });
        });
    }

    fn set_paused(&self, id: usize, paused: bool) {
        let Some(entry) = self.snapshot().entry(id).cloned() else {
            return;
        };
        let engine = self.engine.clone();
        let downloading = entry.downloading;
        match self
            .live_map()
            .get(&entry.info_hash.to_ascii_lowercase())
            .copied()
        {
            Some(engine_id) => {
                self.spawn_result(if paused { "pause" } else { "resume" }, async move {
                    if paused {
                        engine.pause(engine_id).await
                    } else {
                        engine.resume(engine_id).await
                    }
                });
            }
            None if !paused => self.recycle(entry, downloading, None, false),
            None => {}
        }
    }

    fn remove(&self, id: usize, delete_files: bool) {
        let engine = self.engine.clone();
        let state = self.catalog.clone();
        let library = self.library.clone();
        let data_dir = self.catalog.data_dir.clone();
        let entry = self.snapshot().entry(id).cloned();

        if let Some(entry) = entry.clone() {
            if let Some(path) = Some(LibraryStore::torrent_path(&data_dir, &entry)) {
                let _ = std::fs::remove_file(path);
            }
            let mut store = library.lock().unwrap_or_else(|e| e.into_inner());
            store.remove(id);
            let snapshot = store.clone();
            drop(store);
            let _ = snapshot.save(&data_dir.join("library.json"));
        }

        self.runtime.spawn(async move {
            if let Some(entry) = entry {
                if let Some(engine_id) = engine.id_for_hash(&entry.info_hash) {
                    if let Err(e) = engine.remove(engine_id, delete_files).await {
                        tracing::warn!(error = %e, "remove failed");
                    }
                }
                let mut history = state.history.lock().unwrap_or_else(|e| e.into_inner());
                let _ = history.forget(&entry.info_hash);
            }
        });
    }

    fn set_only_files(&self, id: usize, files: &[usize]) {
        if files.is_empty() {
            tracing::warn!(id, "refusing to select no files");
            return;
        }
        // Remember the selection so a download can be restored after a restart.
        {
            let mut store = self.library.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = store.entry_mut(id) {
                entry.selected_files = files.to_vec();
            }
            let snapshot = store.clone();
            drop(store);
            let _ = snapshot.save(&self.library_path());
        }

        let Some(entry) = self.snapshot().entry(id).cloned() else {
            return;
        };
        let Some(engine_id) = self
            .live_map()
            .get(&entry.info_hash.to_ascii_lowercase())
            .copied()
        else {
            return;
        };
        let engine = self.engine.clone();
        let files = files.to_vec();
        self.spawn_result("select files", async move {
            engine.set_only_files(engine_id, &files).await
        });
    }

    fn start_files(&self, id: usize, files: &[usize]) {
        if files.is_empty() {
            tracing::warn!(id, "refusing to start an empty selection");
            return;
        }
        let Some(entry) = self.snapshot().entry(id).cloned() else {
            return;
        };

        // The file being watched, plus anything already being kept, is what the
        // torrent should fetch.
        let mut selection = files.to_vec();
        for file in &entry.selected_files {
            if !selection.contains(file) {
                selection.push(*file);
            }
        }
        selection.sort_unstable();
        selection.dedup();

        // Mark it playing so the session cache never evicts what is on screen.
        *self.playing.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
        self.touch_cache(id);

        let live_id = self
            .live_map()
            .get(&entry.info_hash.to_ascii_lowercase())
            .copied();
        let events = self.events.clone();
        let info_hash = entry.info_hash.clone();
        // A cached stream already has bytes on hand and is initialised, so it
        // can just resume. A freshly added torrent has nothing yet and cannot
        // accept a selection change while it initialises, so it is re-added.
        let has_buffer = self
            .item(id)
            .is_some_and(|item| item.torrent.stats.progress_bytes > 0);
        let was_paused = self
            .item(id)
            .is_some_and(|item| item.torrent.stats.is_paused());
        tracing::info!(
            id,
            live = live_id.is_some(),
            cache_hit = has_buffer,
            kept = entry.selected_files.len(),
            selections = selection.len(),
            "starting a stream"
        );
        match live_id {
            Some(engine_id) if has_buffer || !entry.selected_files.is_empty() => {
                let engine = self.engine.clone();
                self.runtime.spawn(async move {
                    // Resume before changing the selection: `update_only_files`
                    // is refused while a torrent is paused/initialising, and
                    // retrying for its full budget before ever resuming is a
                    // 20-second stall with no playback.
                    if was_paused {
                        if let Err(e) = engine.resume(engine_id).await {
                            tracing::warn!(error = %e, "could not start the stream");
                        }
                    }
                    if let Err(e) = engine.set_only_files(engine_id, &selection).await {
                        tracing::warn!(error = %e, "could not set the stream selection");
                    }
                    events
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(BackendEvent::Ready { info_hash });
                });
            }
            // In the session but not yet buffered: re-add so the selection is
            // applied as it initialises. Not in the session (evicted or never
            // played) is the same path. `recycle` keeps the same engine id, so
            // a player holding the old URL still finds it.
            _ => self.recycle(entry, false, Some(selection), false),
        }
    }

    fn download_media(&self, items: Vec<MediaDownload>) {
        if items.is_empty() {
            return;
        }
        let snapshot = self.snapshot();
        let now = now_unix();
        let mut affected: HashSet<String> = HashSet::new();
        {
            let mut media = self.media.lock().unwrap_or_else(|e| e.into_inner());
            // A key can name files held by more than one torrent; merge them.
            let mut by_key: HashMap<String, Vec<MediaRef>> = HashMap::new();
            for item in &items {
                let Some(entry) = snapshot.entry(item.torrent_id) else {
                    continue;
                };
                affected.insert(entry.info_hash.clone());
                for id in &item.files {
                    if let Some(file) = entry.files.iter().find(|file| file.id == *id) {
                        by_key
                            .entry(item.key.clone())
                            .or_default()
                            .push(MediaRef::new(entry.info_hash.clone(), file.path.clone()));
                    }
                }
            }
            for (key, files) in by_key {
                if files.is_empty() {
                    continue;
                }
                // If this media was previously kept from another source, that
                // source is affected too and has to drop the download.
                if let Some(old) = media.get(&key) {
                    for file in &old.files {
                        affected.insert(file.info_hash.clone());
                    }
                }
                media.keep(key, files, now);
            }
        }
        self.save_media();
        let modes: Vec<(String, bool)> = affected
            .iter()
            .map(|hash| {
                (
                    hash.clone(),
                    snapshot.by_hash(hash).is_some_and(|entry| entry.downloading),
                )
            })
            .collect();
        self.rehydrate_downloads();
        self.apply_download_state(&modes);
    }

    fn stop_media_download(&self, items: Vec<MediaDownload>) {
        if items.is_empty() {
            return;
        }
        let snapshot = self.snapshot();
        let mut affected: HashSet<String> = HashSet::new();
        {
            let mut media = self.media.lock().unwrap_or_else(|e| e.into_inner());
            for item in &items {
                let Some(entry) = snapshot.entry(item.torrent_id) else {
                    continue;
                };
                // If this is the media's last file, the whole record goes and
                // every source it referenced is affected.
                if let Some(record) = media.get(&item.key) {
                    for file in &record.files {
                        affected.insert(file.info_hash.clone());
                    }
                }
                affected.insert(entry.info_hash.clone());
                for id in &item.files {
                    if let Some(file) = entry.files.iter().find(|file| file.id == *id) {
                        media.stop_file(
                            &item.key,
                            &MediaRef::new(entry.info_hash.clone(), file.path.clone()),
                        );
                    }
                }
            }
        }
        self.save_media();
        let modes: Vec<(String, bool)> = affected
            .iter()
            .map(|hash| {
                (
                    hash.clone(),
                    snapshot.by_hash(hash).is_some_and(|entry| entry.downloading),
                )
            })
            .collect();
        self.rehydrate_downloads();
        self.apply_download_state(&modes);
    }

    fn choose_source(&self, key: String, torrent_id: usize, file_id: usize) {
        let Some(entry) = self.snapshot().entry(torrent_id).cloned() else {
            return;
        };
        let Some(file) = entry.files.iter().find(|file| file.id == file_id) else {
            return;
        };
        let reference = MediaRef::new(entry.info_hash.clone(), file.path.clone());
        {
            let mut media = self.media.lock().unwrap_or_else(|e| e.into_inner());
            media.choose(key, reference);
        }
        self.save_media();
    }

    fn chosen_source(&self, key: &str) -> Option<(usize, usize)> {
        let reference = self
            .media
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .chosen(key)
            .cloned()?;
        let entry = self.snapshot().by_hash(&reference.info_hash)?.clone();
        let file = entry.files.iter().find(|file| file.path == reference.path)?;
        Some((entry.id, file.id))
    }

    fn stop_streaming(&self, id: usize) {
        let Some(entry) = self.snapshot().entry(id).cloned() else {
            return;
        };
        if entry.downloading {
            return;
        }
        // The engine's own per-stream look-ahead bounds fetching now; there is
        // no separate governor to reset.
        {
            let mut playing = self.playing.lock().unwrap_or_else(|e| e.into_inner());
            if *playing == Some(id) {
                *playing = None;
            }
        }

        // Keep the buffer: pause instead of removing, so a replay in this
        // session is instant. The session cache bounds how much is kept; the
        // least recently watched streams are released when it overflows.
        // Do NOT pause. A stream-only torrent with no active stream fetches
        // nothing (the picker has no window to prioritise), so leaving it live
        // costs no bandwidth, keeps the peer connections warm, and keeps the
        // buffer — which is what makes the replay instant. Pausing instead
        // dropped the peers and is where a resume could stall.
        let progress = self
            .item(id)
            .map(|item| item.torrent.stats.progress_bytes)
            .unwrap_or(0);
        tracing::info!(id, progress, "kept a stream in the session cache");
        self.touch_cache(id);
        self.enforce_stream_cache(Some(id));
    }

    fn restart_stream(&self, id: usize, files: &[usize]) {
        if files.is_empty() {
            return;
        }
        let Some(entry) = self.snapshot().entry(id).cloned() else {
            return;
        };
        tracing::warn!(id, "restarting a stalled stream from scratch");
        *self.playing.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
        self.recycle(entry, false, Some(files.to_vec()), false);
    }

    fn is_live(&self, id: usize) -> bool {
        self.snapshot()
            .entry(id)
            .map(|entry| entry.info_hash.to_ascii_lowercase())
            .is_some_and(|hash| self.live_map().contains_key(&hash))
    }

    fn note_playback(&self, id: usize, file_id: usize, position: f64, duration: Option<f64>) {
        // Fetching is bounded by the engine's per-stream look-ahead window,
        // driven by how far the HTTP stream has actually read. There is no
        // separate governor that pauses and resumes the whole torrent: that
        // used to fight the window (its `progress_bytes` heuristic is wrong for
        // a windowed fetch) and could stall startup with nothing to resume it.
        tracing::trace!(id, file_id, position, duration = ?duration, "playback position");
    }

    fn record_watch(
        &self,
        info_hash: &str,
        file_id: usize,
        file_name: Option<String>,
        title: Option<String>,
        position: f64,
        duration: Option<f64>,
    ) {
        let progress = WatchProgress {
            position,
            duration: duration.filter(|d| d.is_finite() && *d > 0.0),
            updated_at: now_unix(),
            file_name: file_name.clone(),
            title: title.clone(),
        };
        let mut history = self.catalog.history.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = history.record_file(info_hash, file_id, file_name, title, position, duration) {
            tracing::warn!(error = %e, "could not record watch position");
            return;
        }
        drop(history);

        // Also record it against the media, so the position follows the episode
        // to whichever release holds it next.
        if let Some(key) = self.media_watch_key(info_hash, file_id) {
            let mut media = self.media.lock().unwrap_or_else(|e| e.into_inner());
            media.record_watch(key, progress);
        }

        // `record` writes every time; throttle it so a playing film does not
        // rewrite the file several times a second.
        let mut last = self
            .catalog
            .last_history_write
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if last.elapsed() >= HISTORY_WRITE_INTERVAL {
            *last = Instant::now();
            let history = self.catalog.history.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = history.save() {
                tracing::warn!(error = %e, "could not save watch history");
            }
            drop(history);
            self.save_media();
        }
    }

    fn mark_finished(&self, info_hash: &str, file_id: usize, title: Option<String>) {
        let mut history = self.catalog.history.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = history.mark_file_finished(info_hash, file_id, None, title) {
            tracing::warn!(error = %e, "could not mark as finished");
            return;
        }
        let finished = history.get_file(info_hash, file_id).cloned();
        drop(history);
        if let (Some(key), Some(progress)) = (self.media_watch_key(info_hash, file_id), finished) {
            let mut media = self.media.lock().unwrap_or_else(|e| e.into_inner());
            media.record_watch(key, progress);
            drop(media);
            self.save_media();
        }
        self.push(BackendEvent::WatchUpdated);
    }

    fn forget_watch(&self, info_hash: &str, file_id: usize) {
        let mut history = self.catalog.history.lock().unwrap_or_else(|e| e.into_inner());
        let _ = history.forget_file(info_hash, file_id);
        drop(history);
        if let Some(key) = self.media_watch_key(info_hash, file_id) {
            let mut media = self.media.lock().unwrap_or_else(|e| e.into_inner());
            media.forget_watch(&key);
            drop(media);
            self.save_media();
        }
        self.push(BackendEvent::WatchUpdated);
    }

    fn search(&self, query: &str) {
        let query = query.trim().to_string();
        if query.is_empty() {
            return;
        }

        let state = self.catalog.clone();
        let events = self.events.clone();

        self.runtime.spawn(async move {
            let request = reel_catalog::SearchQuery::new(query.clone());
            let outcome = state.search.search(&request).await;

            let event = match outcome {
                Ok(results) => {
                    tracing::info!(
                        query = %query,
                        hits = results.hits.len(),
                        failures = results.failures.len(),
                        "search finished"
                    );
                    BackendEvent::SearchResults {
                        query: query.clone(),
                        results,
                    }
                }
                Err(e) => BackendEvent::SearchFailed {
                    query: query.clone(),
                    message: e.to_string(),
                },
            };

            events.lock().unwrap_or_else(|e| e.into_inner()).push(event);
        });
    }

    fn search_sources(&self) -> Vec<SourceInfo> {
        self.catalog
            .search
            .names()
            .into_iter()
            .map(|name| SourceInfo {
                configured: self.catalog.search.configured().contains(&name),
                name: name.to_string(),
            })
            .collect()
    }

    fn catalog_status(&self) -> CatalogStatus {
        let items = self.library();
        let enriched = items.iter().filter(|i| i.entry.metadata.is_some()).count();
        let pending = {
            let in_flight = self.catalog.in_flight.lock().unwrap_or_else(|e| e.into_inner());
            in_flight.len()
        };

        let settings = self.catalog.settings.lock().unwrap_or_else(|e| e.into_inner());
        let source = settings.key_source();
        drop(settings);

        CatalogStatus {
            provider: self.catalog.provider().name().to_string(),
            configured: self.catalog.provider().is_configured(),
            key_source: source.describe().to_string(),
            // Settings are canonical now, so the key can always be changed here
            // even when the environment supplied the current one.
            can_set_key: true,
            note: self.catalog.note(),
            cached_metadata: self.catalog.cache.metadata_count(),
            cache_bytes: self.catalog.cache.total_bytes(),
            cache_dir: self.catalog.cache.root().display().to_string(),
            enriched,
            pending,
        }
    }

    fn settings(&self) -> CatalogSettings {
        self.catalog
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn save_settings(&self, mut settings: CatalogSettings) {
        settings.normalise();

        let previous = self.settings();
        let key_changed = previous.stored_key() != settings.stored_key();

        {
            let mut current = self.catalog.settings.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = settings.save(&self.catalog.data_dir) {
                self.push(BackendEvent::Error(format!(
                    "could not save settings: {e}"
                )));
                return;
            }
            *current = settings.clone();
        }

        if key_changed {
            let provider =
                provider_from_key(settings.stored_key().as_deref(), self.catalog.cache.clone());
            *self
                .catalog
                .provider
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = provider;

            // A new key means the answers change, so ask again.
            self.catalog
                .attempted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            self.catalog
                .metadata
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            self.enrich_library();
        }

        self.push(BackendEvent::Info("Settings saved".to_string()));
    }

    fn set_api_key(&self, key: Option<String>) {
        let mut settings = self.settings();
        settings.tmdb_api_key = key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
        self.save_settings(settings);
    }

    fn test_api_key(&self, key: String) {
        let events = self.events.clone();
        let cache_root = self.catalog.cache.root().to_path_buf();

        self.runtime.spawn(async move {
            let message = match TmdbClient::new(key) {
                Ok(client) => match client.ping().await {
                    Ok(()) => "The key works. Posters and synopses will load.".to_string(),
                    Err(e) => e.user_message(),
                },
                Err(e) => e.user_message(),
            };
            let ok = message.starts_with("The key works");
            let _ = cache_root;
            events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(BackendEvent::ApiKeyChecked { ok, message });
        });
    }

    fn clear_catalog_cache(&self) {
        if let Err(e) = self.catalog.cache.clear() {
            self.push(BackendEvent::Error(format!("clearing the catalog cache: {e}")));
            return;
        }
        self.catalog
            .metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.catalog
            .attempted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.push(BackendEvent::Info(
            "Cleared cached posters and metadata".to_string(),
        ));
    }

    fn refresh_metadata(&self) {
        self.catalog
            .attempted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.catalog
            .metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.enrich_library();
        self.push(BackendEvent::Info("Looking up metadata again".to_string()));
    }

    fn take_events(&self) -> Vec<BackendEvent> {
        std::mem::take(&mut *self.events.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Drop for EngineBackend {
    fn drop(&mut self) {
        // Stop the HTTP server, flush the watch history, then flush fast-resume
        // state so the next start is instant.
        if let Some(shutdown) = self
            .shutdown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = shutdown.send(());
        }

        {
            let history = self.catalog.history.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = history.save() {
                tracing::warn!(error = %e, "could not save watch history on exit");
            }
        }

        let engine = self.engine.clone();
        self.runtime.block_on(async move {
            engine.shutdown().await;
        });
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
        }
    }

    /// Replace the whole library, as a test would between assertions.
    pub fn set_items(&self, items: Vec<LibraryItem>) {
        *self.items.lock().unwrap_or_else(|e| e.into_inner()) = items;
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
        self.items
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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

    fn search(&self, query: &str) {
        // Answer with fixtures, so the UI can be driven without a network.
        let mut results = SearchResults::default();
        for hit in crate::testing::sample_search_hits() {
            if hit.title.to_lowercase().contains(&query.to_lowercase())
                || query.trim().is_empty()
            {
                results.hits.push(hit);
            }
        }
        results.hits.sort_by_key(|hit| std::cmp::Reverse(hit.sort_key()));

        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(BackendEvent::SearchResults {
                query: query.to_string(),
                results,
            });
    }

    fn search_sources(&self) -> Vec<SourceInfo> {
        vec![SourceInfo {
            name: "demo".to_string(),
            configured: true,
        }]
    }

    fn catalog_status(&self) -> CatalogStatus {
        let mut status = self.catalog_status.clone();
        let items = self.library();
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
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use crate::testing::sample_library;

    use super::*;

    fn entry_without_name() -> LibraryEntry {
        let item = sample_library().into_iter().next().expect("a sample");
        LibraryEntry {
            id: 0,
            info_hash: item.torrent.info_hash.clone(),
            source: String::new(),
            name: None,
            files: Vec::new(),
            primary_file_id: None,
            selected_files: Vec::new(),
            downloading: false,
            added_at: 0,
        }
    }

    #[test]
    fn a_stream_path_names_the_torrent_and_the_file() {
        let path = stream_path(0, 3, "Show S01E04.mkv");
        assert_eq!(path, "/stream/0/3/Show%20S01E04.mkv");
        // Always `stream / <torrent> / <file> / <name>`: a missing file id
        // makes the router treat the file name as the id and answer 400.
        assert_eq!(path.trim_start_matches('/').split('/').count(), 4);
    }

    #[test]
    fn an_unresolved_magnet_is_not_called_by_its_hash() {
        let entry = entry_without_name();
        let (title, year) = display_title_for_entry(&entry);
        assert_eq!(title, "Resolving magnet\u{2026}");
        assert_eq!(year, None);
        assert!(
            !title.contains(&entry.info_hash),
            "the info hash should never be shown as a title"
        );
    }

    #[test]
    fn an_empty_name_is_treated_as_no_name() {
        let mut entry = entry_without_name();
        entry.name = Some("   ".to_string());
        assert_eq!(display_title_for_entry(&entry).0, "Resolving magnet\u{2026}");
    }

    #[test]
    fn a_real_name_is_cleaned_as_usual() {
        let mut entry = entry_without_name();
        entry.name = Some("The.Matrix.1999.1080p.BluRay.x264-GROUP".to_string());
        let (title, year) = display_title_for_entry(&entry);
        assert_eq!(title, "The Matrix");
        assert_eq!(year, Some(1999));
    }
}
