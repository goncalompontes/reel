use std::path::PathBuf;

/// Configuration for an [`crate::Engine`].
///
/// Every field has a sane default except [`EngineConfig::download_dir`], which
/// is where finished (and in-progress) torrent data lands.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Where torrent data is written. Created if missing.
    pub download_dir: PathBuf,

    /// Remember torrents across restarts (fast resume + re-add on boot).
    pub persist_session: bool,

    /// Where the session state JSON lives when `persist_session` is on.
    /// Defaults to `<download_dir>/.reel-session`.
    pub session_state_dir: Option<PathBuf>,

    /// TCP/uTP port for peers. `None` lets the OS pick one.
    pub listen_port: Option<u16>,

    /// Ask the router to forward the listen port via UPnP.
    pub enable_upnp: bool,

    /// Disable the DHT entirely (trackers only). Useful on locked-down networks.
    pub disable_dht: bool,

    /// Disable tracker announces (DHT + LSD only).
    pub disable_trackers: bool,

    /// Extra trackers added to every torrent.
    pub extra_trackers: Vec<String>,

    /// Peers to connect to for every torrent added, when it does not name its
    /// own. Useful for a private swarm or another instance on the LAN.
    pub initial_peers: Vec<std::net::SocketAddr>,

    /// Hard cap on connected peers per torrent.
    pub peer_limit: Option<usize>,

    /// Restrict all networking to IPv4.
    pub ipv4_only: bool,

    /// Reported to peers and trackers as the client name.
    pub client_name: String,

    /// Refuse to upload. On by default: a client that streams content it does
    /// not own must not become a distributor. Only the CLI's seeding mode turns
    /// it off, deliberately.
    pub disable_upload: bool,

    /// How much RAM a stream-only torrent may use before spilling to scratch.
    pub stream_memory_budget: usize,

    /// Where stream-only scratch files are written. `None` uses the OS temp
    /// directory. Nothing here is ever a "download".
    pub stream_scratch_dir: Option<PathBuf>,
}

impl EngineConfig {
    pub fn new(download_dir: impl Into<PathBuf>) -> Self {
        Self {
            download_dir: download_dir.into(),
            persist_session: true,
            session_state_dir: None,
            listen_port: None,
            enable_upnp: false,
            disable_dht: false,
            disable_trackers: false,
            extra_trackers: Vec::new(),
            initial_peers: Vec::new(),
            peer_limit: None,
            ipv4_only: false,
            client_name: format!("reel/{}", env!("CARGO_PKG_VERSION")),
            disable_upload: true,
            stream_memory_budget: crate::streaming::DEFAULT_MEMORY_BUDGET,
            stream_scratch_dir: None,
        }
    }

    pub fn session_state_dir(&self) -> PathBuf {
        self.session_state_dir
            .clone()
            .unwrap_or_else(|| self.download_dir.join(".reel-session"))
    }

    /// Where stream-only scratch files go. Never inside the download folder:
    /// the point is that a stream leaves nothing behind to find later.
    pub fn stream_scratch_dir(&self) -> PathBuf {
        self.stream_scratch_dir.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("reel-stream-{}", std::process::id()))
        })
    }
}
