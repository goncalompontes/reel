//! SSDP: discover MediaRenderers on the local network.
//!
//! This is the client half of SSDP (the part a controller plays): send an
//! `M-SEARCH` to the SSDP multicast group and collect the unicast replies. Each
//! reply carries a `LOCATION` URL pointing at the device description XML, which
//! [`crate::device`] then parses.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use url::Url;

use crate::error::{CastError, Result};

/// The SSDP multicast group.
pub const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);

/// The SSDP port.
pub const SSDP_PORT: u16 = 1900;

/// Search target for a UPnP AV MediaRenderer.
pub const MEDIA_RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";

/// One reply (or announcement) to a discovery search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsdpResponse {
    /// `LOCATION`: URL of the device description document.
    pub location: Url,
    /// Search target (`ST`), or notification type (`NT`) for announcements.
    pub st: String,
    /// Unique service name identifying the device and the service it answered for.
    pub usn: String,
    /// `SERVER` header, a device/OS/UPnP description.
    pub server: Option<String>,
    /// Advertised lifetime from `CACHE-CONTROL: max-age=…`, in seconds.
    pub max_age: Option<u64>,
}

impl SsdpResponse {
    /// Parse a raw datagram. Accepts both `HTTP/1.1 200 OK` replies and
    /// `NOTIFY * HTTP/1.1` announcements; headers are matched case-insensitively.
    pub fn parse(bytes: &[u8]) -> Result<SsdpResponse> {
        let text = String::from_utf8_lossy(bytes);
        let mut lines = text.split('\n').map(|line| line.trim_end_matches('\r'));

        let status = lines.next().unwrap_or_default();
        if !(status.starts_with("HTTP/1.1 200") || status.starts_with("NOTIFY")) {
            return Err(CastError::Ssdp(format!(
                "unexpected status line {status:?}"
            )));
        }

        let mut headers: Vec<(String, String)> = Vec::new();
        for line in lines {
            if line.trim().is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                headers.push((key.trim().to_ascii_lowercase(), value.trim().to_string()));
            }
        }

        let get = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };

        let location =
            get("location").ok_or_else(|| CastError::Ssdp("missing LOCATION header".into()))?;
        let location = CastError::parse_url(location)?;

        Ok(SsdpResponse {
            location,
            st: get("st")
                .or_else(|| get("nt"))
                .unwrap_or_default()
                .to_string(),
            usn: get("usn").unwrap_or_default().to_string(),
            server: get("server").map(str::to_string),
            max_age: get("cache-control").and_then(parse_max_age),
        })
    }
}

/// Extract the `max-age` seconds from a `CACHE-CONTROL` value.
fn parse_max_age(value: &str) -> Option<u64> {
    value
        .split(',')
        .find_map(|part| part.trim().strip_prefix("max-age="))
        .map(|value| value.trim_matches('"'))
        .and_then(|value| value.parse().ok())
}

/// Build an `M-SEARCH` request for the given search target.
///
/// `mx` is the maximum number of seconds the device may wait before replying;
/// it should be at least 1 (and, per the spec, not more than 5).
pub fn m_search(st: &str, mx: u8) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: {MULTICAST_ADDR}:{SSDP_PORT}\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: {mx}\r\n\
         ST: {st}\r\n\
         \r\n"
    )
}

/// Search for media renderers, collecting replies until `timeout` elapses.
///
/// Replies are deduplicated by `USN`. The socket is bound to the ephemeral
/// port, so this performs an active search (no dependency on receiving
/// periodic `NOTIFY` announcements), which is what UPnP controllers normally
/// do.
pub async fn discover(timeout: Duration) -> Result<Vec<SsdpResponse>> {
    discover_for(MEDIA_RENDERER, timeout).await
}

/// Like [`discover`], but for an arbitrary search target.
pub async fn discover_for(st: &str, timeout: Duration) -> Result<Vec<SsdpResponse>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    let request = m_search(st, 2);
    socket
        .send_to(
            request.as_bytes(),
            SocketAddr::from((MULTICAST_ADDR, SSDP_PORT)),
        )
        .await?;

    let mut found: Vec<SsdpResponse> = Vec::new();
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 16 * 1024];

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((len, _from))) => {
                if let Ok(response) = SsdpResponse::parse(&buf[..len]) {
                    let duplicate = found.iter().any(|seen| seen.usn == response.usn);
                    if !duplicate {
                        tracing::debug!(location = %response.location, usn = %response.usn, "found renderer");
                        found.push(response);
                    }
                }
            }
            Ok(Err(error)) => return Err(error.into()),
            // Nothing more arrived before the deadline.
            Err(_elapsed) => break,
        }
    }

    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPLY: &str = "HTTP/1.1 200 OK\r\n\
        CACHE-CONTROL: max-age=1800\r\n\
        EXT:\r\n\
        LOCATION: http://192.168.1.50:9197/dmr\r\n\
        SERVER: Linux/3.14 UPnP/1.0 RpMedia/1.0\r\n\
        ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
        USN: uuid:abcd-1234::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
        \r\n";

    #[test]
    fn m_search_has_the_required_headers() {
        let request = m_search(MEDIA_RENDERER, 2);
        assert!(request.starts_with("M-SEARCH * HTTP/1.1\r\n"));
        assert!(request.contains("HOST: 239.255.255.250:1900\r\n"));
        assert!(request.contains("MAN: \"ssdp:discover\"\r\n"));
        assert!(request.contains("MX: 2\r\n"));
        assert!(request.ends_with("\r\n\r\n"));
    }

    #[test]
    fn parses_a_renderer_reply() {
        let response = SsdpResponse::parse(REPLY.as_bytes()).unwrap();
        assert_eq!(response.location.as_str(), "http://192.168.1.50:9197/dmr");
        assert_eq!(response.st, MEDIA_RENDERER);
        assert_eq!(
            response.usn,
            "uuid:abcd-1234::urn:schemas-upnp-org:device:MediaRenderer:1"
        );
        assert_eq!(response.max_age, Some(1800));
        assert_eq!(
            response.server.as_deref(),
            Some("Linux/3.14 UPnP/1.0 RpMedia/1.0")
        );
    }

    #[test]
    fn tolerates_lowercase_header_names_and_notify() {
        let notice = "NOTIFY * HTTP/1.1\r\n\
            location: http://10.0.0.2:8080/desc.xml\r\n\
            nt: upnp:rootdevice\r\n\
            usn: uuid:xyz::upnp:rootdevice\r\n\
            \r\n";
        let response = SsdpResponse::parse(notice.as_bytes()).unwrap();
        assert_eq!(response.st, "upnp:rootdevice");
        assert_eq!(response.location.as_str(), "http://10.0.0.2:8080/desc.xml");
        assert_eq!(response.max_age, None);
    }

    #[test]
    fn rejects_a_non_ssdp_datagram() {
        assert!(SsdpResponse::parse(b"hello world").is_err());
        assert!(SsdpResponse::parse(b"HTTP/1.1 200 OK\r\nEXT:\r\n\r\n").is_err());
    }
}
