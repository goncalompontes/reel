//! Entry point for the native desktop app.

use std::path::PathBuf;

use clap::Parser;
use reel_core::EngineConfig;
use reel_desktop::backend::{Backend, EngineBackend, FakeBackend};
use reel_desktop::{App, testing};

#[derive(Parser, Debug)]
#[command(
    name = "reel-desktop",
    version,
    about = "Native reel desktop app: catalogue, library and player"
)]
struct Args {
    /// Where torrent data is stored.
    #[arg(long, env = "REEL_DOWNLOAD_DIR", value_name = "DIR")]
    dir: Option<PathBuf>,

    /// Run with a built-in sample library and no engine or network at all.
    #[arg(long)]
    demo: bool,

    /// Increase log verbosity (-v, -vv).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn init_tracing(verbosity: u8) {
    let level = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    // librqbit is chatty below warn; keep the app's own logs readable.
    let filter = format!(
        "{level},librqbit=warn,librqbit_dht=warn,librqbit_utp=warn,reel_player=warn"
    );
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_target(false)
        .try_init();
}

fn default_download_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join("Downloads").join("reel");
    }
    PathBuf::from("./reel-downloads")
}

fn main() -> eframe::Result<()> {
    let args = Args::parse();
    init_tracing(args.verbose);

    let backend: Box<dyn Backend> = if args.demo {
        let torrents = testing::sample_torrents();
        tracing::info!(count = torrents.len(), "running with a sample library");
        Box::new(FakeBackend::new(torrents))
    } else {
        let dir = args.dir.unwrap_or_else(default_download_dir);
        tracing::info!(dir = %dir.display(), "starting engine");
        match EngineBackend::start(EngineConfig::new(&dir)) {
            Ok(backend) => Box::new(backend),
            Err(e) => {
                eprintln!("reel: could not start the torrent engine: {e:#}");
                eprintln!("hint: `reel-desktop --demo` runs without an engine.");
                std::process::exit(1);
            }
        }
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("reel")
            .with_inner_size([1360.0, 880.0])
            .with_min_inner_size([940.0, 620.0]),
        ..Default::default()
    };

    eframe::run_native(
        "reel",
        options,
        Box::new(move |cc| {
            reel_desktop::apply_theme(&cc.egui_ctx);
            Ok(Box::new(App::new(backend)))
        }),
    )
}
