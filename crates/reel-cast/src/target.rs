//! Cast targets: describe a renderer and drive it over HTTP.
//!
//! [`CastDevice`] is a discovered renderer (its `LOCATION` plus the parsed
//! description). [`DlnaTarget`] implements the [`CastTarget`] trait by POSTing
//! the SOAP documents from [`crate::soap`] to the device's `AVTransport` and
//! `RenderingControl` control URLs.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use url::Url;

use crate::device::{self, DeviceDescription, Service};
use crate::error::{CastError, Result};
use crate::soap::{self, MediaInfo, PositionInfo, TransportInfo};

/// A renderer that can be cast to.
#[derive(Debug, Clone)]
pub struct CastDevice {
    /// The `LOCATION` URL the description was fetched from.
    pub location: Url,
    /// The parsed description.
    pub description: DeviceDescription,
}

impl CastDevice {
    /// Friendly name, for the device picker.
    pub fn name(&self) -> &str {
        &self.description.friendly_name
    }

    /// Stable device identifier.
    pub fn udn(&self) -> &str {
        &self.description.udn
    }

    /// Model name, when advertised.
    pub fn model(&self) -> Option<&str> {
        self.description.model_name.as_deref()
    }

    /// The playback-control service, if the device exposes one.
    pub fn av_transport(&self) -> Option<&Service> {
        self.description.av_transport()
    }

    /// Build a target for this device, using a fresh HTTP client.
    pub fn target(&self) -> Result<DlnaTarget> {
        DlnaTarget::new(self)
    }
}

/// Fetch and parse a device description.
pub async fn describe(client: &reqwest::Client, location: &Url) -> Result<CastDevice> {
    let body = client
        .get(location.clone())
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let description = device::parse_description(&body, location)?;
    Ok(CastDevice {
        location: location.clone(),
        description,
    })
}

/// Discover media renderers on the LAN and fetch each description.
///
/// SSDP itself is delegated to `rupnp`, which joins the multicast group on each
/// interface (something a hand-rolled search tends to get wrong). Devices
/// without an `AVTransport` service are skipped (they cannot be cast to), and
/// duplicates are collapsed by `UDN`.
pub async fn discover_devices(timeout: Duration) -> Result<Vec<CastDevice>> {
    use futures_util::TryStreamExt;
    use rupnp::ssdp::SearchTarget;

    let stream = rupnp::discover(&SearchTarget::RootDevice, timeout, None)
        .await
        .map_err(|error| CastError::Discovery(error.to_string()))?;
    let mut stream = std::pin::pin!(stream);

    let client = reqwest::Client::new();
    let mut devices: Vec<CastDevice> = Vec::new();
    while let Some(device) = stream
        .try_next()
        .await
        .map_err(|error| CastError::Discovery(error.to_string()))?
    {
        let Ok(location) = Url::parse(&device.url().to_string()) else {
            continue;
        };
        match describe(&client, &location).await {
            Ok(cast) if cast.av_transport().is_some() => {
                if !devices.iter().any(|seen| seen.udn() == cast.udn()) {
                    tracing::debug!(name = cast.name(), "described renderer");
                    devices.push(cast);
                }
            }
            Ok(_) => tracing::debug!(%location, "not an AVTransport renderer"),
            Err(error) => {
                tracing::debug!(%location, %error, "could not describe renderer")
            }
        }
    }
    Ok(devices)
}

/// Probe several hosts for DLNA renderers at once.
pub async fn probe_hosts(
    hosts: &[IpAddr],
    first_port: u16,
    last_port: u16,
    connect_timeout: Duration,
) -> Vec<CastDevice> {
    // Scan one host at a time: probing several at once multiplies the concurrent
    // connections, drops SYNs on WiFi and makes the scan miss real ports.
    let mut devices: Vec<CastDevice> = Vec::new();
    for ip in hosts {
        let Ok(found) = probe_host(*ip, first_port, last_port, connect_timeout).await else {
            continue;
        };
        for device in found {
            if !devices.iter().any(|seen| seen.udn() == device.udn()) {
                devices.push(device);
            }
        }
    }
    devices
}

