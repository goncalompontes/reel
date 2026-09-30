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
    ArchiveOrgBackend, CatalogCache, CatalogEntry, Metadata, MetadataProvider, SearchAggregator,
    SearchResults, WatchHistory, WatchProgress, default_data_dir, provider_from_key,
};
use reel_core::model::TorrentView;
use reel_core::title::clean_title;
use reel_core::{AddOptions, AddSource, Engine, EngineConfig};

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
    /// Watch positions changed.
    WatchUpdated,
    /// Results for a search. Carries the query so a stale answer for an older
    /// query can be recognised and dropped.
    SearchResults { query: String, results: SearchResults },
    /// A search that could not run at all.
    SearchFailed { query: String, message: String },
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
    /// Download only this file of a torrent.
    fn set_only_file(&self, id: usize, file_id: usize);

    /// Remember how far through a torrent playback got.
    fn record_watch(
        &self,
        info_hash: &str,
        file_name: Option<String>,
        title: Option<String>,
        position: f64,
        duration: Option<f64>,
    );
    fn mark_finished(&self, info_hash: &str, title: Option<String>);
    fn forget_watch(&self, info_hash: &str);

    /// Start a search. Results arrive as [`BackendEvent::SearchResults`].
    fn search(&self, query: &str);
    /// The configured search sources, for the settings page.
    fn search_sources(&self) -> Vec<SourceInfo>;

    fn catalog_status(&self) -> CatalogStatus;
    /// Clear cached metadata and artwork, then enrich again.
    fn clear_catalog_cache(&self);
    /// Forget what we know and look everything up again.
    fn refresh_metadata(&self);

    fn take_events(&self) -> Vec<BackendEvent>;
}

// ------------------------------------------------------------ real backend

struct CatalogState {
    provider: Arc<dyn MetadataProvider>,
    cache: Arc<CatalogCache>,
    metadata: Mutex<HashMap<String, Metadata>>,
    in_flight: Mutex<HashSet<String>>,
    attempted: Mutex<HashSet<String>>,
    history: Mutex<WatchHistory>,
    last_history_write: Mutex<Instant>,
    search: SearchAggregator,
}

impl CatalogState {
    fn note(&self) -> Option<String> {
        if self.provider.is_configured() {
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
}

impl EngineBackend {
    /// Start the engine, serve the streaming API on an ephemeral localhost port,
    /// and build the metadata provider.
    pub fn start(config: EngineConfig) -> anyhow::Result<Self> {
        Self::start_with_options(config, CatalogOptions::from_env())
    }

    pub fn start_with_options(
        config: EngineConfig,
        catalog_options: CatalogOptions,
    ) -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("reel-backend")
            .build()?;

        let download_dir = config.download_dir.display().to_string();
        let client_name = config.client_name.clone();

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
            provider,
            cache,
            metadata: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
            attempted: Mutex::new(HashSet::new()),
            history: Mutex::new(history),
            last_history_write: Mutex::new(Instant::now() - HISTORY_WRITE_INTERVAL),
            search,
        });

        let backend = Self {
            runtime,
            engine,
            base_url,
            events: Arc::new(Mutex::new(Vec::new())),
            capabilities,
            shutdown: Mutex::new(Some(shutdown_tx)),
            catalog,
        };

        // Kick off enrichment for whatever is already in the session.
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

    /// Look up metadata for every torrent we have not tried yet.
    ///
    /// Bounded by [`MAX_CONCURRENT_LOOKUPS`], and each title is attempted once
    /// per session so a provider that does not know a film is not asked again
    /// on every refresh.
    fn enrich_library(&self) {
        if !self.catalog.provider.is_configured() {
            return;
        }

        let torrents = self.engine.list();
        let mut to_start = Vec::new();

        for torrent in torrents {
            let key = torrent.info_hash.clone();

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

            to_start.push((key, torrent.name.clone()));
        }

        for (info_hash, name) in to_start {
            self.spawn_lookup(info_hash, name);
        }
    }

