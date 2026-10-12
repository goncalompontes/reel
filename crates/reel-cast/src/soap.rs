//! SOAP requests and responses for the UPnP `AVTransport` / `RenderingControl`
//! services.
//!
//! Requests are built here as plain strings (the bodies are tiny and fixed),
//! and responses are parsed through [`crate::xml`]. Faults are surfaced as
//! [`CastError::Fault`].

use std::time::Duration;

use crate::device::{AV_TRANSPORT, RENDERING_CONTROL};
use crate::didl::escape;
use crate::error::{CastError, Result};
use crate::xml;

/// A SOAP call: the service it targets, the action name and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoapRequest {
    /// Target service type URN.
    pub service_type: &'static str,
    /// Action name, e.g. `Play`.
    pub action: &'static str,
    /// Ordered arguments (`InstanceID` is always first for AVTransport).
    pub arguments: Vec<(&'static str, String)>,
}

impl SoapRequest {
    /// The `SOAPACTION` HTTP header value.
    pub fn soap_action(&self) -> String {
        format!("\"{}#{}\"", self.service_type, self.action)
    }

    /// The request body.
    pub fn envelope(&self) -> String {
        let mut body = String::new();
        body.push_str(&format!(
            "<u:{action} xmlns:u=\"{service}\">",
            action = self.action,
            service = self.service_type
        ));
        for (name, value) in &self.arguments {
            body.push_str(&format!("<{name}>{}</{name}>", escape(value)));
        }
        body.push_str(&format!("</u:{}>", self.action));

        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
             <s:Body>{body}</s:Body></s:Envelope>"
        )
    }
}

fn av(action: &'static str, mut arguments: Vec<(&'static str, String)>) -> SoapRequest {
    let mut all = vec![("InstanceID", "0".to_string())];
    all.append(&mut arguments);
    SoapRequest {
        service_type: AV_TRANSPORT,
        action,
        arguments: all,
    }
}

/// `SetAVTransportURI`: point the renderer at a URL and its DIDL-Lite metadata.
pub fn set_av_transport_uri(uri: &str, metadata: &str) -> SoapRequest {
    av(
        "SetAVTransportURI",
        vec![
            ("CurrentURI", uri.to_string()),
            ("CurrentURIMetaData", metadata.to_string()),
        ],
    )
}

/// `Play` at normal speed.
pub fn play() -> SoapRequest {
    av("Play", vec![("Speed", "1".to_string())])
}

/// `Pause`.
pub fn pause() -> SoapRequest {
    av("Pause", vec![])
}

/// `Stop`.
pub fn stop() -> SoapRequest {
    av("Stop", vec![])
}

/// `Seek` to an absolute offset (`Unit=REL_TIME`).
pub fn seek_rel_time(target: Duration) -> SoapRequest {
    av(
        "Seek",
        vec![
            ("Unit", "REL_TIME".to_string()),
            ("Target", format_rel_time(target)),
        ],
    )
}

/// `GetTransportInfo`.
pub fn get_transport_info() -> SoapRequest {
    av("GetTransportInfo", vec![])
}

/// `GetPositionInfo`.
pub fn get_position_info() -> SoapRequest {
    av("GetPositionInfo", vec![])
}

/// `GetMediaInfo`.
pub fn get_media_info() -> SoapRequest {
    av("GetMediaInfo", vec![])
}

/// `RenderingControl.SetVolume` for the master channel.
pub fn set_volume(volume: u8) -> SoapRequest {
    SoapRequest {
        service_type: RENDERING_CONTROL,
        action: "SetVolume",
        arguments: vec![
            ("InstanceID", "0".to_string()),
            ("Channel", "Master".to_string()),
            ("DesiredVolume", volume.min(100).to_string()),
        ],
    }
}

/// Transport state and speed, from a `GetTransportInfo` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportInfo {
    /// e.g. `PLAYING`, `PAUSED_PLAYBACK`, `STOPPED`, `TRANSITIONING`.
    pub state: String,
    /// e.g. `OK`, `ERROR_OCCURRED`.
    pub status: Option<String>,
    /// Usually `1`.
    pub speed: Option<String>,
}

