//! The torrent engine: a thin, opinionated wrapper around a `librqbit` session.
//!
//! Responsibilities:
//!
//! * own the session (DHT, listeners, trackers, storage),
//! * translate engine state into the crate's own [`TorrentView`] model,
//! * expose on-demand file access ([`Engine::stream`]) for the HTTP layer,
//! * make the common case ("just give me the video") work out of the box via
//!   [`AddOptions::media_only`].

use std::borrow::Cow;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;

use anyhow::Context;
use bytes::Bytes;
use librqbit::api::{Api, TorrentDetailsResponse, TorrentIdOrHash};
use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ListenerMode, ListenerOptions, Session,
    SessionOptions, SessionPersistenceConfig, TorrentStats, TorrentStatsState,
};
use tokio::io::{AsyncRead, AsyncSeekExt};
use tracing::{info, warn};

use crate::config::EngineConfig;
use crate::media::{
    is_audio_file, is_subtitle_file, is_video_file, mime_for_name, pick_primary_file, FileCandidate,
};
use crate::model::{
    percent_encode_path_segment, AddOutcome, FileProbe, FileView, PeerView, StatsView,
    StreamTarget, TorrentView,
};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("torrent {0} is not in the session")]
    TorrentNotFound(usize),
    #[error("torrent {torrent_id} does not contain a file with id {file_id}")]
    FileNotFound { torrent_id: usize, file_id: usize },
    #[error("torrent {0} has no metadata yet; try again once it has resolved")]
    NoMetadata(usize),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl EngineError {
    /// Map onto an HTTP status code. Lives here so the HTTP layer stays dumb.
    pub fn status_code(&self) -> u16 {
        match self {
            EngineError::TorrentNotFound(_) | EngineError::FileNotFound { .. } => 404,
            EngineError::NoMetadata(_) => 409,
            EngineError::Other(_) => 500,
        }
    }
}

/// Where a torrent comes from.
#[derive(Debug, Clone)]
pub enum AddSource {
    /// `magnet:` URI or a bare 40-char hex info hash, or an HTTP(S) `.torrent` URL.
    Url(String),
    /// Raw `.torrent` file bytes.
    File(Vec<u8>),
}

impl AddSource {
    /// Classify a user-supplied string as a URL-ish source.
    pub fn parse(source: impl Into<String>) -> Self {
        AddSource::Url(source.into())
    }

    /// Classify a string the way a CLI user means it: a `magnet:` URI, an
    /// http(s) `.torrent` URL and a bare info hash stay as-is; anything that is
    /// an existing local file is read into memory as a `.torrent`.
    pub fn detect(source: &str) -> anyhow::Result<Self> {
        let source = source.trim();

        if source.starts_with("magnet:")
            || source.starts_with("http://")
            || source.starts_with("https://")
            || (source.len() == 40 && source.chars().all(|c| c.is_ascii_hexdigit()))
        {
            return Ok(AddSource::Url(source.to_string()));
        }

        let path = std::path::Path::new(source);
        if path.is_file() {
            let bytes =
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            return Ok(AddSource::File(bytes));
        }

        anyhow::bail!(
            "`{source}` is not a magnet link, http(s) URL, info hash, or an existing .torrent file"
        )
    }
}

/// How to add a torrent.
#[derive(Debug, Clone)]
pub struct AddOptions {
    /// Select only playable files. Recommended: torrents are usually padded
    /// with samples, `.nfo` files and cover art.
    pub media_only: bool,
    /// Add without starting the download.
    pub paused: bool,
    /// Per-torrent output folder override.
    pub output_folder: Option<String>,
    /// Peers to connect to immediately, bypassing trackers/DHT. Essential for
    /// private swarms and for talking to another instance on the LAN.
    pub initial_peers: Vec<std::net::SocketAddr>,
    /// Allow the engine to use (and re-check) files that already exist on disk.
    ///
    /// Needed when seeding content you already have, or when re-adding a
    /// torrent after a restart without fast-resume state.
    pub allow_overwrite: bool,
    /// Cap upload for this torrent, in bytes per second.
    pub upload_limit_bps: Option<u32>,
    /// Cap download for this torrent, in bytes per second.
    pub download_limit_bps: Option<u32>,
}

impl Default for AddOptions {
    fn default() -> Self {
        Self {
            media_only: true,
            paused: false,
            output_folder: None,
            initial_peers: Vec::new(),
            allow_overwrite: false,
            upload_limit_bps: None,
            download_limit_bps: None,
        }
    }
}