    fn spawn_lookup(&self, info_hash: String, name: Option<String>) {
        let provider = self.catalog.provider.clone();
        let state = self.catalog.clone();
        let events = self.events.clone();

        self.runtime.spawn(async move {
            // The same cleaner the UI uses, so the provider sees a title rather
            // than a filename.
            let query = reel_catalog::LookupQuery::from_release_name(
                name.as_deref().unwrap_or(&info_hash),
            );

            let result = provider.lookup(query).await;

            state
                .in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&info_hash);

            match result {
                Ok(Some(metadata)) => {
                    tracing::info!(
                        %info_hash,
                        title = %metadata.title,
                        "matched metadata"
                    );
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
                Ok(None) => {
                    tracing::debug!(%info_hash, "no metadata match");
                }
                Err(e) => {
                    // A missing key or an offline machine must not surface as a
                    // scary error for every title in the library.
                    tracing::debug!(%info_hash, error = %e, "metadata lookup failed");
                }
            }
        });
    }

    fn entry_for(&self, torrent: &TorrentView) -> LibraryItem {
        let clean = clean_title(torrent.name.as_deref().unwrap_or(&torrent.info_hash));

        let metadata = self
            .catalog
            .metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&torrent.info_hash)
            .cloned();

        let watch = self
            .catalog
            .history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&torrent.info_hash)
            .cloned();

        LibraryItem {
            torrent: torrent.clone(),
            entry: CatalogEntry {
                torrent_id: torrent.id,
                info_hash: torrent.info_hash.clone(),
                display_title: clean.title,
                year: clean.year,
                metadata,
                watch,
            },
        }
    }
}

/// Where the metadata key and cache live. Read once at startup.
#[derive(Debug, Clone)]
pub struct CatalogOptions {
    pub api_key: Option<String>,
    pub data_dir: std::path::PathBuf,
    /// Leave the bundled search source out entirely.
    pub disable_bundled_sources: bool,
}

impl CatalogOptions {
    /// `REEL_TMDB_API_KEY` overrides the stored key, which is convenient for
    /// running without writing anything to disk.
    pub fn from_env() -> Self {
        Self {
            api_key: std::env::var("REEL_TMDB_API_KEY")
                .ok()
                .filter(|key| !key.trim().is_empty()),
            data_dir: default_data_dir(),
            disable_bundled_sources: std::env::var("REEL_NO_BUNDLED_SOURCES")
                .map(|value| value != "0")
                .unwrap_or(false),
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

        let mut torrents = self.engine.list();
        for torrent in &mut torrents {
            torrent.with_base_url(&self.base_url);
        }
        torrents.iter().map(|t| self.entry_for(t)).collect()
    }

    fn item(&self, id: usize) -> Option<LibraryItem> {
        let mut torrent = self.engine.view(id).ok()?;
        torrent.with_base_url(&self.base_url);
        Some(self.entry_for(&torrent))
    }

    fn add(&self, source: &str, media_only: bool) {
        let engine = self.engine.clone();
        let events = self.events.clone();
        let source = source.to_string();

        self.runtime.spawn(async move {
            let options = AddOptions {
                media_only,
                ..Default::default()
            };
            let result = match AddSource::detect(&source) {
                Ok(parsed) => engine.add(parsed, options).await,
                Err(e) => {
                    events
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(BackendEvent::Error(format!("{e}")));
                    return;
                }
            };

            let mut queue = events.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                Ok(outcome) => {
                    let title = outcome
                        .torrent
                        .name
                        .clone()
                        .unwrap_or_else(|| outcome.torrent.info_hash.clone());
                    queue.push(BackendEvent::Added {
                        id: outcome.torrent.id,
                        title,
                    });
                }
                Err(e) => queue.push(BackendEvent::Error(format!("could not add torrent: {e}"))),
            }
        });
    }

