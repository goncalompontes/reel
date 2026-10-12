//! The real backend: engine, catalogue and streaming in one process.

use super::*;

// ------------------------------------------------------------ real backend

pub(crate) struct CatalogState {
    /// Swappable: setting a key in Settings has to take effect without a restart.
    provider: Mutex<Arc<dyn MetadataProvider>>,
    settings: Mutex<CatalogSettings>,
    data_dir: std::path::PathBuf,
    cache: Arc<CatalogCache>,
    metadata: Mutex<HashMap<String, Metadata>>,
    in_flight: Mutex<HashSet<String>>,
    attempted: Mutex<HashSet<String>>,
    /// Metadata matched to search results, keyed by title and year, and the set
    /// of keys already looked up so a miss is not retried every refresh.
    search_metadata: Mutex<HashMap<String, Metadata>>,
    /// Memoized raw-release-name -> parsed-work key. The UI asks for metadata on
    /// every visible row every frame, so parsing the release name must not recur
    /// on that hot path.
    search_key_cache: Mutex<HashMap<String, String>>,
    /// Memoized torrent -> parsed release, keyed by info hash and a fingerprint
    /// of its file list. `library()` is polled a few times a second and used to
    /// re-parse every file name with `hunch` on each poll, which stuttered the
    /// whole UI (and playback) whenever the stats refreshed. The parse only has
    /// to run again when the files actually change (e.g. a magnet resolving).
    release_cache: Mutex<HashMap<String, (u64, Release)>>,
    search_attempted: Mutex<HashSet<String>>,
    /// Distinct metadata identities considered for the current search. Unlike
    /// the process-wide attempt cache, this is reset for each new query and is
    /// the actual per-search request budget.
    search_budget_used: Mutex<HashSet<String>>,
    /// Search metadata lookups still running.
    search_in_flight: Mutex<usize>,
    /// Last time a search metadata request started, shared across searches.
    search_lookup_started: Mutex<Option<Instant>>,
    /// Ensures only one timer wakes the UI to dispatch the next paced request.
    search_retry_scheduled: Mutex<bool>,
    history: Mutex<WatchHistory>,
    last_history_write: Mutex<Instant>,
    search: Mutex<Arc<SearchAggregator>>,
}

