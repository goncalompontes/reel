//! The seam between the UI and the torrent engine.
//!
//! The UI only ever talks to [`Backend`], which keeps two useful properties:
//!
//! * the whole interface can be driven in tests by [`FakeBackend`], with no
//!   engine, no network and no HTTP server;
//! * the engine stays off the UI thread for anything slow, because the async
//!   methods are spawned onto a runtime and report back through events.
//!
//! The real implementation runs the engine and the HTTP API *in-process* and
//! hands the player URLs on `127.0.0.1`, so playback uses the same
//! range-request path the CLI and the integration tests exercise.

use std::sync::{Arc, Mutex};

use reel_core::model::TorrentView;
use reel_core::{AddOptions, AddSource, Engine, EngineConfig};

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

/// Something the backend wants the UI to know about.
#[derive(Debug, Clone)]
pub enum BackendEvent {
    Info(String),
    Error(String),
    Added { id: usize, title: String },
}

pub trait Backend {
    fn capabilities(&self) -> &BackendCapabilities;
    fn base_url(&self) -> &str;

    /// All torrents, with stream URLs already made absolute.
    fn torrents(&self) -> Vec<TorrentView>;
    fn view(&self, id: usize) -> Option<TorrentView>;

    fn add(&self, source: &str, media_only: bool);
    fn set_paused(&self, id: usize, paused: bool);
    fn remove(&self, id: usize, delete_files: bool);
    /// Download only this file of a torrent.
    fn set_only_file(&self, id: usize, file_id: usize);

    fn take_events(&self) -> Vec<BackendEvent>;
}

// ------------------------------------------------------------ real backend

pub struct EngineBackend {
    runtime: tokio::runtime::Runtime,
    engine: Arc<Engine>,
    base_url: String,
    events: Arc<Mutex<Vec<BackendEvent>>>,
    capabilities: BackendCapabilities,
    shutdown: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl EngineBackend {
    /// Start the engine and serve the streaming API on an ephemeral localhost
    /// port. The port is chosen by the OS so several instances never clash.
    pub fn start(config: EngineConfig) -> anyhow::Result<Self> {
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

        let capabilities = BackendCapabilities {
            download_dir,
            client_name,
            player,
            mpv_api: reel_player::mpv_api_version(),
            player_note,
        };

        Ok(Self {
            runtime,
            engine,
            base_url,
            events: Arc::new(Mutex::new(Vec::new())),
            capabilities,
            shutdown: Mutex::new(Some(shutdown_tx)),
        })
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
}

impl Backend for EngineBackend {
    fn capabilities(&self) -> &BackendCapabilities {
        &self.capabilities
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn torrents(&self) -> Vec<TorrentView> {
        let mut views = self.engine.list();
        for view in &mut views {
            view.with_base_url(&self.base_url);
        }
        views
    }

    fn view(&self, id: usize) -> Option<TorrentView> {
        let mut view = self.engine.view(id).ok()?;
        view.with_base_url(&self.base_url);
        Some(view)
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
        self.spawn_result(
            if paused { "pause" } else { "resume" },
            async move {
                if paused {
                    engine.pause(id).await
                } else {
                    engine.resume(id).await
                }
            },
        );
    }

    fn remove(&self, id: usize, delete_files: bool) {
        let engine = self.engine.clone();
        self.spawn_result("remove", async move { engine.remove(id, delete_files).await });
    }

    fn set_only_file(&self, id: usize, file_id: usize) {
        let engine = self.engine.clone();
        self.spawn_result("select file", async move {
            engine.set_only_files(id, &[file_id]).await
        });
    }

    fn take_events(&self) -> Vec<BackendEvent> {
        std::mem::take(&mut *self.events.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Drop for EngineBackend {
    fn drop(&mut self) {
        // Stop the HTTP server, then flush fast-resume state so the next start
        // is instant.
        if let Some(shutdown) = self.shutdown.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = shutdown.send(());
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
    torrents: Mutex<Vec<TorrentView>>,
    events: Mutex<Vec<BackendEvent>>,
    capabilities: BackendCapabilities,
    base_url: String,
}

impl FakeBackend {
    pub fn new(torrents: Vec<TorrentView>) -> Self {
        Self {
            torrents: Mutex::new(torrents),
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
        }
    }
}

impl Backend for FakeBackend {
    fn capabilities(&self) -> &BackendCapabilities {
        &self.capabilities
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn torrents(&self) -> Vec<TorrentView> {
        self.torrents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn view(&self, id: usize) -> Option<TorrentView> {
        self.torrents()
            .into_iter()
            .find(|view| view.id == id)
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
        let mut torrents = self.torrents.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(view) = torrents.iter_mut().find(|v| v.id == id) {
            view.stats.state = if paused { "paused" } else { "live" }.to_string();
            view.state = if paused { "paused" } else { "live" }.to_string();
        }
    }

    fn remove(&self, id: usize, _delete_files: bool) {
        let mut torrents = self.torrents.lock().unwrap_or_else(|e| e.into_inner());
        torrents.retain(|view| view.id != id);
    }

    fn set_only_file(&self, id: usize, file_id: usize) {
        let mut torrents = self.torrents.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(view) = torrents.iter_mut().find(|v| v.id == id) {
            for file in &mut view.files {
                file.included = file.id == file_id;
            }
        }
    }

    fn take_events(&self) -> Vec<BackendEvent> {
        std::mem::take(&mut *self.events.lock().unwrap_or_else(|e| e.into_inner()))
    }
}