    fn set_paused(&self, id: usize, paused: bool) {
        let engine = self.engine.clone();
        self.spawn_result(if paused { "pause" } else { "resume" }, async move {
            if paused {
                engine.pause(id).await
            } else {
                engine.resume(id).await
            }
        });
    }

    fn remove(&self, id: usize, delete_files: bool) {
        let engine = self.engine.clone();
        // Remember the info hash so the watch history entry goes with it.
        let info_hash = self.engine.view(id).ok().map(|view| view.info_hash);
        let state = self.catalog.clone();
        let events = self.events.clone();

        self.runtime.spawn(async move {
            if let Err(e) = engine.remove(id, delete_files).await {
                events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(BackendEvent::Error(format!("remove: {e}")));
                return;
            }
            if let Some(hash) = info_hash {
                let mut history = state.history.lock().unwrap_or_else(|e| e.into_inner());
                let _ = history.forget(&hash);
            }
        });
    }

    fn set_only_file(&self, id: usize, file_id: usize) {
        let engine = self.engine.clone();
        self.spawn_result("select file", async move {
            engine.set_only_files(id, &[file_id]).await
        });
    }

    fn record_watch(
        &self,
        info_hash: &str,
        file_name: Option<String>,
        title: Option<String>,
        position: f64,
        duration: Option<f64>,
    ) {
        let mut history = self.catalog.history.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = history.record(info_hash, file_name, title, position, duration) {
            tracing::warn!(error = %e, "could not record watch position");
            return;
        }
        drop(history);

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
        }
    }

    fn mark_finished(&self, info_hash: &str, title: Option<String>) {
        let mut history = self.catalog.history.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = history.mark_finished(info_hash, None, title) {
            tracing::warn!(error = %e, "could not mark as finished");
            return;
        }
        drop(history);
        self.push(BackendEvent::WatchUpdated);
    }

    fn forget_watch(&self, info_hash: &str) {
        let mut history = self.catalog.history.lock().unwrap_or_else(|e| e.into_inner());
        let _ = history.forget(info_hash);
        drop(history);
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

        CatalogStatus {
            provider: self.catalog.provider.name().to_string(),
            configured: self.catalog.provider.is_configured(),
            note: self.catalog.note(),
            cached_metadata: self.catalog.cache.metadata_count(),
            cache_bytes: self.catalog.cache.total_bytes(),
            cache_dir: self.catalog.cache.root().display().to_string(),
            enriched,
            pending,
        }
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
    events: Mutex<Vec<BackendEvent>>,
    capabilities: BackendCapabilities,
    base_url: String,
    catalog_status: CatalogStatus,
}

impl FakeBackend {
    pub fn new(items: Vec<LibraryItem>) -> Self {
        Self {
            items: Mutex::new(items),
            events: Mutex::new(Vec::new()),
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

    fn set_only_file(&self, id: usize, file_id: usize) {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(item) = items.iter_mut().find(|i| i.torrent.id == id) {
            for file in &mut item.torrent.files {
                file.included = file.id == file_id;
            }
        }
    }

    fn record_watch(
        &self,
        info_hash: &str,
        file_name: Option<String>,
        _title: Option<String>,
        position: f64,
        duration: Option<f64>,
    ) {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(item) = items.iter_mut().find(|i| i.entry.info_hash == info_hash) {
            item.entry.watch = Some(WatchProgress {
                position,
                duration,
                updated_at: now_unix(),
                file_name,
                title: None,
            });
        }
    }

    fn mark_finished(&self, info_hash: &str, _title: Option<String>) {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(item) = items.iter_mut().find(|i| i.entry.info_hash == info_hash) {
            let duration = item.entry.watch.as_ref().and_then(|w| w.duration).unwrap_or(100.0);
            item.entry.watch = Some(WatchProgress {
                position: duration,
                duration: Some(duration),
                updated_at: now_unix(),
                file_name: None,
                title: None,
            });
        }
    }

    fn forget_watch(&self, info_hash: &str) {
        let mut items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(item) = items.iter_mut().find(|i| i.entry.info_hash == info_hash) {
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