/// A sequential byte stream over one file in a torrent.
///
/// The engine owns positioning: callers ask for a stream *starting at* an
/// offset, which is exactly what an HTTP range request needs. That keeps
/// `librqbit`'s concrete (and module-private) stream type out of this API.
pub type BoxedByteStream = Box<dyn AsyncRead + Send + Unpin>;

/// The engine. Cheap to clone (it is an `Arc` inside).
pub struct Engine {
    session: Arc<Session>,
    api: Api,
    config: EngineConfig,
}

impl Engine {
    /// Create a session and start networking.
    pub async fn new(config: EngineConfig) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(&config.download_dir).with_context(|| {
            format!("creating download dir {}", config.download_dir.display())
        })?;

        let mut opts = SessionOptions::default();

        if config.disable_dht {
            opts.dht = None;
        }
        opts.disable_trackers = config.disable_trackers;
        opts.peer_limit = config.peer_limit;
        opts.ipv4_only = config.ipv4_only;
        opts.client_name_and_version = Some(config.client_name.clone());
        opts.trackers = config
            .extra_trackers
            .iter()
            .filter_map(|t| url::Url::parse(t).ok())
            .collect();

        if config.persist_session {
            opts.persistence = Some(SessionPersistenceConfig::Json {
                folder: Some(config.session_state_dir()),
            });
        }

