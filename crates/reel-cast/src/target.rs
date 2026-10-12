//! Cast targets: describe a renderer and drive it over HTTP.
//!
//! [`CastDevice`] is a discovered renderer (its `LOCATION` plus the parsed
//! description). [`DlnaTarget`] implements the [`CastTarget`] trait by POSTing
//! the SOAP documents from [`crate::soap`] to the device's `AVTransport` and
//! `RenderingControl` control URLs.

use std::time::Duration;

use async_trait::async_trait;
use url::Url;

use crate::device::{self, DeviceDescription, Service};
use crate::error::{CastError, Result};
use crate::soap::{self, MediaInfo, PositionInfo, TransportInfo};
use crate::ssdp;

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
/// Devices without an `AVTransport` service are skipped (they cannot be cast
/// to), and duplicates are collapsed by `UDN`. A device that fails to describe
/// is logged and ignored, so one broken renderer does not hide the others.
pub async fn discover_devices(timeout: Duration) -> Result<Vec<CastDevice>> {
    let client = reqwest::Client::new();
    let responses = ssdp::discover(timeout).await?;

    let mut devices: Vec<CastDevice> = Vec::new();
    for response in responses {
        match describe(&client, &response.location).await {
            Ok(device) if device.av_transport().is_some() => {
                if devices.iter().any(|seen| seen.udn() == device.udn()) {
                    continue;
                }
                tracing::debug!(name = device.name(), "described renderer");
                devices.push(device);
            }
            Ok(_) => tracing::debug!(location = %response.location, "not an AVTransport renderer"),
            Err(error) => {
                tracing::debug!(location = %response.location, %error, "could not describe renderer")
            }
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
            client: reqwest::Client::new(),
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
        let response = self
            .client
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

    /// Send an action to the `AVTransport` service.
    async fn av(&self, request: &soap::SoapRequest) -> Result<String> {
        self.call(&self.control_url, request).await
    }
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
