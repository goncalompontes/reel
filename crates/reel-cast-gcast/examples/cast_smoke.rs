//! Smoke-test Google Cast / FCast against a real device on the LAN.
//!
//! ```text
//! cargo run -p reel-cast-gcast --example cast_smoke            # list devices
//! cargo run -p reel-cast-gcast --example cast_smoke "Living Room"
//! ```
//!
//! With a device name it connects, loads a public sample MP4, and reports the
//! receiver's state for a while. This is the diagnostic for "the UI finds the
//! device but casting does not work".

use std::time::Duration;

use reel_cast::CastTarget;
use reel_cast_gcast::Discovery;

/// A small, stable, publicly reachable MP4. (The old
/// `commondatastorage.googleapis.com/gtv-videos-bucket` samples now return 403,
/// which the receiver reports as `LOAD_FAILED`.)
const SAMPLE: &str = "https://media.w3.org/2010/05/sintel/trailer.mp4";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("debug"),
    )
    .try_init();

    let name = std::env::args().nth(1);
    let media_url = std::env::args().nth(2).unwrap_or_else(|| SAMPLE.to_string());
    let subtitle_url = std::env::args().nth(3);

    let Some(discovery) = Discovery::start() else {
        eprintln!("could not start discovery (no async runtime)");
        return Ok(());
    };

    println!("discovering for 6s...");
    tokio::time::sleep(Duration::from_secs(6)).await;
    let devices = discovery.devices();
    if devices.is_empty() {
        println!("no Cast/FCast receivers found");
        return Ok(());
    }
    for device in &devices {
        println!(
            "  {} [{}] id={} protocol={:?}",
            device.name,
            device.id,
            device.id,
            device.protocol
        );
    }

    let Some(device) = devices
        .into_iter()
        .find(|device| {
            name.as_deref()
                .is_none_or(|want| device.name.to_lowercase().contains(&want.to_lowercase()))
        })
    else {
        println!("no device matched {name:?}");
        return Ok(());
    };
    println!("\nconnecting to {} ...", device.name);

    let target = discovery.target(&device)?;
    println!("connected; has_volume={}", target.has_volume());

    let mut item = reel_cast::didl::Item::video("reel smoke test", &media_url, true, false);
    if let Some(subtitle_url) = &subtitle_url {
        item = item.with_captions(vec![reel_cast::didl::Resource::subtitle(subtitle_url)]);
        println!("subtitles: {subtitle_url}");
    }
    let metadata = item.to_xml();
    println!("set_uri {media_url} ...");
    target.set_uri(&media_url, &metadata).await?;
    println!("play (waits for the receiver, then loads) ...");
    target.play().await?;

    for step in 0..25 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let transport = target.transport_info().await?;
        let position = target.position_info().await;
        let volume = target.volume().await;
        let rel = position
            .as_ref()
            .ok()
            .and_then(|p| p.rel_time.clone())
            .unwrap_or_else(|| "?".to_string());
        let dur = position
            .as_ref()
            .ok()
            .and_then(|p| p.track_duration.clone())
            .unwrap_or_else(|| "?".to_string());
        println!(
            "  [{step:02}s] state={} pos={rel}/{dur} volume={:?}",
            transport.state,
            volume.ok()
        );
        if transport.state == "PLAYING" && step > 3 {
            break;
        }
    }

    println!("stop ...");
    target.stop().await?;
    println!("done");
    Ok(())
}