/// Playback position, from a `GetPositionInfo` response.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PositionInfo {
    /// Current track number.
    pub track: Option<u32>,
    /// Total duration of the track, e.g. `0:42:31`.
    pub track_duration: Option<String>,
    /// Elapsed time, e.g. `0:12:05`.
    pub rel_time: Option<String>,
    /// Absolute (counter) time.
    pub abs_time: Option<String>,
    /// URL currently playing.
    pub track_uri: Option<String>,
}

/// Media information, from a `GetMediaInfo` response.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaInfo {
    /// Number of tracks.
    pub nr_tracks: Option<u32>,
    /// Duration of the media, e.g. `0:42:31`.
    pub media_duration: Option<String>,
    /// Current URI.
    pub current_uri: Option<String>,
}

/// A UPnP fault returned by the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoapFault {
    /// Numeric `UPnPError/errorCode`, when present.
    pub code: Option<i32>,
    /// `errorDescription` or `faultstring`.
    pub description: String,
}

/// Parse a `GetTransportInfo` response.
pub fn parse_transport_info(document: &str) -> Result<TransportInfo> {
    let response = response_node(document)?;
    Ok(TransportInfo {
        state: response
            .child_text("CurrentTransportState")
            .ok_or_else(|| CastError::Missing("CurrentTransportState".into()))?
            .to_string(),
        status: response
            .child_text("CurrentTransportStatus")
            .map(str::to_string),
        speed: response.child_text("CurrentSpeed").map(str::to_string),
    })
}

/// Parse a `GetPositionInfo` response.
pub fn parse_position_info(document: &str) -> Result<PositionInfo> {
    let response = response_node(document)?;
    Ok(PositionInfo {
        track: response
            .child_text("Track")
            .and_then(|value| value.parse().ok()),
        track_duration: response.child_text("TrackDuration").map(str::to_string),
        rel_time: response.child_text("RelTime").map(str::to_string),
        abs_time: response.child_text("AbsTime").map(str::to_string),
        track_uri: response.child_text("TrackURI").map(str::to_string),
    })
}

/// Parse a `GetMediaInfo` response.
pub fn parse_media_info(document: &str) -> Result<MediaInfo> {
    let response = response_node(document)?;
    Ok(MediaInfo {
        nr_tracks: response
            .child_text("NrTracks")
            .and_then(|value| value.parse().ok()),
        media_duration: response.child_text("MediaDuration").map(str::to_string),
        current_uri: response.child_text("CurrentURI").map(str::to_string),
    })
}