/// Discover renderers, falling back to mDNS + a host scan when SSDP is silent.
///
/// SSDP is the normal path. When it returns nothing — some networks drop its
/// multicast between clients while mDNS still works — the hosts advertising
/// television services are found over mDNS and probed directly.
pub async fn discover_all(timeout: Duration) -> Result<Vec<CastDevice>> {
    let devices = discover_devices(timeout).await.unwrap_or_default();
    if !devices.is_empty() {
        return Ok(devices);
    }

    let mut hosts = crate::mdns::browse_tv_hosts(timeout)
        .await
        .unwrap_or_default();
    // IPv6 link-local addresses from mDNS are not reachable without a scope id.
    hosts.retain(IpAddr::is_ipv4);
    if hosts.is_empty() {
        return Ok(devices);
    }
    tracing::info!(
        count = hosts.len(),
        "SSDP found nothing; probing mDNS hosts"
    );

    // A small targeted probe first: it is quick and does not make the device
    // drop SYNs, which a full 64k-port scan on WiFi does.
    let curated = curated_ports();
    let mut found: Vec<CastDevice> = Vec::new();
    for ip in &hosts {
        if let Ok(devices) = probe_ports(*ip, &curated, DEFAULT_PROBE_TIMEOUT).await {
            for device in devices {
                if !found.iter().any(|seen| seen.udn() == device.udn()) {
                    found.push(device);
                }
            }
        }
    }

    // Only if that finds nothing, fall back to the whole port range.
    if found.is_empty() {
        tracing::info!("curated ports found nothing; scanning the full range");
        found = probe_hosts(&hosts, 1024, 65535, DEFAULT_PROBE_TIMEOUT).await;
    }
    Ok(found)
}

/// The default connect timeout for a port scan.
///
/// Long enough for one TCP SYN retransmit: on WiFi a dropped SYN is common, and
/// a short timeout would report a listening port as closed.
const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Resolve a user-entered address to renderers.
///
/// Accepts an IP (scanning its port range), `ip:port` (scanning just that
/// port), or a full description URL (fetched directly). This is the escape
/// hatch for networks that drop SSDP multicast between clients.
pub async fn probe_address(address: &str) -> Result<Vec<CastDevice>> {
    let address = address.trim();

    if let Ok(ip) = address.parse::<IpAddr>() {
        // Curated ports first (fast, reliable); fall back to a full scan.
        let mut found = probe_ports(ip, &curated_ports(), DEFAULT_PROBE_TIMEOUT).await?;
        if found.is_empty() {
            found = probe_host(ip, 1024, 65535, DEFAULT_PROBE_TIMEOUT).await?;
        }
        return Ok(found);
    }
    if let Some((host, port)) = address.rsplit_once(':') {
        if let (Ok(ip), Ok(port)) = (host.parse::<IpAddr>(), port.parse::<u16>()) {
            return probe_host(ip, port, port, DEFAULT_PROBE_TIMEOUT).await;
        }
    }

    let location = CastError::parse_url(address)?;
    let client = reqwest::Client::new();
    Ok(vec![describe(&client, &location).await?])
}

/// Scan a single host for DLNA MediaRenderers, bypassing SSDP.
///
/// Some networks drop SSDP multicast between clients, and some renderers ignore
/// unicast `M-SEARCH`. This reaches them anyway: it probes
/// `first_port..=last_port` for open TCP sockets and fetches each one as a UPnP
/// description, keeping the devices that expose `AVTransport`.
/// Concurrency budget for a port scan.
const SCAN_CONCURRENCY: usize = 2048;

/// Ports worth probing before a full scan: DLNA renderers cluster low (LG uses
/// ~1241) or in the UPnP ephemeral range, and a small probe does not make the
/// device drop SYNs the way a 64k-port burst does.
fn curated_ports() -> Vec<u16> {
    let mut ports: Vec<u16> = (1024..=2048).collect();
    ports.extend(49152..=49216);
    ports.extend([
        80, 443, 2869, 5000, 7000, 7676, 8008, 8009, 8200, 9080, 9081, 9197, 5555, 8060, 55000,
    ]);
    ports
}