        let port = config.listen_port.unwrap_or(0);
        let listen_addr = if config.ipv4_only {
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))
        } else {
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))
        };
        opts.listen = Some(ListenerOptions {
            mode: ListenerMode::TcpAndUtp,
            listen_addr,
            enable_upnp_port_forwarding: config.enable_upnp,
            announce_port: config.listen_port,
            ipv4_only: config.ipv4_only,
            ..Default::default()
        });

        let session = Session::new_with_opts(config.download_dir.clone(), opts)
            .await
            .context("starting torrent session")?;

        if let Some(addr) = session.listen_addr() {
            info!(%addr, "torrent peer listener ready");
        }

        let api = Api::new(session.clone(), None);

        Ok(Arc::new(Self {
            session,
            api,
            config,
        }))
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    fn add_torrent_borrowed<'a>(&self, source: &'a AddSource) -> AddTorrent<'a> {
        match source {
            AddSource::Url(s) => AddTorrent::Url(Cow::Borrowed(s.as_str())),
            AddSource::File(bytes) => AddTorrent::TorrentFileBytes(Bytes::copy_from_slice(bytes)),
        }
    }

    fn build_add_opts(&self, opts: &AddOptions, media_only: bool) -> AddTorrentOptions {
        AddTorrentOptions {
            paused: opts.paused,
            output_folder: opts.output_folder.clone(),
            only_files_regex: media_only.then(crate::media::media_only_regex),
            overwrite: opts.allow_overwrite,
            initial_peers: if opts.initial_peers.is_empty() {
                None
            } else {
                Some(opts.initial_peers.clone())
            },
            ratelimits: LimitsConfig {
                upload_bps: opts.upload_limit_bps.and_then(NonZeroU32::new),
                download_bps: opts.download_limit_bps.and_then(NonZeroU32::new),
            },
            ..Default::default()
        }
    }

    /// Add a torrent and return its view.
    ///
    /// With [`AddOptions::media_only`] the engine asks the session to only
    /// download playable files. If that filter matches nothing (rare, but
    /// happens with e.g. disc images), it transparently retries without it.
    pub async fn add(&self, source: AddSource, opts: AddOptions) -> Result<AddOutcome, EngineError> {
        let add_opts = self.build_add_opts(&opts, opts.media_only);

        let response = match self
            .session
            .add_torrent(self.add_torrent_borrowed(&source), Some(add_opts))
            .await
        {
            Ok(r) => r,
            Err(err) if opts.media_only => {
                warn!(error = %err, "media-only add failed, retrying with all files");
                self.session
                    .add_torrent(
                        self.add_torrent_borrowed(&source),
                        Some(self.build_add_opts(&opts, false)),
                    )
                    .await?
            }
            Err(err) => return Err(err.into()),
        };

        let (id, was_new) = match response {
            AddTorrentResponse::Added(id, _) => (id, true),
            AddTorrentResponse::AlreadyManaged(id, _) => (id, false),
            AddTorrentResponse::ListOnly(_) => {
                return Err(anyhow::anyhow!(
                    "torrent was added in list-only mode, which this engine does not use"
                )
                .into());
            }
        };

        Ok(AddOutcome {
            torrent: self.view(id)?,
            was_new,
        })
    }

    /// Ids of every torrent in the session, ascending.
    pub fn ids(&self) -> Vec<usize> {
        let mut ids: Vec<usize> = self
            .api
            .api_torrent_list()
            .torrents
            .into_iter()
            .filter_map(|t| t.id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// All torrents in the session, with files and stats.
    pub fn list(&self) -> Vec<TorrentView> {
        self.ids()
            .into_iter()
            .filter_map(|id| self.view(id).ok())
            .collect()
    }

    /// One torrent by id.
    pub fn view(&self, id: usize) -> Result<TorrentView, EngineError> {
        let details = self.details(id)?;
        // Stats can be unavailable while a torrent is still initializing; never
        // report an empty state string.
        let stats = self.stats(id).unwrap_or_else(|| StatsView {
            state: "unknown".to_string(),
            ..Default::default()
        });
        Ok(self.assemble(id, details, stats))
    }

    /// Liveness stats for one torrent.
    pub fn stats(&self, id: usize) -> Option<StatsView> {
        let stats = self.api.api_stats_v1(TorrentIdOrHash::Id(id)).ok()?;
        Some(stats_to_view(&stats))
    }

    fn details(&self, id: usize) -> Result<TorrentDetailsResponse, EngineError> {
        let idx = TorrentIdOrHash::Id(id);
        if self.session.get(idx).is_none() {
            return Err(EngineError::TorrentNotFound(id));
        }
        self.api
            .api_torrent_details(idx)
            .map_err(|e| EngineError::Other(anyhow::anyhow!("{e}")))
    }

    /// Pause a torrent (it stays in the session and resumes instantly).
    pub async fn pause(&self, id: usize) -> Result<(), EngineError> {
        let handle = self
            .session
            .get(TorrentIdOrHash::Id(id))
            .ok_or(EngineError::TorrentNotFound(id))?;
        self.session.pause(&handle).await?;
        Ok(())
    }

    /// Resume a paused torrent.
    pub async fn resume(&self, id: usize) -> Result<(), EngineError> {
        let handle = self
            .session
            .get(TorrentIdOrHash::Id(id))
            .ok_or(EngineError::TorrentNotFound(id))?;
        self.session.unpause(&handle).await?;
        Ok(())
    }

    /// Forget a torrent, optionally deleting its files from disk.
    pub async fn remove(&self, id: usize, delete_files: bool) -> Result<(), EngineError> {
        if self.session.get(TorrentIdOrHash::Id(id)).is_none() {
            return Err(EngineError::TorrentNotFound(id));
        }
        self.session
            .delete(TorrentIdOrHash::Id(id), delete_files)
            .await?;
        Ok(())
    }

    /// Change which files are downloaded for a torrent.
    pub async fn set_only_files(
        &self,
        id: usize,
        only_files: &[usize],
    ) -> Result<(), EngineError> {
        let handle = self
            .session
            .get(TorrentIdOrHash::Id(id))
            .ok_or(EngineError::TorrentNotFound(id))?;
        let set = only_files.iter().copied().collect();
        self.session.update_only_files(&handle, &set).await?;
        Ok(())
    }

    /// Open a sequential, on-demand read stream over one file.
    ///
    /// The returned stream implements `AsyncRead + AsyncSeek`; the HTTP layer
    /// seeks it to serve `Range` requests. Opening a stream also tells the
    /// engine to prioritise the pieces covering that file, which is what makes
    /// "start watching immediately" work.
    /// Open a sequential, on-demand read stream over one file, starting at a
    /// byte offset.
    ///
    /// Opening a stream also tells the engine to prioritise the pieces covering
    /// that file, which is what makes "start watching immediately" work.
    pub async fn stream_from(
        &self,
        id: usize,
        file_id: usize,
        offset: u64,
    ) -> Result<BoxedByteStream, EngineError> {
        let handle = self
            .session
            .get(TorrentIdOrHash::Id(id))
            .ok_or(EngineError::TorrentNotFound(id))?;

        match handle.with_metadata(|m| m.file_infos.len() > file_id) {
            Err(_) => return Err(EngineError::NoMetadata(id)),
            Ok(true) => {}
            Ok(false) => {
                return Err(EngineError::FileNotFound {
                    torrent_id: id,
                    file_id,
                });
            }
        }

        let mut stream = self
            .api
            .api_stream(TorrentIdOrHash::Id(id), file_id)
            .await
            .map_err(|e| EngineError::Other(anyhow::anyhow!("{e}")))?;

        if offset > 0 {
            stream
                .seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| EngineError::Other(anyhow::anyhow!("seeking stream: {e}")))?;
        }

        Ok(Box::new(stream))
    }

    /// [`Engine::stream_from`] from the very beginning of the file.
    pub async fn stream(&self, id: usize, file_id: usize) -> Result<BoxedByteStream, EngineError> {
        self.stream_from(id, file_id, 0).await
    }

    /// Lightweight per-file lookup, used by the HTTP layer on every range
    /// request so it does not have to build a whole [`TorrentView`].
    pub fn probe_file(&self, id: usize, file_id: usize) -> Result<FileProbe, EngineError> {
        let handle = self
            .session
            .get(TorrentIdOrHash::Id(id))
            .ok_or(EngineError::TorrentNotFound(id))?;

        let probe = handle
            .with_metadata(|m| {
                m.file_infos.get(file_id).map(|f| {
                    let name = f
                        .relative_filename
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| format!("file-{file_id}"));
                    FileProbe {
                        id: file_id,
                        path: f.relative_filename.to_string_lossy().replace('\\', "/"),
                        mime: mime_for_name(&name).to_string(),
                        name,
                        length: f.len,
                    }
                })
            })
            .map_err(|_| EngineError::NoMetadata(id))?;

        probe.ok_or(EngineError::FileNotFound {
            torrent_id: id,
            file_id,
        })
    }

    /// Stop the session. Call this on shutdown so fast-resume state is flushed.
    pub async fn shutdown(&self) {
        self.session.stop().await;
    }

    // ---------------------------------------------------------------- mapping

    fn assemble(&self, id: usize, details: TorrentDetailsResponse, stats: StatsView) -> TorrentView {
        let files_meta = details.files.unwrap_or_default();

        let candidates: Vec<FileCandidate> = files_meta
            .iter()
            .enumerate()
            .map(|(i, f)| FileCandidate {
                id: i,
                name: f.name.clone(),
                length: f.length,
            })
            .collect();
        let primary_file_id = pick_primary_file(&candidates);

        let files: Vec<FileView> = files_meta
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let name = f
                    .components
                    .last()
                    .cloned()
                    .unwrap_or_else(|| f.name.clone());
                let path = if f.components.is_empty() {
                    f.name.clone()
                } else {
                    f.components.join("/")
                };
                FileView {
                    id: i,
                    path,
                    name: name.clone(),
                    length: f.length,
                    included: f.included,
                    is_video: is_video_file(&name),
                    is_audio: is_audio_file(&name),
                    is_subtitle: is_subtitle_file(&name),
                    stream: StreamTarget {
                        torrent_id: id,
                        file_id: i,
                        mime: mime_for_name(&name).to_string(),
                        length: f.length,
                        path: format!(
                            "/stream/{id}/{i}/{}",
                            percent_encode_path_segment(&name)
                        ),
                        url: None,
                        name,
                    },
                }
            })
            .collect();

        TorrentView {
            id,
            info_hash: details.info_hash,
            name: details.name,
            output_folder: details.output_folder,
            state: stats.state.clone(),
            finished: stats.finished,
            primary_file_id,
            files,
            stats,
        }
    }
}

