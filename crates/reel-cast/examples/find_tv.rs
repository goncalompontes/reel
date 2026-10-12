//! Find DLNA MediaRenderers, for diagnosing why a TV does or does not show up.
//!
//! ```text
//! cargo run -p reel-cast --example find_tv              # SSDP multicast search
//! cargo run -p reel-cast --example find_tv 192.168.1.50 # scan one host
//! ```
//!
//! The scan pass exists because some networks drop SSDP multicast between WiFi
//! clients (and some renderers ignore unicast M-SEARCH), in which case SSDP
//! finds nothing but a direct port scan still locates the renderer.

use std::time::Duration;

#[tokio::main]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let argument = std::env::args().nth(1);

    match argument {
        Some(address) => {
            println!("Probing {address} for DLNA renderers...");
            match reel_cast::probe_address(&address).await {
                Ok(devices) if devices.is_empty() => println!("  no MediaRenderer found"),
                Ok(devices) => report(&devices).await,
                Err(error) => eprintln!("  probe failed: {error}"),
            }
        }
        None => {
            println!("Discovering renderers (SSDP via rupnp, then mDNS + scan)...");
            match reel_cast::discover_all(Duration::from_secs(3)).await {
                Ok(devices) if devices.is_empty() => {
                    println!("  nothing found; try `find_tv <ip>` for a host scan")
                }
                Ok(devices) => report(&devices).await,
                Err(error) => eprintln!("  discovery failed: {error}"),
            }
        }
    }
}

async fn report(devices: &[reel_cast::CastDevice]) {
    for device in devices {
        println!(
            "  {}  [{}]  {}",
            device.name(),
            device.model().unwrap_or("unknown model"),
            device.location
        );
        match reel_cast::sink_capabilities(device).await {
            Ok(caps) => println!(
                "      capabilities: {} entries  hevc={} ac3={} eac3={} mp4={} mkv={}",
                caps.entries.len(),
                caps.supports_hevc(),
                caps.supports_ac3(),
                caps.supports_eac3(),
                caps.accepts_mime("video/mp4"),
                caps.accepts_mime("video/x-matroska"),
            ),
            Err(error) => println!("      capabilities: unavailable ({error})"),
        }
    }
}
