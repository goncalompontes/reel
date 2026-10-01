//! Entry point for the native desktop app.

use std::path::PathBuf;

use clap::Parser;
use reel_catalog::{CatalogSettings, default_data_dir};
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
    /// Where torrent data is stored. Overrides the saved setting for this run.
    #[arg(long, value_name = "DIR")]
    dir: Option<PathBuf>,

    /// Run with a built-in sample library and no engine or network at all.
    #[arg(long)]
    demo: bool,

    /// Increase log verbosity (-v, -vv).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

/// A writer that appends to a shared log file.
struct FileWriter(std::sync::Arc<std::sync::Mutex<std::fs::File>>);

impl std::io::Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .flush()
    }
}

fn init_tracing(verbosity: u8) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let level = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    // librqbit is chatty below warn; keep the app's own logs readable. The
    // player and the streaming server are kept at a level that shows a stall:
    // mpv's warnings and the byte ranges the player asked for.
    let filter = format!(
        "{level},librqbit=warn,librqbit_dht=warn,librqbit_utp=warn,\
         reel_player=info,reel_http=debug"
    );
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter));

    // Log to a file as well as stderr. A desktop app is started by a launcher
    // with no terminal, so without this there is nothing to inspect after a
    // problem; the file is truncated on each start.
    let data_dir = default_data_dir();
    let _ = std::fs::create_dir_all(&data_dir);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(data_dir.join("reel.log"))
        .ok()
        .map(|file| std::sync::Arc::new(std::sync::Mutex::new(file)));

    let stderr_layer = tracing_subscriber::fmt::layer().with_target(false);
    match file {
        Some(file) => {
            let file_layer = tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_ansi(false)
                .with_writer(move || FileWriter(file.clone()));
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .with(file_layer)
                .try_init();
        }
        None => {
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .try_init();
        }
    }
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

    // Settings are canonical: the saved download directory is used unless the
    // user passes an explicit `--dir` for this run.
    let saved = CatalogSettings::load(&default_data_dir());

    let backend: Box<dyn Backend> = if args.demo {
        let library = testing::sample_library();
        tracing::info!(count = library.len(), "running with a sample library");
        Box::new(FakeBackend::new(library))
    } else {
        let dir = args
            .dir
            .or_else(|| saved.download_dir_path())
            .unwrap_or_else(default_download_dir);
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
            // Matches packaging/reel.desktop, so the taskbar groups the window
            // with the launcher and shows the right icon on Wayland.
            .with_app_id("reel")
            .with_inner_size([1360.0, 880.0])
            .with_min_inner_size([940.0, 620.0]),
        ..Default::default()
    };

    eframe::run_native(
        "reel",
        options,
        Box::new(move |cc| {
            reel_desktop::install(&cc.egui_ctx);
            Ok(Box::new(App::new(backend)))
        }),
    )
}
