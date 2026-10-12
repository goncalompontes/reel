//! DIDL-Lite metadata.
//!
//! `SetAVTransportURI` takes the media URL *and* a DIDL-Lite document
//! describing what is at that URL. Many renderers (notably some Samsung and
//! LG sets) will not start playback without it, and it is where the
//! `protocolInfo` (mime + DLNA content features) is advertised.

use crate::dlna;

/// The DIDL-Lite namespace.
pub const DIDL_NS: &str = "urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/";

/// The Dublin Core namespace (titles, dates).
pub const DC_NS: &str = "http://purl.org/dc/elements/1.1/";

/// The UPnP metadata namespace (classes, artwork).
pub const UPNP_NS: &str = "urn:schemas-upnp-org:metadata-1-0/upnp/";

/// The Samsung/LG caption extension namespace. Both accept external subtitles
/// advertised through `sec:CaptionInfoEx`, which is how a text sidecar reaches
/// a renderer that will not read subtitles out of the container.
pub const SEC_NS: &str = "http://www.sec.co.kr/";

/// UPnP class for a single video.
pub const CLASS_VIDEO: &str = "object.item.videoItem";

/// A media resource: where it is and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    /// Absolute URL the renderer should fetch.
    pub url: String,
    /// `protocolInfo`, as built by [`dlna::protocol_info`].
    pub protocol_info: String,
}

impl Resource {
    /// A subtitle resource a renderer can fetch as a sidecar.
    pub fn subtitle(url: impl Into<String>) -> Resource {
        Resource {
            url: url.into(),
            protocol_info: "http-get:*:text/srt:*".to_string(),
        }
    }
}

/// A single item to hand to a renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Item id, unique within the metadata document.
    pub id: String,
    /// Parent id; `-1` for a standalone item.
    pub parent_id: String,
    /// Title rendered by the TV's now-playing screen.
    pub title: String,
    /// UPnP class URN, e.g. [`CLASS_VIDEO`].
    pub class: String,
    /// Resources (usually one).
    pub resources: Vec<Resource>,
    /// Subtitle sidecars. Emitted both as extra `<res>` elements and as
    /// `sec:CaptionInfoEx`, which is what LG/Samsung sets actually read.
    pub captions: Vec<Resource>,
}

impl Item {
    /// A video item with a single `http-get` resource.
    ///
    /// `seekable` advertises whether the URL supports byte-range seeks — a live
    /// remux does not, so the receiver should not try to seek it.
    pub fn video(
        title: impl Into<String>,
        url: impl Into<String>,
        seekable: bool,
        converted: bool,
    ) -> Item {
        let url = url.into();
        Item {
            id: "0".to_string(),
            parent_id: "-1".to_string(),
            title: title.into(),
            class: CLASS_VIDEO.to_string(),
            resources: vec![Resource {
                protocol_info: dlna::protocol_info(dlna::mime_for_url(&url), seekable, converted),
                url,
            }],
            captions: Vec::new(),
        }
    }

    /// Attach subtitle sidecars, delivered as `<res>` elements and via
    /// `sec:CaptionInfoEx`.
    pub fn with_captions(mut self, captions: Vec<Resource>) -> Item {
        self.captions = captions;
        self
    }

    /// Serialise to a DIDL-Lite document.
    pub fn to_xml(&self) -> String {
        let mut out = String::new();
        out.push_str("<DIDL-Lite");
        out.push_str(&format!(" xmlns=\"{DIDL_NS}\""));
        out.push_str(&format!(" xmlns:dc=\"{DC_NS}\""));
        out.push_str(&format!(" xmlns:upnp=\"{UPNP_NS}\""));
        out.push_str(&format!(" xmlns:sec=\"{SEC_NS}\""));
        out.push('>');
        out.push_str(&format!(
            "<item id=\"{}\" parentID=\"{}\" restricted=\"1\">",
            escape(&self.id),
            escape(&self.parent_id)
        ));
        out.push_str(&format!("<dc:title>{}</dc:title>", escape(&self.title)));
        out.push_str(&format!("<upnp:class>{}</upnp:class>", escape(&self.class)));
        for resource in &self.resources {
            out.push_str(&format!(
                "<res protocolInfo=\"{}\">{}</res>",
                escape(&resource.protocol_info),
                escape(&resource.url)
            ));
        }
        for caption in &self.captions {
            out.push_str(&format!(
                "<res protocolInfo=\"{}\">{}</res>",
                escape(&caption.protocol_info),
                escape(&caption.url)
            ));
        }
        for caption in &self.captions {
            out.push_str(&format!(
                "<sec:CaptionInfoEx sec:type=\"srt\">{}</sec:CaptionInfoEx>",
                escape(&caption.url)
            ));
        }
        out.push_str("</item></DIDL-Lite>");
        out
    }
}

/// Escape text for use in XML character data or a double-quoted attribute.
pub fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_characters() {
        assert_eq!(escape("a & b <c>"), "a &amp; b &lt;c&gt;");
        assert_eq!(escape(r#"say "hi""#), "say &quot;hi&quot;");
    }

    #[test]
    fn builds_a_video_item() {
        let xml = Item::video("Episode 1", "http://10.0.0.1/media/x.mp4", true, false).to_xml();
        assert!(xml.starts_with("<DIDL-Lite"));
        assert!(xml.contains("<dc:title>Episode 1</dc:title>"));
        assert!(xml.contains("<upnp:class>object.item.videoItem</upnp:class>"));
        assert!(xml.contains("protocolInfo=\"http-get:*:video/mp4:"));
        assert!(xml.contains(">http://10.0.0.1/media/x.mp4</res>"));
    }

    #[test]
    fn a_non_seekable_resource_advertises_no_seeking() {
        let xml = Item::video("Live", "http://h/x.mp4", false, true).to_xml();
        assert!(xml.contains("DLNA.ORG_OP=00"));
        assert!(xml.contains("DLNA.ORG_CI=1"));
    }

    #[test]
    fn escapes_the_title() {
        let xml = Item::video("Tom & Jerry", "http://h/x.mp4", true, false).to_xml();
        assert!(xml.contains("<dc:title>Tom &amp; Jerry</dc:title>"));
    }

    #[test]
    fn advertises_subtitles_as_res_and_caption_info() {
        let xml = Item::video("Episode", "http://h/x.mkv", true, false)
            .with_captions(vec![Resource::subtitle("http://h/sub.srt")])
            .to_xml();
        assert!(xml.contains("xmlns:sec=\"http://www.sec.co.kr/\""));
        assert!(xml.contains("<res protocolInfo=\"http-get:*:text/srt:*\">http://h/sub.srt</res>"));
        assert!(xml.contains("<sec:CaptionInfoEx sec:type=\"srt\">http://h/sub.srt</sec:CaptionInfoEx>"));
    }
}