fn stats_to_view(stats: &TorrentStats) -> StatsView {
    let state = match &stats.state {
        TorrentStatsState::Initializing { paused } => {
            if *paused {
                "paused"
            } else {
                "initializing"
            }
        }
        TorrentStatsState::Live => "live",
        TorrentStatsState::Paused => "paused",
        TorrentStatsState::Error => "error",
    };

    let (download_bps, upload_bps, peers) = match stats.live.as_ref() {
        Some(live) => {
            let p = &live.snapshot.peer_stats;
            (
                live.download_speed.as_bytes(),
                live.upload_speed.as_bytes(),
                PeerView {
                    live: p.live,
                    connecting: p.connecting,
                    queued: p.queued,
                    seen: p.seen,
                    dead: p.dead,
                },
            )
        }
        None => (0, 0, PeerView::default()),
    };

    let percent = if stats.total_bytes == 0 {
        0.0
    } else {
        (stats.progress_bytes as f64 / stats.total_bytes as f64) * 100.0
    };

    let eta_seconds = if download_bps > 0 && stats.total_bytes > stats.progress_bytes {
        Some((stats.total_bytes - stats.progress_bytes) / download_bps)
    } else {
        None
    };

    StatsView {
        state: state.to_string(),
        error: stats.error.clone(),
        total_bytes: stats.total_bytes,
        progress_bytes: stats.progress_bytes,
        uploaded_bytes: stats.uploaded_bytes,
        finished: stats.finished,
        percent,
        download_bps,
        upload_bps,
        eta_seconds,
        peers,
    }
}