/// Scan a port range on one host.
pub async fn probe_host(
    ip: IpAddr,
    first_port: u16,
    last_port: u16,
    connect_timeout: Duration,
) -> Result<Vec<CastDevice>> {
    let ports: Vec<u16> = (first_port..=last_port).collect();
    probe_ports(ip, &ports, connect_timeout).await
}

/// Scan a set of ports on one host and describe whatever answers.
pub async fn probe_ports(
    ip: IpAddr,
    ports: &[u16],
    connect_timeout: Duration,
) -> Result<Vec<CastDevice>> {
    let permits = Arc::new(Semaphore::new(SCAN_CONCURRENCY));
    let mut tasks: JoinSet<Option<u16>> = JoinSet::new();
    let mut open: Vec<u16> = Vec::new();

    for &port in ports {
        // The permit moves into the task, so the scan stays bounded.
        let Ok(permit) = permits.clone().acquire_owned().await else {
            break;
        };
        tasks.spawn(async move {
            let _permit = permit;
            match tokio::time::timeout(connect_timeout, TcpStream::connect((ip, port))).await {
                Ok(Ok(_)) => Some(port),
                _ => None,
            }
        });
        while let Some(joined) = tasks.try_join_next() {
            if let Ok(Some(port)) = joined {
                open.push(port);
            }
        }
    }
    while let Some(joined) = tasks.join_next().await {
        if let Ok(Some(port)) = joined {
            open.push(port);
        }
    }

    describe_ports(ip, open).await
}

/// Fetch a device description from each open port, concurrently.
async fn describe_ports(ip: IpAddr, open: Vec<u16>) -> Result<Vec<CastDevice>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    // Vendors disagree on where the description lives; these cover the devices
    // seen so far.
    const PATHS: [&str; 4] = ["/", "/description.xml", "/rootDesc.xml", "/dd.xml"];

    // Concurrently: a few open ports hang (they are not HTTP), and doing this in
    // series made a scan take a minute.
    let mut describes: JoinSet<Option<CastDevice>> = JoinSet::new();
    for port in open {
        let client = client.clone();
        describes.spawn(async move {
            for path in PATHS {
                let Ok(location) = Url::parse(&format!("http://{ip}:{port}{path}")) else {
                    continue;
                };
                if let Ok(device) = describe(&client, &location).await {
                    return Some(device);
                }
            }
            None
        });
    }

    let mut devices: Vec<CastDevice> = Vec::new();
    while let Some(joined) = describes.join_next().await {
        let Ok(Some(device)) = joined else { continue };
        let duplicate = devices.iter().any(|seen| seen.udn() == device.udn());
        if device.av_transport().is_some() && !duplicate {
            devices.push(device);
        }
    }
    Ok(devices)
}

/// The control surface a cast protocol must provide.
///
/// `reel-cast` ships the DLNA implementation; the trait exists so a Google Cast
/// target can be added later without the session/UI layers knowing which
/// protocol is in use.
#[async_trait]
pub trait CastTarget: Send + Sync {
    /// Point the renderer at a media URL and its DIDL-Lite metadata.
    async fn set_uri(&self, uri: &str, metadata: &str) -> Result<()>;
    /// Begin or resume playback.
    async fn play(&self) -> Result<()>;
    /// Pause playback.
    async fn pause(&self) -> Result<()>;
    /// Stop playback.
    async fn stop(&self) -> Result<()>;
    /// Seek to an absolute offset.
    async fn seek(&self, position: Duration) -> Result<()>;
    /// Current transport state.
    async fn transport_info(&self) -> Result<TransportInfo>;
    /// Current playback position and duration.
    async fn position_info(&self) -> Result<PositionInfo>;
    /// Current media information.
    async fn media_info(&self) -> Result<MediaInfo>;
    /// Set the master volume, 0–100.
    async fn set_volume(&self, volume: u8) -> Result<()>;
}

