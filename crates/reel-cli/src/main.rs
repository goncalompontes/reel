//! `reel` — the command line face of the engine.
//!
//! Two halves:
//!
//! * `reel serve` runs the engine plus the HTTP/streaming server in one process.
//! * `reel add|ls|play|rm` are thin clients for a running `reel serve` (or for
//!   the desktop app, which exposes the same API).

mod client;
mod format;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use reel_core::{AddOptions, AddSource, EngineConfig};

#[derive(Parser, Debug)]
#[command(
    name = "reel",
    version,
    about = "Stream torrents over HTTP, with a range-request streaming server",
    long_about = None
)]
struct Cli {
    /// Increase log verbosity (-v, -vv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the torrent engine and the HTTP/streaming server.
    Serve(ServeArgs),
    /// Add a magnet link, .torrent URL or info hash to a running server.
    Add(AddArgs),
    /// List torrents known to a running server.
    Ls(LsArgs),
    /// Print (and optionally open) a playable stream URL.
    Play(PlayArgs),
    /// Remove a torrent from a running server.
    Rm(RmArgs),
    /// Create a .torrent from a local file or folder (for seeding your own content).
    Create(CreateArgs),
}

#[derive(Args, Debug)]
struct ServeArgs {
    /// Where torrent data is stored.
    #[arg(long, env = "REEL_DOWNLOAD_DIR", value_name = "DIR")]
    dir: Option<PathBuf>,

    /// Address for the HTTP API and streaming endpoints.
    #[arg(long, default_value = "127.0.0.1:3030", value_name = "ADDR")]
    api_addr: SocketAddr,

    /// Fixed TCP+uTP port for peer connections (default: OS-assigned).
    #[arg(long, value_name = "PORT")]
    listen_port: Option<u16>,

    /// Torrents to add right after starting. Repeatable.
    #[arg(long = "add", value_name = "SOURCE")]
    adds: Vec<String>,

    /// Download every file instead of just playable ones.
    #[arg(long)]
    all_files: bool,

    /// Add the `--add` torrents paused.
    #[arg(long)]
    paused: bool,

    /// Only use IPv4 for torrent traffic.
    #[arg(long)]
    ipv4_only: bool,

    /// Do not remember torrents across restarts.
    #[arg(long)]
    no_persist: bool,

    /// Disable the DHT.
    #[arg(long)]
    no_dht: bool,

    /// Disable tracker announces.
    #[arg(long)]
    no_trackers: bool,

    /// Try to open the peer port with UPnP.
    #[arg(long)]
    upnp: bool,

    /// Connect to this peer immediately. Repeatable, e.g. --peer 10.0.0.5:51413.
    #[arg(long = "peer", value_name = "ADDR")]
    peers: Vec<SocketAddr>,

    /// Use files that already exist in the download dir. Required to seed, or
    /// to re-add a torrent when session persistence is off.
    #[arg(long)]
    overwrite: bool,

    /// Cap upload speed in bytes per second (keeps streaming from saturating
    /// your uplink).
    #[arg(long, value_name = "BPS")]
    upload_limit: Option<u32>,

    /// Cap download speed in bytes per second.
    #[arg(long, value_name = "BPS")]
    download_limit: Option<u32>,

    /// Allow browser access from this origin. Repeatable. Off by default.
    #[arg(long = "cors-origin", value_name = "ORIGIN")]
    cors_origins: Vec<String>,

    /// URL the server should advertise in stream URLs, e.g. when reached over
    /// the LAN or through a proxy. Defaults to the bound API address.
    #[arg(long, value_name = "URL")]
    public_base_url: Option<String>,
}

#[derive(Args, Debug)]
struct AddArgs {
    /// magnet: URI, http(s) .torrent URL, 40-char info hash, or a local .torrent path.
    source: String,

    /// Base URL of the running reel server.
    #[arg(long, default_value = "http://127.0.0.1:3030", env = "REEL_API")]
    api: String,

    /// Download every file instead of just playable ones.
    #[arg(long)]
    all_files: bool,

    /// Add without starting the download.
    #[arg(long)]
    paused: bool,

    /// Use files that already exist on disk (needed to seed).
    #[arg(long)]
    overwrite: bool,

    /// Connect to this peer immediately. Repeatable.
    #[arg(long = "peer", value_name = "ADDR")]
    peers: Vec<SocketAddr>,

    /// Cap upload speed in bytes per second.
    #[arg(long, value_name = "BPS")]
    upload_limit: Option<u32>,

    /// Cap download speed in bytes per second.
    #[arg(long, value_name = "BPS")]
    download_limit: Option<u32>,

    /// Print the raw JSON response.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct CreateArgs {
    /// File or directory to turn into a torrent.
    input: PathBuf,

    /// Where to write the .torrent (default: next to the input).
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Override the torrent name.
    #[arg(long)]
    name: Option<String>,

    /// Tracker URL to embed. Repeatable.
    #[arg(long = "tracker", value_name = "URL")]
    trackers: Vec<String>,

    /// Piece length in bytes (default: 2 MiB).
    #[arg(long, value_name = "BYTES")]
    piece_length: Option<u32>,
}

#[derive(Args, Debug)]
struct LsArgs {
    #[arg(long, default_value = "http://127.0.0.1:3030", env = "REEL_API")]
    api: String,

    /// Print the raw JSON response.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct PlayArgs {
    /// Torrent id (see `reel ls`).
    id: usize,

