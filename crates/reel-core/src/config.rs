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

    /// Hard cap on connected peers per torrent.
    pub peer_limit: Option<usize>,

    /// Restrict all networking to IPv4.
    pub ipv4_only: bool,

    /// Reported to peers and trackers as the client name.
    pub client_name: String,
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
            peer_limit: None,
            ipv4_only: false,
            client_name: format!("reel/{}", env!("CARGO_PKG_VERSION")),
        }
    }

    pub fn session_state_dir(&self) -> PathBuf {
        self.session_state_dir
            .clone()
            .unwrap_or_else(|| self.download_dir.join(".reel-session"))
    }
}
