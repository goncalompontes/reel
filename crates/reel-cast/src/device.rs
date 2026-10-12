//! UPnP device description: turn a `LOCATION` document into renderer services.
//!
//! A device description describes the device (name, model) and lists its
//! services with the relative URLs the controller must POST SOAP to. We only
//! care about `AVTransport` (playback control) and, optionally,
//! `RenderingControl` (volume).

use url::Url;

use crate::error::{CastError, Result};
use crate::xml;

/// UPnP service type for playback control.
pub const AV_TRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";

/// UPnP service type for volume/mute control.
pub const RENDERING_CONTROL: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

/// UPnP service type for protocol/format negotiation.
pub const CONNECTION_MANAGER: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

/// A service endpoint from a device description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// Service type URN, e.g. `urn:schemas-upnp-org:service:AVTransport:1`.
    pub service_type: String,
    /// Optional service id, e.g. `urn:upnp-org:serviceId:AVTransport`.
    pub service_id: Option<String>,
    /// Absolute URL to POST SOAP actions to.
    pub control_url: Url,
    /// Absolute URL for event subscriptions, if the device advertises one.
    pub event_sub_url: Option<Url>,
    /// Absolute URL of the service description document, if advertised.
    pub scpd_url: Option<Url>,
}

/// The parsed description of a renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDescription {
    /// Human-readable name shown in the device picker.
    pub friendly_name: String,
    /// `deviceType` URN.
    pub device_type: String,
    /// Stable unique device name, e.g. `uuid:abcd-1234`.
    pub udn: String,
    /// Manufacturer, when advertised.
    pub manufacturer: Option<String>,
    /// Model name, when advertised.
    pub model_name: Option<String>,
    /// Model number, when advertised.
    pub model_number: Option<String>,
    /// Base URL used to resolve relative service URLs (`URLBase` or the
    /// `LOCATION` URL).
    pub base_url: Url,
    /// Every advertised service.
    pub services: Vec<Service>,
}

impl DeviceDescription {
    /// The first service with the given URN.
    pub fn service(&self, service_type: &str) -> Option<&Service> {
        self.services
            .iter()
            .find(|service| service.service_type == service_type)
    }

    /// The playback-control service. A renderer without this cannot be cast to.
    pub fn av_transport(&self) -> Option<&Service> {
        self.service(AV_TRANSPORT)
    }

    /// The volume-control service, when present.
    pub fn rendering_control(&self) -> Option<&Service> {
        self.service(RENDERING_CONTROL)
    }

    /// Whether the device advertises itself as a MediaRenderer.
    pub fn is_media_renderer(&self) -> bool {
        self.device_type.contains("MediaRenderer")
    }
}

/// Parse a device description.
///
/// `location` is the URL the document was fetched from; relative service URLs
/// are resolved against the document's `URLBase` when present, else against
/// `location`.
pub fn parse_description(document: &str, location: &Url) -> Result<DeviceDescription> {
    let root = xml::parse(document)?;

    let url_base = match root.find_text("URLBase") {
        Some(base) => Some(CastError::parse_url(base)?),
        None => None,
    };
    let base_url = url_base.unwrap_or_else(|| location.clone());

    let device = root
        .find("device")
        .ok_or_else(|| CastError::Missing("device element".into()))?;

    let friendly_name = device
        .find_text("friendlyName")
        .ok_or_else(|| CastError::Missing("friendlyName".into()))?
        .to_string();
    let udn = device
        .find_text("UDN")
        .ok_or_else(|| CastError::Missing("UDN".into()))?
        .to_string();

    let mut services = Vec::new();
    for node in device.find_all("service") {
        let Some(service_type) = node.find_text("serviceType") else {
            continue;
        };
        let Some(control) = node.find_text("controlURL") else {
            continue;
        };
        services.push(Service {
            service_type: service_type.to_string(),
            service_id: node.find_text("serviceId").map(str::to_string),
            control_url: resolve(&base_url, control)?,
            event_sub_url: node
                .find_text("eventSubURL")
                .map(|url| resolve(&base_url, url))
                .transpose()?,
            scpd_url: node
                .find_text("SCPDURL")
                .map(|url| resolve(&base_url, url))
                .transpose()?,
        });
    }

    Ok(DeviceDescription {
        friendly_name,
        device_type: device
            .find_text("deviceType")
            .unwrap_or_default()
            .to_string(),
        udn,
        manufacturer: device.find_text("manufacturer").map(str::to_string),
        model_name: device.find_text("modelName").map(str::to_string),
        model_number: device.find_text("modelNumber").map(str::to_string),
        base_url,
        services,
    })
}

/// Resolve a possibly-relative URL against a base.
fn resolve(base: &Url, value: &str) -> Result<Url> {
    let value = value.trim();
    if let Ok(url) = Url::parse(value) {
        return Ok(url);
    }
    base.join(value)
        .map_err(|error| CastError::url(value, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESCRIPTION: &str = r#"<?xml version="1.0"?>
        <root xmlns="urn:schemas-upnp-org:device-1-0">
          <specVersion><major>1</major><minor>0</minor></specVersion>
          <device>
            <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
            <friendlyName>Living Room TV</friendlyName>
            <manufacturer>Samsung</manufacturer>
            <modelName>QE55Q80</modelName>
            <modelNumber>QE55Q80A</modelNumber>
            <UDN>uuid:abcd-1234</UDN>
            <serviceList>
              <service>
                <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
                <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
                <controlURL>/upnp/control/AVTransport1</controlURL>
                <eventSubURL>/upnp/event/AVTransport1</eventSubURL>
                <SCPDURL>/AVTransport1.xml</SCPDURL>
              </service>
              <service>
                <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
                <controlURL>/upnp/control/RenderingControl1</controlURL>
              </service>
            </serviceList>
          </device>
        </root>"#;

    fn location() -> Url {
        Url::parse("http://192.168.1.50:9197/dmr").unwrap()
    }

    #[test]
    fn parses_device_identity_and_services() {
        let device = parse_description(DESCRIPTION, &location()).unwrap();
        assert_eq!(device.friendly_name, "Living Room TV");
        assert_eq!(device.udn, "uuid:abcd-1234");
        assert_eq!(device.model_name.as_deref(), Some("QE55Q80"));
        assert!(device.is_media_renderer());
        assert_eq!(device.services.len(), 2);
    }

    #[test]
    fn resolves_relative_service_urls_against_the_location() {
        let device = parse_description(DESCRIPTION, &location()).unwrap();
        let av = device.av_transport().unwrap();
        assert_eq!(
            av.control_url.as_str(),
            "http://192.168.1.50:9197/upnp/control/AVTransport1"
        );
        assert_eq!(
            av.event_sub_url.as_ref().unwrap().as_str(),
            "http://192.168.1.50:9197/upnp/event/AVTransport1"
        );
        assert!(device.rendering_control().is_some());
    }

    #[test]
    fn honours_url_base_over_the_location() {
        let xml = r#"<root><URLBase>http://10.0.0.9:1234/base/</URLBase>
            <device><deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
            <friendlyName>TV</friendlyName><UDN>uuid:t</UDN>
            <serviceList><service>
              <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
              <controlURL>control</controlURL>
            </service></serviceList></device></root>"#;
        let device = parse_description(xml, &location()).unwrap();
        assert_eq!(
            device.av_transport().unwrap().control_url.as_str(),
            "http://10.0.0.9:1234/base/control"
        );
    }

    #[test]
    fn requires_a_friendly_name_and_udn() {
        let xml = r#"<root><device><deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType></device></root>"#;
        assert!(parse_description(xml, &location()).is_err());
    }
}
