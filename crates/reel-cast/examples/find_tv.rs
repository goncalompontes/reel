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

use std::net::IpAddr;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let argument = std::env::args().nth(1);

    match argument {
        Some(address) => {
            let ip: IpAddr = match address.parse() {
                Ok(ip) => ip,
                Err(_) => {
                    eprintln!("`{address}` is not an IP address");
                    std::process::exit(2);
                }
            };
            println!("Scanning {ip} for DLNA renderers (ports 1024-65535)...");
            match reel_cast::probe_host(ip, 1024, 65535, Duration::from_millis(120)).await {
                Ok(devices) if devices.is_empty() => println!("  no MediaRenderer found"),
                Ok(devices) => report(&devices),
                Err(error) => eprintln!("  scan failed: {error}"),
            }
        }
        None => {
            println!("Searching for renderers over SSDP (multicast)...");
            match reel_cast::discover_devices(Duration::from_secs(3)).await {
                Ok(devices) if devices.is_empty() => {
                    println!("  nothing found; try `find_tv <ip>` for a host scan")
                }
                Ok(devices) => report(&devices),
                Err(error) => eprintln!("  search failed: {error}"),
            }
        }
    }
}

fn report(devices: &[reel_cast::CastDevice]) {
    for device in devices {
        println!(
            "  {}  [{}]  {}",
            device.name(),
            device.model().unwrap_or("unknown model"),
            device.location
        );
    }
}
