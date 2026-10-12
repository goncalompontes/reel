//! mDNS discovery, via the `mdns-sd` crate.
//!
//! DLNA discovery is SSDP, but SSDP is sometimes dropped between WiFi clients
//! while mDNS is not. Televisions still advertise themselves over mDNS
//! (`_airplay`, `_googlecast`, `_androidtvremote2`), so this finds their
//! addresses; the caller can then probe those hosts for a DLNA description.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};

use crate::error::{CastError, Result};

/// Service types that mark a host as a television or media receiver.
pub const TV_SERVICES: [&str; 3] = [
    "_airplay._tcp.local.",
    "_googlecast._tcp.local.",
    "_androidtvremote2._tcp.local.",
];

/// Browse for TV-ish hosts, returning their addresses.
pub async fn browse_tv_hosts(timeout: Duration) -> Result<Vec<IpAddr>> {
    let daemon = ServiceDaemon::new().map_err(|error| CastError::Discovery(error.to_string()))?;

    let mut receivers = Vec::new();
    for service in TV_SERVICES {
        match daemon.browse(service) {
            Ok(receiver) => receivers.push(receiver),
            Err(error) => tracing::debug!(%service, %error, "mDNS browse failed"),
        }
    }

    let mut hosts: Vec<IpAddr> = Vec::new();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let mut drained = false;
        for receiver in &receivers {
            while let Ok(event) = receiver.try_recv() {
                drained = true;
                if let ServiceEvent::ServiceResolved(info) = event {
                    for address in info.get_addresses() {
                        let ip = address.to_ip_addr();
                        if !ip.is_loopback() && !hosts.contains(&ip) {
                            tracing::debug!(%ip, service = %info.ty_domain, "mDNS: television host");
                            hosts.push(ip);
                        }
                    }
                }
            }
        }
        if !drained {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    let _ = daemon.shutdown();
    tracing::debug!(count = hosts.len(), "mDNS browse finished");
    Ok(hosts)
}