impl CatalogState {
    pub(crate) fn provider(&self) -> Arc<dyn MetadataProvider> {
        self.provider
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The current search backends. Swapped when the search settings change.
    pub(crate) fn search(&self) -> Arc<SearchAggregator> {
        self.search
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn search_key(&self, hit: &reel_catalog::SearchHit) -> String {
        let raw_key = format!(
            "{}|{}",
            hit.title,
            hit.year.map_or_else(String::new, |year| year.to_string())
        );
        let mut cache = self
            .search_key_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cache
            .entry(raw_key)
            .or_insert_with(|| search_metadata_key(hit))
            .clone()
    }

    /// Parse a torrent's files into a release, reusing the last result while the
    /// file list is unchanged. This is the hot path behind `library()`, which the
    /// UI polls several times a second.
    pub(crate) fn cached_release(
        &self,
        info_hash: &str,
        signature: u64,
        compute: impl FnOnce() -> Release,
    ) -> Release {
        let mut cache = self
            .release_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((cached_signature, release)) = cache.get(info_hash) {
            if *cached_signature == signature {
                return release.clone();
            }
        }
        let release = compute();
        cache.insert(info_hash.to_string(), (signature, release.clone()));
        release
    }

    pub(crate) fn note(&self) -> Option<String> {
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
    /// Last measured cache figures, so the settings page does not walk the
    /// cache directory on every frame.
    cache_stats: Mutex<Option<CacheStats>>,
    /// Casting to DLNA renderers, when enabled and available. Swappable while
    /// running: toggling the setting restarts the media server.
    cast: Mutex<Option<Arc<CastService>>>,
    /// Why casting is off, when `cast` is `None`.
    cast_unavailable: Mutex<Option<String>>,
    /// The casting settings last applied, to detect changes on save.
    cast_config: Mutex<Option<CastConfig>>,
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

        // Network settings from the Settings tab. They are fixed at engine
        // start, so a change takes effect on the next run.
        {
            let settings = &catalog_options.settings;
            config.listen_port = settings.listen_port.filter(|port| *port != 0);
            config.disable_dht = !settings.dht_enabled;
            config.disable_trackers = !settings.trackers_enabled;
            config.enable_upnp = settings.enable_upnp;
            config.ipv4_only = settings.ipv4_only;
            config.peer_limit = settings.peer_limit;
        }

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
                        PlayerCapability::Unavailable {
                            reason: note.clone(),
                        },
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

        // ext.to is the only bundled backend. Whether it is registered and its
        // credentials come from saved settings; `disable_bundled_sources` is the
        // test/CLI master switch that turns it off.
        let search = Arc::new(build_search(
            &catalog_options.settings,
            catalog_options.disable_bundled_sources,
        ));

        // Warm the sources in the background: ext.to clears its Cloudflare
        // challenge here, so the first search is not the one that waits for it.
        {
            let warm = search.clone();
            runtime.spawn(async move {
                warm.warm().await;
            });
        }

        let catalog = Arc::new(CatalogState {
            provider: Mutex::new(provider),
            settings: Mutex::new(catalog_options.settings.clone()),
            data_dir: catalog_options.data_dir.clone(),
            cache,
            metadata: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
            attempted: Mutex::new(HashSet::new()),
            search_metadata: Mutex::new(HashMap::new()),
            search_key_cache: Mutex::new(HashMap::new()),
            release_cache: Mutex::new(HashMap::new()),
            search_attempted: Mutex::new(HashSet::new()),
            search_budget_used: Mutex::new(HashSet::new()),
            search_in_flight: Mutex::new(0),
            search_lookup_started: Mutex::new(None),
            search_retry_scheduled: Mutex::new(false),
            history: Mutex::new(history),
            last_history_write: Mutex::new(Instant::now() - HISTORY_WRITE_INTERVAL),
            search: Mutex::new(search),
        });

        let library = Arc::new(Mutex::new(LibraryStore::load(
            &data_dir.join("library.json"),
        )));
        let media = Arc::new(Mutex::new(MediaStore::load(&MediaStore::path(&data_dir))));

        // Casting is opt-in: it opens a LAN-facing media port, so it only runs
        // when the setting is on and ffmpeg is available.
        let cast_config = CastConfig::from_settings(&catalog_options.settings);
        let (cast, cast_unavailable) = match CastService::new(
            engine.clone(),
            base_url.clone(),
            data_dir.clone(),
            &catalog_options.settings,
            runtime.handle().clone(),
        ) {
            Ok(service) => {
                service.refresh_devices();
                (Some(Arc::new(service)), None)
            }
            Err(reason) => {
                tracing::info!(%reason, "casting is not available");
                (None, Some(reason))
            }
        };

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
            cache_stats: Mutex::new(None),
            cast: Mutex::new(cast),
            cast_unavailable: Mutex::new(cast_unavailable),
            cast_config: Mutex::new(Some(cast_config)),
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

    /// The casting service, when the LAN media server is running.
    ///
    /// `None` means casting is off or unavailable (the setting is disabled, or
    /// ffmpeg is missing).
    pub fn cast(&self) -> Option<Arc<CastService>> {
        self.cast
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Apply casting settings that changed since the last save, without a
    /// restart.
    ///
    /// Toggling the LAN toggle, changing the advertised address or the ffmpeg
    /// path rebuilds the media server (ending any cast in progress); the
    /// conversion mode and height are updated in place.
    fn reconcile_casting(&self, settings: &CatalogSettings) {
        let want = CastConfig::from_settings(settings);
        let previous = self
            .cast_config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if previous.as_ref() == Some(&want) {
            return;
        }

        if previous.as_ref().is_none_or(|old| !old.needs_rebind(&want)) {
            if let Some(cast) = self.cast() {
                cast.set_options(crate::cast::plan_options(settings));
            }
            *self.cast_config.lock().unwrap_or_else(|e| e.into_inner()) = Some(want);
            return;
        }

        // The listener or tools changed: tear the old server down (ending any
        // cast) and, if still wanted, start a fresh one.
        let old = self
            .cast
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(cast) = old {
            cast.shutdown();
        }

        let (cast, unavailable) = if want.enabled {
            match CastService::new(
                self.engine.clone(),
                self.base_url.clone(),
                self.catalog.data_dir.clone(),
                settings,
                self.runtime.handle().clone(),
            ) {
                Ok(service) => {
                    service.refresh_devices();
                    (Some(Arc::new(service)), None)
                }
                Err(reason) => (None, Some(reason)),
            }
        } else {
            (
                None,
                Some("streaming to the local network is turned off".to_string()),
            )
        };

        *self.cast.lock().unwrap_or_else(|e| e.into_inner()) = cast;
        *self.cast_unavailable.lock().unwrap_or_else(|e| e.into_inner()) = unavailable;
        *self.cast_config.lock().unwrap_or_else(|e| e.into_inner()) = Some(want);
    }

    pub(crate) fn push(&self, event: BackendEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
    }

    /// Spawn a fallible engine operation and report the outcome as an event.
    pub(crate) fn spawn_result<F, E>(&self, what: &'static str, future: F)
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
    pub(crate) fn live_map(&self) -> HashMap<String, usize> {
        self.engine
            .list()
            .into_iter()
            .map(|view| (view.info_hash.to_ascii_lowercase(), view.id))
            .collect()
    }

    pub(crate) fn library_path(&self) -> std::path::PathBuf {
        self.catalog.data_dir.join("library.json")
    }

    pub(crate) fn save_library(&self) {
        let library = self.library.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = library.save(&self.library_path()) {
            tracing::warn!(error = %e, "could not save the library");
        }
    }

    /// Clear the saved pause on these sources, because the user asked them to
    /// run again (a new download, or a resume).
    pub(crate) fn clear_paused(&self, hashes: &std::collections::HashSet<String>) {
        let mut store = self.library.lock().unwrap_or_else(|e| e.into_inner());
        let mut changed = false;
        for entry in store.entries.iter_mut() {
            if entry.paused && hashes.contains(&entry.info_hash) {
                entry.paused = false;
                changed = true;
            }
        }
        if changed {
            let snapshot = store.clone();
            drop(store);
            let _ = snapshot.save(&self.library_path());
        }
    }

    /// The per-torrent rate caps from settings, in bytes per second. `0`/unset
    /// means unlimited.
    pub(crate) fn rate_limits(&self) -> (Option<u32>, Option<u32>) {
        let settings = self.settings();
        let kib = |value: Option<u32>| {
            value
                .filter(|kib| *kib > 0)
                .map(|kib| kib.saturating_mul(1024))
        };
        (
            kib(settings.upload_limit_kib),
            kib(settings.download_limit_kib),
        )
    }

    pub(crate) fn snapshot(&self) -> LibraryStore {
        self.library
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Persist the `.torrent` bytes so the title can be re-added offline.
    pub(crate) fn save_torrent_bytes(&self, info_hash: &str, bytes: &[u8]) {
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
    pub(crate) fn recycle(
        &self,
        entry: LibraryEntry,
        downloading: bool,
        start: Option<Vec<usize>>,
        delete_files: bool,
    ) {
        let engine = self.engine.clone();
        let events = self.events.clone();
        let data_dir = self.catalog.data_dir.clone();
        let (upload_limit_bps, download_limit_bps) = self.rate_limits();

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
            // against anyway. A play (`start`) always runs; a restored download
            // honours a pause the user set before the last shutdown.
            let running = start.is_some() || (downloading && !entry.paused);
            let selection = start.clone().filter(|files| !files.is_empty()).or_else(|| {
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
                upload_limit_bps,
                download_limit_bps,
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
            // Only a running (unpaused, selected) torrent can actually be
            // streamed. A demote to idle streaming is re-added paused with no
            // selection, and announcing it as ready would let a racing play open
            // a URL that never produces bytes.
            if running {
                queue.push(BackendEvent::Ready {
                    info_hash: entry.info_hash.clone(),
                });
            }
        });
    }

    /// Re-add every title the user asked to keep.
    pub(crate) fn restore_downloads(&self) {
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
    pub(crate) fn save_media(&self) {
        let media = self.media.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = media.save(&MediaStore::path(&self.catalog.data_dir)) {
            tracing::warn!(error = %e, "could not save the media store");
        }
    }

    /// The media key a file of a torrent belongs to, or `None` if the torrent
    /// is not in the library.
    pub(crate) fn media_watch_key(&self, info_hash: &str, file_id: usize) -> Option<String> {
        let entry = self.snapshot().by_hash(info_hash)?.clone();
        let release = analyse_entry(&entry);
        let work_key = self.work_key_for(&entry, &release);
        Some(Self::media_key_for_release(&work_key, &release, file_id))
    }

    /// Adopt state written before downloads were media-keyed. Runs once, when
    /// the media store has no downloads yet.
    pub(crate) fn adopt_legacy_downloads(&self) {
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
    pub(crate) fn rehydrate_downloads(&self) {
        let media = self.media.lock().unwrap_or_else(|e| e.into_inner()).clone();
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
    pub(crate) fn selected_from_store(entry: &LibraryEntry, media: &MediaStore) -> Vec<usize> {
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
    pub(crate) fn apply_download_state(&self, modes: &[(String, bool)]) {
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
    pub(crate) fn touch_cache(&self, id: usize) {
        self.stream_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Instant::now());
    }

    /// Cached metadata-file count and total bytes, re-measured only when the
    /// memoized value is older than [`CACHE_STATS_TTL`].
    pub(crate) fn cache_stats(&self) -> (usize, u64) {
        let mut slot = self.cache_stats.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = slot
            .as_ref()
            .filter(|stats| stats.measured_at.elapsed() < CACHE_STATS_TTL)
            .map(|stats| (stats.metadata_files, stats.bytes));
        if let Some(figures) = fresh {
            return figures;
        }
        let stats = CacheStats {
            measured_at: Instant::now(),
            metadata_files: self.catalog.cache.metadata_count(),
            bytes: self.catalog.cache.total_bytes(),
        };
        *slot = Some(stats);
        (stats.metadata_files, stats.bytes)
    }

    /// Forget the memoized cache figures, so the next status read re-measures.
    pub(crate) fn invalidate_cache_stats(&self) {
        *self.cache_stats.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Release the least recently watched stream buffers once the session cache
    /// is over its limit. Streams with downloads attached are left alone; the
    /// title currently playing is never evicted.
    pub(crate) fn enforce_stream_cache(&self, keep: Option<usize>) {
        let cap_mb = self.settings().stream_cache_mb;
        let cap = cap_mb as u64 * 1024 * 1024;
        let store = self.snapshot();
        let playing = *self.playing.lock().unwrap_or_else(|e| e.into_inner());

        // A cap of zero means "keep nothing": every stopped stream is released,
        // so the just-stopped title is not protected the way it is under a real
        // cap.
        let protect_keep = cap_mb != 0;

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
            if Some(entry.id) != playing && (!protect_keep || Some(entry.id) != keep) {
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
            // Re-read `playing`: a stream can start between building the
            // candidate list and evicting, and evicting what just started would
            // 404 the player's next range request.
            if Some(store_id) == *self.playing.lock().unwrap_or_else(|e| e.into_inner()) {
                continue;
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
    pub(crate) fn sync_entry(&self, id: usize, view: &TorrentView) {
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
    pub(crate) fn enrich_library(&self) {
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
                let in_flight = self
                    .catalog
                    .in_flight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if in_flight.contains(&key) || in_flight.len() >= MAX_CONCURRENT_LOOKUPS {
                    continue;
                }
            }
            {
                let mut attempted = self
                    .catalog
                    .attempted
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if !attempted.insert(key.clone()) {
                    continue;
                }
            }
            self.catalog
                .in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone());
            to_start.push((key, analyse_entry(entry)));
        }

        for (info_hash, release) in to_start {
            self.spawn_lookup(info_hash, release);
        }
    }

    pub(crate) fn spawn_lookup(&self, info_hash: String, release: Release) {
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

    pub(crate) fn schedule_search_retry(&self, delay: Duration) {
        let mut scheduled = self
            .catalog
            .search_retry_scheduled
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *scheduled {
            return;
        }
        *scheduled = true;
        drop(scheduled);

        let state = self.catalog.clone();
        let events = self.events.clone();
        self.runtime.spawn(async move {
            tokio::time::sleep(delay).await;
            *state
                .search_retry_scheduled
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = false;
            events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(BackendEvent::SearchLookupReady);
        });
    }

    /// Look up one search result's title in the background and remember it.
    pub(crate) fn spawn_search_lookup(&self, key: String, hit: reel_catalog::SearchHit) {
        let provider = self.catalog.provider();
        let state = self.catalog.clone();
        let events = self.events.clone();

        self.runtime.spawn(async move {
            let query = search_lookup_query(&hit);
            let metadata_updated = match provider.lookup(query).await {
                Ok(Some(metadata)) => {
                    tracing::debug!(title = %metadata.title, "matched search metadata");
                    state
                        .search_metadata
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(key, metadata);
                    true
                }
                Ok(None) => {
                    tracing::debug!(%key, "no metadata match for a search result");
                    false
                }
                Err(e) => {
                    tracing::debug!(%key, error = %e, "search metadata lookup failed");
                    false
                }
            };

            let mut in_flight = state
                .search_in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *in_flight = in_flight.saturating_sub(1);
            drop(in_flight);

            events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(if metadata_updated {
                    BackendEvent::SearchMetadata
                } else {
                    BackendEvent::SearchLookupReady
                });
        });
    }

    /// A view for a title that is not currently in the session, built from what
    /// the store remembers so the library can still draw it.
    pub(crate) fn synth_view(&self, entry: &LibraryEntry) -> TorrentView {
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

    pub(crate) fn entry_for(&self, entry: &LibraryEntry, live: Option<&TorrentView>) -> LibraryItem {
        let release = match live {
            Some(view) => {
                let signature = file_list_signature(
                    true,
                    view.files
                        .iter()
                        .map(|file| (file.id as u64, file.length, file.path.as_str())),
                );
                self.catalog
                    .cached_release(&view.info_hash, signature, || analyse_torrent(view))
            }
            None => {
                let signature = file_list_signature(
                    false,
                    entry
                        .files
                        .iter()
                        .map(|file| (file.id as u64, file.length, file.path.as_str())),
                );
                self.catalog
                    .cached_release(&entry.info_hash, signature, || analyse_entry(entry))
            }
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

        let history = self
            .catalog
            .history
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
    pub(crate) fn work_key_for(&self, entry: &LibraryEntry, release: &Release) -> String {
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
    pub(crate) fn media_key_for_release(work_key: &str, release: &Release, file_id: usize) -> String {
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

/// A cheap fingerprint of a torrent's file list, used to decide whether a
/// cached [`Release`] is still valid. `live` distinguishes the two parse paths
/// so a torrent cannot pick up the other path's result.
pub(crate) fn file_list_signature<'a>(
    live: bool,
    files: impl Iterator<Item = (u64, u64, &'a str)>,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    live.hash(&mut hasher);
    for (id, length, path) in files {
        id.hash(&mut hasher);
        length.hash(&mut hasher);
        path.hash(&mut hasher);
    }
    hasher.finish()
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

/// Build the lightweight TMDB query used for search cards. Search cards need
/// work-level metadata (poster, rating and overview), not episode stills; a
/// season or episode in the release therefore must not turn one show into many
/// separate TMDB requests.
pub(crate) fn search_lookup_query(hit: &reel_catalog::SearchHit) -> LookupQuery {
    let release = reel_catalog::release::analyse_name(&hit.title);
    let mut query = LookupQuery::from_release(&release);
    if query.title.trim().is_empty() {
        query.title = hit.title.clone();
    }
    if let Some(year) = hit.year {
        query.year = Some(year);
    }
    query.season = None;
    query.seasons.clear();
    query.air_dates.clear();
    query.episodes.clear();
    query
}

/// The metadata key for a search result is its parsed work identity, not the
/// raw release name. `The.Bear.S03E01.1080p...` and `The.Bear.S03E02.720p...`
/// should share one lookup.
pub(crate) fn search_metadata_key(hit: &reel_catalog::SearchHit) -> String {
    let query = search_lookup_query(hit);
    let title = reel_catalog::matching::normalize(&query.title);
    let kind = match query.kind {
        reel_catalog::MediaKind::Movie => "movie",
        reel_catalog::MediaKind::Series => "series",
        reel_catalog::MediaKind::Unknown => "unknown",
    };
    format!(
        "{kind}|{title}|{}",
        query.year.map_or_else(String::new, |year| year.to_string())
    )
}

pub(crate) fn claim_search_lookup_budget(budget: &mut HashSet<String>, key: &str) -> bool {
    budget.len() < MAX_SEARCH_LOOKUPS && budget.insert(key.to_string())
}

pub(crate) fn search_lookup_delay_remaining(last_started: Instant, now: Instant) -> Option<Duration> {
    let elapsed = now.saturating_duration_since(last_started);
    (elapsed < SEARCH_LOOKUP_DELAY).then(|| SEARCH_LOOKUP_DELAY - elapsed)
}

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
            // Tests can still set this directly to force an empty source set.
            disable_bundled_sources: !settings.ext_to_enabled,
            settings,
            data_dir,
        }
    }
}

/// Build the search aggregator from settings. `disable_bundled_sources` is the
/// test/CLI override that registers nothing.
pub(crate) fn build_search(settings: &CatalogSettings, disable_bundled_sources: bool) -> SearchAggregator {
    if disable_bundled_sources {
        return SearchAggregator::new(Vec::new());
    }
    SearchAggregator::new(reel_catalog::backends::bundled(settings))
}

/// Whether a settings change affects the search sources, so the aggregator must
/// be rebuilt.
pub(crate) fn search_settings_differ(a: &CatalogSettings, b: &CatalogSettings) -> bool {
    a.ext_to_enabled != b.ext_to_enabled
        || a.ext_to_base_url != b.ext_to_base_url
        || a.ext_to_cookie != b.ext_to_cookie
        || a.ext_to_user_agent != b.ext_to_user_agent
        || a.ext_to_solver_url != b.ext_to_solver_url
}

/// The handles every add needs, so `add_source` keeps a manageable signature.
#[derive(Clone)]
pub(crate) struct AddContext {
    engine: Arc<Engine>,
    events: Arc<Mutex<Vec<BackendEvent>>>,
    library: Arc<Mutex<LibraryStore>>,
    data_dir: std::path::PathBuf,
}

impl EngineBackend {
    pub(crate) fn add_context(&self) -> AddContext {
        AddContext {
            engine: self.engine.clone(),
            events: self.events.clone(),
            library: self.library.clone(),
            data_dir: self.catalog.data_dir.clone(),
        }
    }
}

pub(crate) fn report_search_action(ctx: &AddContext, key: &str, phase: Option<SearchActionPhase>) {
    let event = match phase {
        Some(phase) => BackendEvent::SearchActionProgress {
            key: key.to_string(),
            phase,
        },
        None => BackendEvent::SearchActionFinished {
            key: key.to_string(),
        },
    };
    ctx.events
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(event);
}

/// Add one source: resolve metadata, save the `.torrent`, and record it in the
/// durable library. Shared by a plain add and a resolved search hit. Returns the
/// library id (what the UI keys on) and the info hash, so a caller that wants to
/// play it can wait for its files.
async fn add_source(
    ctx: AddContext,
    source: String,
    media_only: bool,
    downloading: bool,
    upload_limit_bps: Option<u32>,
    download_limit_bps: Option<u32>,
) -> Option<(usize, String)> {
    let re_add_source = source.clone();
    let options = AddOptions {
        media_only,
        pause_multi_file: !downloading,
        ephemeral: !downloading,
        upload_limit_bps,
        download_limit_bps,
        ..Default::default()
    };
    let parsed = match AddSource::detect(&source) {
        Ok(parsed) => parsed,
        Err(e) => {
            ctx.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(BackendEvent::Error(format!("{e}")));
            return None;
        }
    };
    let outcome = match ctx.engine.add(parsed, options).await {
        Ok(outcome) => outcome,
        Err(e) => {
            ctx.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(BackendEvent::Error(format!("could not add torrent: {e}")));
            return None;
        }
    };
    let view = outcome.torrent;

    if let Some(bytes) = ctx.engine.torrent_bytes(view.id) {
        let dir = ctx.data_dir.join("torrents");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            dir.join(format!("{}.torrent", view.info_hash.to_ascii_lowercase())),
            bytes,
        );
    }

    let (id, snapshot) = {
        let mut store = ctx.library.lock().unwrap_or_else(|e| e.into_inner());
        let id = store.upsert(
            &view.info_hash,
            &re_add_source,
            view.name.clone(),
            now_unix(),
        );
        if let Some(entry) = store.entry_mut(id) {
            entry.files = view.files.iter().map(StoredFile::from_view).collect();
            entry.primary_file_id = view.primary_file_id;
            // A stream keeps nothing; a download keeps what the add selected
            // (the media files).
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
    let _ = snapshot.save(&ctx.data_dir.join("library.json"));

    let title = view.name.clone().unwrap_or_else(|| view.info_hash.clone());
    ctx.events
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(BackendEvent::Added { id, title });
    // The library id (what the UI keys on) and the info hash (to find the
    // engine view, whose id space is its own).
    Some((id, view.info_hash.clone()))
}

/// Wait for a freshly added torrent's files to resolve, then ask the UI to open
/// the player on its best file.
///
/// A magnet has no files until its metadata arrives, so this is what makes
/// "Play" on a search row work for a page-only result as well as a magnet.
async fn play_when_ready(ctx: AddContext, torrent_id: usize, info_hash: String) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        // Find the engine view by info hash: the engine's id space is its own,
        // and the id the UI uses is the library's.
        if let Some(view) = ctx
            .engine
            .list()
            .into_iter()
            .find(|view| view.info_hash.eq_ignore_ascii_case(&info_hash))
        {
            let file_id = view.primary_file_id.or_else(|| {
                view.files
                    .iter()
                    .filter(|file| file.is_video)
                    .max_by_key(|file| file.length)
                    .map(|file| file.id)
            });
            if let Some(file_id) = file_id {
                tracing::debug!(torrent_id, file_id, "search play is ready");
                ctx.events.lock().unwrap_or_else(|e| e.into_inner()).push(
                    BackendEvent::PlayReady {
                        torrent_id,
                        file_id,
                    },
                );
                return;
            }
        }
        if std::time::Instant::now() >= deadline {
            ctx.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(BackendEvent::Error(
                    "the torrent's files did not resolve in time".to_string(),
                ));
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

impl Backend for EngineBackend {
    fn capabilities(&self) -> &BackendCapabilities {
        &self.capabilities
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn cast_unavailable(&self) -> Option<String> {
        self.cast_unavailable
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn cast_devices(&self) -> Vec<crate::cast::DeviceInfo> {
        self.cast()
            .map(|cast| cast.devices())
            .unwrap_or_default()
    }

    fn cast_discovering(&self) -> bool {
        self.cast().is_some_and(|cast| cast.discovering())
    }

    fn cast_status(&self) -> Option<crate::cast::CastStatus> {
        self.cast().and_then(|cast| cast.status())
    }

    fn refresh_cast_devices(&self) {
        if let Some(cast) = self.cast() {
            cast.refresh_devices();
        }
    }

    fn add_cast_device(&self, address: String) {
        if let Some(cast) = self.cast() {
            cast.add_by_address(&address);
        }
    }

    fn start_cast(&self, udn: String, torrent_id: usize, file_id: usize, title: String) {
        if let Some(cast) = self.cast() {
            cast.start_cast(udn, torrent_id, file_id, title);
        }
    }

    fn cast_action(&self, action: crate::cast::CastAction) {
        if let Some(cast) = self.cast() {
            cast.control(action);
        }
    }

    fn clear_cast_cache(&self) {
        if let Some(cast) = self.cast() {
            if let Err(error) = cast.clear_cache() {
                tracing::warn!(%error, "could not clear the cast cache");
            }
        }
    }

    fn dismiss_cast(&self) {
        if let Some(cast) = self.cast() {
            cast.dismiss();
        }
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
        // Streaming is the primary way to use the app: a new title is temporary
        // unless the user has turned that off, in which case it is a download.
        let downloading = !self.settings().stream_only;
        let (upload_limit_bps, download_limit_bps) = self.rate_limits();
        self.runtime.spawn(add_source(
            self.add_context(),
            source.to_string(),
            media_only,
            downloading,
            upload_limit_bps,
            download_limit_bps,
        ));
    }

    fn add_search_hit(&self, hit: reel_catalog::SearchHit, media_only: bool) {
        let hit_key = hit.key();
        tracing::debug!(
            source = %hit.source,
            has_magnet = hit.magnet.is_some(),
            has_detail = hit.detail_url.is_some(),
            "adding a search result"
        );
        let downloading = !self.settings().stream_only;
        let (upload_limit_bps, download_limit_bps) = self.rate_limits();
        let ctx = self.add_context();
        let catalog = self.catalog.clone();

        self.runtime.spawn(async move {
            // A hit that already carries a magnet or .torrent URL is added
            // straight away; one that only has a page (ext.to) is resolved
            // first, which costs one extra page load.
            if let Some(source) = hit.magnet.clone().or_else(|| hit.torrent_url.clone()) {
                report_search_action(&ctx, &hit_key, Some(SearchActionPhase::Adding));
                add_source(
                    ctx.clone(),
                    source,
                    media_only,
                    downloading,
                    upload_limit_bps,
                    download_limit_bps,
                )
                .await;
                report_search_action(&ctx, &hit_key, None);
                return;
            }

            report_search_action(&ctx, &hit_key, Some(SearchActionPhase::ResolvingMagnet));
            let search = catalog.search();
            let resolved = search.magnet_for(&hit).await;
            match resolved {
                Ok(Some(source)) => {
                    report_search_action(&ctx, &hit_key, Some(SearchActionPhase::Adding));
                    add_source(
                        ctx.clone(),
                        source,
                        media_only,
                        downloading,
                        upload_limit_bps,
                        download_limit_bps,
                    )
                    .await;
                }
                Ok(None) => {
                    ctx.events
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(BackendEvent::Error(
                            "This result has nothing to add".to_string(),
                        ))
                }
                Err(e) => {
                    ctx.events
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(BackendEvent::Error(format!(
                            "could not resolve the magnet: {e}"
                        )))
                }
            }
            report_search_action(&ctx, &hit_key, None);
        });
    }

    fn play_search_hit(&self, hit: reel_catalog::SearchHit, media_only: bool) {
        let hit_key = hit.key();
        tracing::debug!(source = %hit.source, "playing a search result");
        let downloading = !self.settings().stream_only;
        let (upload_limit_bps, download_limit_bps) = self.rate_limits();
        let ctx = self.add_context();
        let catalog = self.catalog.clone();

        self.runtime.spawn(async move {
            // Resolve the magnet first when the row only carries a page.
            let source = match hit.magnet.clone().or_else(|| hit.torrent_url.clone()) {
                Some(source) => Some(source),
                None => {
                    report_search_action(&ctx, &hit_key, Some(SearchActionPhase::ResolvingMagnet));
                    match catalog.search().magnet_for(&hit).await {
                        Ok(source) => source,
                        Err(e) => {
                            ctx.events.lock().unwrap_or_else(|e| e.into_inner()).push(
                                BackendEvent::Error(format!("could not resolve the magnet: {e}")),
                            );
                            report_search_action(&ctx, &hit_key, None);
                            return;
                        }
                    }
                }
            };
            let Some(source) = source else {
                ctx.events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(BackendEvent::Error(
                        "This result has nothing to play".to_string(),
                    ));
                report_search_action(&ctx, &hit_key, None);
                return;
            };

            report_search_action(&ctx, &hit_key, Some(SearchActionPhase::Adding));
            if let Some((library_id, info_hash)) = add_source(
                ctx.clone(),
                source,
                media_only,
                downloading,
                upload_limit_bps,
                download_limit_bps,
            )
            .await
            {
                tracing::debug!(library_id, "search play: added");
                report_search_action(&ctx, &hit_key, Some(SearchActionPhase::PreparingPlayback));
                play_when_ready(ctx.clone(), library_id, info_hash).await;
            }
            report_search_action(&ctx, &hit_key, None);
        });
    }

    fn enrich_search(&self, hits: &[reel_catalog::SearchHit]) {
        if !self.catalog.provider().is_configured() {
            return;
        }
        if self
            .catalog
            .search_budget_used
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
            >= MAX_SEARCH_LOOKUPS
        {
            return;
        }
        for hit in hits {
            let key = self.catalog.search_key(hit);
            if self
                .catalog
                .search_metadata
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&key)
            {
                continue;
            }
            if self
                .catalog
                .search_attempted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&key)
            {
                continue;
            }

            {
                let budget = self
                    .catalog
                    .search_budget_used
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if budget.contains(&key) {
                    continue;
                }
                if budget.len() >= MAX_SEARCH_LOOKUPS {
                    break;
                }
            }

            // Do not flood the provider: start a few at a time, and let the
            // next refresh pick up when a slot is free.
            let in_flight = *self
                .catalog
                .search_in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if in_flight >= MAX_SEARCH_CONCURRENCY {
                break;
            }

            let now = Instant::now();
            let retry_after = self
                .catalog
                .search_lookup_started
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .and_then(|last| search_lookup_delay_remaining(last, now));
            if let Some(delay) = retry_after {
                self.schedule_search_retry(delay);
                return;
            }

            {
                let mut budget = self
                    .catalog
                    .search_budget_used
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if !claim_search_lookup_budget(&mut budget, &key) {
                    if budget.len() >= MAX_SEARCH_LOOKUPS {
                        break;
                    }
                    continue;
                }
            }
            if !self
                .catalog
                .search_attempted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone())
            {
                continue;
            }
            *self
                .catalog
                .search_in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner()) += 1;
            *self
                .catalog
                .search_lookup_started
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(now);
            self.spawn_search_lookup(key, hit.clone());
        }
    }

    fn search_metadata(&self, hit: &reel_catalog::SearchHit) -> Option<Metadata> {
        let key = self.catalog.search_key(hit);
        self.catalog
            .search_metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
    }

    fn pending_lookups(&self) -> usize {
        let library = self
            .catalog
            .in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        let search = *self
            .catalog
            .search_in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        library + search
    }

    fn set_paused(&self, id: usize, paused: bool) {
        let Some(entry) = self.snapshot().entry(id).cloned() else {
            return;
        };
        // Remember the choice: a download the user paused must not quietly
        // resume the next time the app starts.
        {
            let mut store = self.library.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = store.entry_mut(id) {
                entry.paused = paused;
            }
            let snapshot = store.clone();
            drop(store);
            let _ = snapshot.save(&self.library_path());
        }
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
                            events.lock().unwrap_or_else(|e| e.into_inner()).push(
                                BackendEvent::Error(format!("could not start the stream: {e}")),
                            );
                            return;
                        }
                    }
                    if let Err(e) = engine.set_only_files(engine_id, &selection).await {
                        tracing::warn!(error = %e, "could not set the stream selection");
                        events
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(BackendEvent::Error(format!(
                                "could not set the stream selection: {e}"
                            )));
                        return;
                    }
                    // Only now, with the torrent resumed and the selection
                    // applied, is it safe to tell the UI to open the player. A
                    // Ready on a paused or wrongly-selected torrent hands mpv a
                    // URL that answers 200 and then never produces bytes.
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
        // Asking to download is asking for it to run, so drop an earlier pause.
        self.clear_paused(&affected);
        let modes: Vec<(String, bool)> = affected
            .iter()
            .map(|hash| {
                (
                    hash.clone(),
                    snapshot
                        .by_hash(hash)
                        .is_some_and(|entry| entry.downloading),
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
                    snapshot
                        .by_hash(hash)
                        .is_some_and(|entry| entry.downloading),
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
        let file = entry
            .files
            .iter()
            .find(|file| file.path == reference.path)?;
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
        let mut history = self
            .catalog
            .history
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Err(e) =
            history.record_file(info_hash, file_id, file_name, title, position, duration)
        {
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
            let history = self
                .catalog
                .history
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Err(e) = history.save() {
                tracing::warn!(error = %e, "could not save watch history");
            }
            drop(history);
            self.save_media();
        }
    }

    fn mark_finished(&self, info_hash: &str, file_id: usize, title: Option<String>) {
        let mut history = self
            .catalog
            .history
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        let mut history = self
            .catalog
            .history
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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

    fn search(&self, query: &reel_catalog::SearchQuery) {
        if query.text.trim().is_empty() {
            return;
        }
        self.catalog
            .search_budget_used
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        // The describe string is the key the UI matches answers against, so two
        // searches for the same text with different filters are not confused.
        let key = query.describe();
        let query = query.clone();

        let state = self.catalog.clone();
        let events = self.events.clone();

        self.runtime.spawn(async move {
            let search = state.search();
            let outcome = search.search(&query).await;

            match outcome {
                Ok(results) => {
                    tracing::info!(
                        query = %key,
                        hits = results.hits.len(),
                        failures = results.failures.len(),
                        "search finished"
                    );
                    events.lock().unwrap_or_else(|e| e.into_inner()).push(
                        BackendEvent::SearchResults {
                            query: key.clone(),
                            results,
                        },
                    );
                }
                Err(e) => {
                    events.lock().unwrap_or_else(|e| e.into_inner()).push(
                        BackendEvent::SearchFailed {
                            query: key.clone(),
                            message: e.to_string(),
                        },
                    );
                    return;
                }
            }

            // Keep pulling pages and stream them in, so a source that paginates
            // fills the list instead of stopping at a page count.
            let mut streamed = 0usize;
            for round in 1..=MAX_SEARCH_ROUNDS {
                let more = search.search_more(&query, round).await;
                if more.hits.is_empty() {
                    break;
                }
                streamed += more.hits.len();
                events.lock().unwrap_or_else(|e| e.into_inner()).push(
                    BackendEvent::SearchHitsAdded {
                        query: key.clone(),
                        hits: more.hits,
                    },
                );
            }
            if streamed > 0 {
                tracing::info!(query = %key, streamed, "search streamed more results");
            }
            events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(BackendEvent::SearchStreamEnded { query: key.clone() });
        });
    }

    fn search_sources(&self) -> Vec<SourceInfo> {
        let search = self.catalog.search();
        let configured = search.configured();
        search
            .names()
            .into_iter()
            .map(|name| SourceInfo {
                configured: configured.contains(&name),
                name: name.to_string(),
            })
            .collect()
    }

    fn catalog_status(&self) -> CatalogStatus {
        // This is read on every frame the settings page is open, so it must not
        // re-read the engine or walk the cache directory each time. The library
        // count comes from the store we already hold, and the cache figures are
        // memoized for a couple of seconds.
        let enriched = {
            let store = self.library.lock().unwrap_or_else(|e| e.into_inner());
            let metadata = self
                .catalog
                .metadata
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            store
                .iter()
                .filter(|entry| metadata.contains_key(&entry.info_hash))
                .count()
        };
        let pending = {
            let in_flight = self
                .catalog
                .in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            in_flight.len()
        };

        let settings = self
            .catalog
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let source = settings.key_source();
        drop(settings);

        let (cached_metadata, cache_bytes) = self.cache_stats();

        CatalogStatus {
            provider: self.catalog.provider().name().to_string(),
            configured: self.catalog.provider().is_configured(),
            key_source: source.describe().to_string(),
            // Settings are canonical now, so the key can always be changed here
            // even when the environment supplied the current one.
            can_set_key: true,
            note: self.catalog.note(),
            cached_metadata,
            cache_bytes,
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
        let search_changed = search_settings_differ(&previous, &settings);

        {
            let mut current = self
                .catalog
                .settings
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Err(e) = settings.save(&self.catalog.data_dir) {
                self.push(BackendEvent::Error(format!("could not save settings: {e}")));
                return;
            }
            *current = settings.clone();
        }

        // Search sources take effect immediately: rebuild the aggregator so a
        // newly registered source (and its credentials) is used by the next
        // search rather than only after a restart.
        if search_changed {
            let rebuilt = build_search(&settings, false);
            *self
                .catalog
                .search
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Arc::new(rebuilt);
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

        self.reconcile_casting(&settings);

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
            self.push(BackendEvent::Error(format!(
                "clearing the catalog cache: {e}"
            )));
            return;
        }
        // The figures the settings page shows are now wrong; re-measure on the
        // next read rather than waiting out the memoization window.
        self.invalidate_cache_stats();
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

        // Stop the LAN media server too.
        if let Some(cast) = self
            .cast
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            cast.shutdown();
        }

        {
            let history = self
                .catalog
                .history
                .lock()
                .unwrap_or_else(|e| e.into_inner());
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