/// Parse a fault, if the document carries one.
pub fn parse_fault(document: &str) -> Option<SoapFault> {
    let root = xml::parse(document).ok()?;
    let fault = root.find("Fault")?;
    let code = fault
        .find("errorCode")
        .and_then(|node| node.text().parse().ok());
    let description = fault
        .find("errorDescription")
        .or_else(|| fault.find("faultstring"))
        .map(|node| node.text().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown upnp error".to_string());
    Some(SoapFault { code, description })
}

/// Return the action response element, or convert a fault into an error.
fn response_node(document: &str) -> Result<xml::Node> {
    let root = xml::parse(document)?;
    let body = root
        .find("Body")
        .ok_or_else(|| CastError::Missing("soap Body".into()))?;
    let response = body
        .children
        .first()
        .ok_or_else(|| CastError::Missing("soap response element".into()))?
        .clone();

    if response.name() == "Fault" {
        let fault = parse_fault(document).unwrap_or(SoapFault {
            code: None,
            description: "unknown upnp error".to_string(),
        });
        return Err(CastError::Fault {
            code: fault.code,
            description: fault.description,
        });
    }
    Ok(response)
}

/// Format a duration as the UPnP `H:MM:SS` time string.
pub fn format_rel_time(duration: Duration) -> String {
    let total = duration.as_secs();
    format!(
        "{}:{:02}:{:02}",
        total / 3600,
        (total % 3600) / 60,
        total % 60
    )
}

/// Parse a UPnP `H:MM:SS[.fraction]` or `HH:MM:SS` time string.
pub fn parse_rel_time(value: &str) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("not_implemented") {
        return None;
    }
    let mut parts = value.split(':').rev();
    let seconds: f64 = parts.next()?.parse().ok()?;
    let minutes: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let hours: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Some(Duration::from_secs_f64(
        hours as f64 * 3600.0 + minutes as f64 * 60.0 + seconds,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRANSPORT: &str = r#"<?xml version="1.0"?>
        <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"
                    s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
          <s:Body>
            <u:GetTransportInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
              <CurrentTransportState>PLAYING</CurrentTransportState>
              <CurrentTransportStatus>OK</CurrentTransportStatus>
              <CurrentSpeed>1</CurrentSpeed>
            </u:GetTransportInfoResponse>
          </s:Body>
        </s:Envelope>"#;

    const POSITION: &str = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
        <u:GetPositionInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
          <Track>1</Track><TrackDuration>0:42:31</TrackDuration>
          <RelTime>0:12:05</RelTime><AbsTime>0:12:05</AbsTime>
          <TrackURI>http://10.0.0.1/media/x.mp4</TrackURI>
        </u:GetPositionInfoResponse></s:Body></s:Envelope>"#;

    const FAULT: &str = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
        <s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring>
        <detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
          <errorCode>714</errorCode><errorDescription>No such object</errorDescription>
        </UPnPError></detail></s:Fault></s:Body></s:Envelope>"#;

    #[test]
    fn builds_set_av_transport_uri() {
        let request = set_av_transport_uri("http://h/x.mp4", "<DIDL-Lite/>");
        assert_eq!(
            request.soap_action(),
            "\"urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI\""
        );
        let body = request.envelope();
        assert!(body.contains(
            "<u:SetAVTransportURI xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">"
        ));
        assert!(body.contains("<InstanceID>0</InstanceID>"));
        assert!(body.contains("<CurrentURI>http://h/x.mp4</CurrentURI>"));
        // Nested metadata must be escaped, not injected.
        assert!(body.contains("<CurrentURIMetaData>&lt;DIDL-Lite/&gt;</CurrentURIMetaData>"));
    }

    #[test]
    fn play_includes_speed() {
        let body = play().envelope();
        assert!(body.contains("<Speed>1</Speed>"));
    }

    #[test]
    fn parses_transport_and_position_responses() {
        let info = parse_transport_info(TRANSPORT).unwrap();
        assert_eq!(info.state, "PLAYING");
        assert_eq!(info.speed.as_deref(), Some("1"));

        let position = parse_position_info(POSITION).unwrap();
        assert_eq!(position.track, Some(1));
        assert_eq!(position.rel_time.as_deref(), Some("0:12:05"));
        assert_eq!(position.track_duration.as_deref(), Some("0:42:31"));
    }

    #[test]
    fn turns_a_fault_into_an_error() {
        let fault = parse_fault(FAULT).unwrap();
        assert_eq!(fault.code, Some(714));
        assert_eq!(fault.description, "No such object");

        match parse_transport_info(FAULT) {
            Err(CastError::Fault { code, description }) => {
                assert_eq!(code, Some(714));
                assert_eq!(description, "No such object");
            }
            other => panic!("expected a fault, got {other:?}"),
        }
    }

    #[test]
    fn formats_and_parses_times() {
        assert_eq!(format_rel_time(Duration::from_secs(12 * 60 + 5)), "0:12:05");
        assert_eq!(format_rel_time(Duration::from_secs(3600 + 61)), "1:01:01");
        assert_eq!(parse_rel_time("0:12:05"), Some(Duration::from_secs(725)));
        assert_eq!(
            parse_rel_time("00:00:03.500"),
            Some(Duration::from_millis(3500))
        );
        assert_eq!(parse_rel_time("NOT_IMPLEMENTED"), None);
    }
}