/// A DLNA/UPnP-AV target.
#[derive(Debug, Clone)]
pub struct DlnaTarget {
    client: reqwest::Client,
    control_url: Url,
    volume_url: Option<Url>,
}

impl DlnaTarget {
    /// Build a target from a described device.
    pub fn new(device: &CastDevice) -> Result<DlnaTarget> {
        let av = device
            .av_transport()
            .ok_or_else(|| CastError::Missing("AVTransport service".into()))?;
        Ok(DlnaTarget {
            // A renderer that hangs (e.g. wedged in TRANSITIONING) must not hang
            // the cast forever.
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            control_url: av.control_url.clone(),
            volume_url: device
                .description
                .rendering_control()
                .map(|service| service.control_url.clone()),
        })
    }

    /// The `AVTransport` control URL.
    pub fn control_url(&self) -> &Url {
        &self.control_url
    }

    /// Whether the device exposes volume control.
    pub fn has_volume(&self) -> bool {
        self.volume_url.is_some()
    }

    /// POST a SOAP action and return the response body, turning UPnP faults
    /// into errors.
    async fn call(&self, url: &Url, request: &soap::SoapRequest) -> Result<String> {
        post_soap(&self.client, url, request).await
    }

    /// Send an action to the `AVTransport` service.
    async fn av(&self, request: &soap::SoapRequest) -> Result<String> {
        self.call(&self.control_url, request).await
    }
}

/// POST a SOAP action and return the response body, turning UPnP faults into
/// errors.
async fn post_soap(
    client: &reqwest::Client,
    url: &Url,
    request: &soap::SoapRequest,
) -> Result<String> {
    let response = client
        .post(url.clone())
        .header(reqwest::header::CONTENT_TYPE, "text/xml; charset=\"utf-8\"")
        .header("SOAPACTION", request.soap_action())
        .body(request.envelope())
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;

    // A fault is returned as HTTP 500 with a SOAP Fault body.
    if let Some(fault) = soap::parse_fault(&body) {
        return Err(CastError::Fault {
            code: fault.code,
            description: fault.description,
        });
    }
    if !status.is_success() {
        return Err(CastError::HttpStatus {
            status: status.as_u16(),
            body,
        });
    }
    Ok(body)
}

/// What a renderer says it can play, from `ConnectionManager.GetProtocolInfo`.
///
/// `SinkProtocolInfo` is a comma-separated list of the `protocolInfo` strings a
/// renderer accepts (for example `http-get:*:video/mp4:*` or
/// `http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_MP_HD_1080i_AAC`), which is how a
/// controller decides whether to pass a file through or convert it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SinkCapabilities {
    /// The raw `SinkProtocolInfo` entries.
    pub entries: Vec<String>,
}

impl SinkCapabilities {
    /// Parse the `Sink` field of a `GetProtocolInfo` response.
    pub fn parse(sink: &str) -> SinkCapabilities {
        SinkCapabilities {
            entries: sink
                .split(',')
                .map(|entry| entry.trim().to_string())
                .filter(|entry| !entry.is_empty())
                .collect(),
        }
    }

    /// Whether any entry contains `needle` (case-insensitive).
    fn mentions(&self, needle: &str) -> bool {
        let needle = needle.to_ascii_lowercase();
        self.entries
            .iter()
            .any(|entry| entry.to_ascii_lowercase().contains(&needle))
    }

    /// Whether the renderer advertises H.265/HEVC support.
    pub fn supports_hevc(&self) -> bool {
        self.mentions("hevc") || self.mentions("h265") || self.mentions("h.265")
    }

    /// Whether the renderer advertises AC-3 support.
    pub fn supports_ac3(&self) -> bool {
        self.mentions("ac3") || self.mentions("ac-3")
    }