    /// File id to play. Defaults to the torrent's primary (usually largest) video.
    #[arg(long)]
    file: Option<usize>,

    #[arg(long, default_value = "http://127.0.0.1:3030", env = "REEL_API")]
    api: String,

    /// Player to launch. Pass an empty string to only print the URL.
    #[arg(long, default_value = "mpv", env = "REEL_PLAYER")]
    player: String,

    /// Only print the URL, do not launch a player.
    #[arg(long)]
    print: bool,
}

#[derive(Args, Debug)]
struct RmArgs {
    id: usize,

    #[arg(long, default_value = "http://127.0.0.1:3030", env = "REEL_API")]
    api: String,

    /// Also delete the downloaded files from disk.
    #[arg(long)]
    files: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Command::Serve(args) => runtime()?.block_on(serve(args)),
        Command::Create(args) => runtime()?.block_on(create(args)),
        Command::Add(args) => client::add(args),
        Command::Ls(args) => client::ls(args),
        Command::Play(args) => client::play(args),
        Command::Rm(args) => client::rm(args),
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")
}

fn init_tracing(verbosity: u8) {
    let level = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    // librqbit is chatty at debug level; keep it one notch quieter.
    let filter = format!("{level},librqbit=warn,librqbit_dht=warn,librqbit_utp=warn");
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_target(false)
        .try_init();
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let download_dir = args.dir.unwrap_or_else(default_download_dir);

    let mut config = EngineConfig::new(&download_dir);
    config.persist_session = !args.no_persist;
    config.listen_port = args.listen_port;
    config.ipv4_only = args.ipv4_only;
    config.disable_dht = args.no_dht;
    config.disable_trackers = args.no_trackers;
    config.enable_upnp = args.upnp;

    tracing::info!(dir = %download_dir.display(), "starting engine");
    let engine = reel_core::Engine::new(config).await?;

    let add_opts = AddOptions {
        media_only: !args.all_files,
        paused: args.paused,
        output_folder: None,
        initial_peers: args.peers.clone(),
        allow_overwrite: args.overwrite,
        upload_limit_bps: args.upload_limit,
        download_limit_bps: args.download_limit,
    };

    for source in &args.adds {
        let parsed = match AddSource::detect(source) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "could not interpret torrent source");
                continue;
            }
        };
        match engine.add(parsed, add_opts.clone()).await {
            Ok(outcome) => {
                let name = outcome
                    .torrent
                    .name
                    .clone()
                    .unwrap_or_else(|| outcome.torrent.info_hash.clone());
                tracing::info!(id = outcome.torrent.id, %name, "added torrent");
            }
            Err(e) => tracing::error!(error = %e, "failed to add torrent"),
        }
    }

    let listener = reel_http::bind(args.api_addr).await?;
    let local_addr = listener.local_addr()?;

    let base_url = args.public_base_url.unwrap_or_else(|| {
        let host = if local_addr.ip().is_unspecified() {
            "127.0.0.1".to_string()
        } else {
            local_addr.ip().to_string()
        };
        format!("http://{host}:{}", local_addr.port())
    });

    println!("reel {}", env!("CARGO_PKG_VERSION"));
    println!("  library   {}", download_dir.display());
    println!("  api       {base_url}/api/torrents");
    println!("  catalog   {base_url}/");
    println!();
    println!("  add       curl -s {base_url}/api/torrents -H 'content-type: application/json' \\");
    println!("              -d '{{\"source\":\"magnet:?xt=urn:btih:<hash>\"}}'");
    println!("  stream    {base_url}/stream/<id>/<file_id>/<name>");
    println!();
    println!("Ctrl-C to stop.");

    // Graceful shutdown flushes fast-resume state, so the next start is instant.
    let router = if args.cors_origins.is_empty() {
        reel_http::build_router(engine.clone(), base_url.clone())
    } else {
        reel_http::build_router_with_allowed_origins(
            engine.clone(),
            base_url.clone(),
            args.cors_origins.clone(),
        )
    };

    reel_http::serve_router_with_shutdown(listener, router, reel_http::shutdown_signal())
        .await
        .context("http server error")?;

    tracing::info!("flushing session state");
    engine.shutdown().await;
    Ok(())
}

async fn create(args: CreateArgs) -> anyhow::Result<()> {
    let input = args
        .input
        .canonicalize()
        .with_context(|| format!("resolving {}", args.input.display()))?;

    eprintln!("hashing {} ...", input.display());
    let created = reel_core::create_torrent_file(
        &input,
        args.name.as_deref(),
        args.trackers.clone(),
        args.piece_length,
    )
    .await?;

    let stem = input
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "torrent".to_string());

    let output = args
        .output
        .unwrap_or_else(|| input.with_file_name(format!("{stem}.torrent")));

    std::fs::write(&output, &created.bytes)
        .with_context(|| format!("writing {}", output.display()))?;

    let seed_dir = input.parent().unwrap_or(std::path::Path::new("."));

    println!("wrote {} ({} bytes)", output.display(), created.bytes.len());
    println!("  info hash  {}", created.info_hash);
    println!("  magnet     magnet:?xt=urn:btih:{}", created.info_hash);
    println!();
    println!("Seed it with:");
    println!(
        "  reel serve --dir {} --overwrite --add {}",
        seed_dir.display(),
        output.display()
    );

    Ok(())
}

fn default_download_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("HOME") {
        let candidate = PathBuf::from(dir).join("Downloads").join("reel");
        return candidate;
    }
    PathBuf::from("./reel-downloads")
}