    /// Whether the renderer advertises E-AC-3 (Dolby Digital Plus) support.
    pub fn supports_eac3(&self) -> bool {
        self.mentions("eac3") || self.mentions("ec-3") || self.mentions("e-ac3")
    }

    /// Whether the renderer accepts a MIME type.
    pub fn accepts_mime(&self, mime: &str) -> bool {
        self.entries.iter().any(|entry| {
            let fields: Vec<&str> = entry.splitn(4, ':').collect();
            fields.len() >= 3 && (fields[2] == mime || fields[2] == "*")
        })
    }
}

/// Ask the renderer what it can play (`ConnectionManager.GetProtocolInfo`).
///
/// Returns `Err` when the device does not expose a ConnectionManager service or
/// does not answer, in which case the caller should fall back to a conservative
/// assumption.
pub async fn sink_capabilities(device: &CastDevice) -> Result<SinkCapabilities> {
    let service = device
        .description
        .service(device::CONNECTION_MANAGER)
        .ok_or_else(|| CastError::Missing("ConnectionManager service".into()))?;
    let request = soap::SoapRequest {
        service_type: device::CONNECTION_MANAGER,
        action: "GetProtocolInfo",
        arguments: Vec::new(),
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let body = post_soap(&client, &service.control_url, &request).await?;
    let root = crate::xml::parse(&body)?;
    Ok(SinkCapabilities::parse(
        root.find_text("Sink").unwrap_or_default(),
    ))
}

#[async_trait]
impl CastTarget for DlnaTarget {
    async fn set_uri(&self, uri: &str, metadata: &str) -> Result<()> {
        self.av(&soap::set_av_transport_uri(uri, metadata)).await?;
        Ok(())
    }

    async fn play(&self) -> Result<()> {
        self.av(&soap::play()).await?;
        Ok(())
    }

    async fn pause(&self) -> Result<()> {
        self.av(&soap::pause()).await?;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.av(&soap::stop()).await?;
        Ok(())
    }

    async fn seek(&self, position: Duration) -> Result<()> {
        self.av(&soap::seek_rel_time(position)).await?;
        Ok(())
    }

    async fn transport_info(&self) -> Result<TransportInfo> {
        let body = self.av(&soap::get_transport_info()).await?;
        soap::parse_transport_info(&body)
    }

    async fn position_info(&self) -> Result<PositionInfo> {
        let body = self.av(&soap::get_position_info()).await?;
        soap::parse_position_info(&body)
    }

    async fn media_info(&self) -> Result<MediaInfo> {
        let body = self.av(&soap::get_media_info()).await?;
        soap::parse_media_info(&body)
    }

    async fn set_volume(&self, volume: u8) -> Result<()> {
        let url = self
            .volume_url
            .as_ref()
            .ok_or_else(|| CastError::Missing("RenderingControl service".into()))?;
        self.call(url, &soap::set_volume(volume)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    const SINK: &str = "http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_BL_CIF15_AAC_520,\
        http-get:*:video/x-matroska:*,http-get:*:audio/mp4:DLNA.ORG_PN=AAC_ISO_320,\
        http-get:*:audio/x-flac:*";

    #[test]
    fn parses_and_reports_capabilities() {
        let caps = SinkCapabilities::parse(SINK);
        assert_eq!(caps.entries.len(), 4);
        assert!(caps.accepts_mime("video/mp4"));
        assert!(caps.accepts_mime("video/x-matroska"));
        assert!(!caps.accepts_mime("video/x-msvideo"));
        assert!(!caps.supports_hevc());
        assert!(!caps.supports_eac3());
    }

    #[test]
    fn recognises_hevc_and_eac3_when_advertised() {
        let caps = SinkCapabilities::parse(
            "http-get:*:video/mp4:DLNA.ORG_PN=HEVC_MP4_MP_HD_1080i_MAIN,\
             http-get:*:audio/vnd.dlna.adts:DLNA.ORG_PN=EAC3",
        );
        assert!(caps.supports_hevc());
        assert!(caps.supports_eac3());
    }
}
