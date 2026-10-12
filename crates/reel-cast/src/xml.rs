//! A tiny, lenient XML DOM built on `quick-xml`.
//!
//! UPnP speaks SOAP and DIDL-Lite in XML. Descriptions and responses are small
//! and well-formed, but namespaces vary wildly between vendors, so everything
//! here matches on **local** names (`s:Body`, `u:GetTransportInfoResponse`) and
//! ignores prefixes and namespace URIs.

use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::escape;
use quick_xml::events::{BytesStart, Event};

use crate::error::{CastError, Result};

/// One element. Text is the concatenated character data of *this* node only;
/// children are kept separately, so leaf lookups (`<CurrentTransportState>`)
/// are unambiguous.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Node {
    /// Local name, without any namespace prefix (e.g. `Body`, `service`).
    pub name: String,
    /// Attributes keyed by local name.
    pub attrs: Vec<(String, String)>,
    /// Child elements, in document order.
    pub children: Vec<Node>,
    /// Character data directly inside this element.
    pub text: String,
}

impl Node {
    /// Local name of this element.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// An attribute by local name.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Trimmed text of this element.
    pub fn text(&self) -> &str {
        self.text.trim()
    }

    /// The first direct child with the given local name.
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    /// Trimmed text of the first direct child with the given local name.
    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(Node::text)
    }

    /// All direct children with the given local name.
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// Depth-first search for the first descendant (including `self`) with the
    /// given local name.
    pub fn find(&self, name: &str) -> Option<&Node> {
        if self.name == name {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(name))
    }

    /// Depth-first search for every descendant (including `self`) with the
    /// given local name.
    pub fn find_all(&self, name: &str) -> Vec<&Node> {
        let mut out = Vec::new();
        self.collect_all(name, &mut out);
        out
    }

    fn collect_all<'a>(&'a self, name: &str, out: &mut Vec<&'a Node>) {
        if self.name == name {
            out.push(self);
        }
        for c in &self.children {
            c.collect_all(name, out);
        }
    }

    /// Trimmed text of the first descendant with the given local name.
    pub fn find_text(&self, name: &str) -> Option<&str> {
        self.find(name).map(Node::text)
    }
}

/// Parse a document, returning its first top-level element.
///
/// The XML declaration, comments, processing instructions and whitespace are
/// ignored. Entities in text and attribute values are unescaped.
pub fn parse(xml: &str) -> Result<Node> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;

    loop {
        let event = reader.read_event().map_err(xml_err)?;
        match event {
            Event::Start(start) => stack.push(start_node(&start)?),
            Event::Empty(start) => {
                let node = start_node(&start)?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(node),
                    None => root = Some(node),
                }
            }
            Event::End(_) => {
                let node = stack.pop().expect("end event without matching start");
                match stack.last_mut() {
                    Some(parent) => parent.children.push(node),
                    None => root = Some(node),
                }
            }
            Event::Text(text) => {
                if let Some(current) = stack.last_mut() {
                    let raw = text.xml10_content().map_err(xml_err)?;
                    let unescaped = escape::unescape(&raw).map_err(xml_err)?;
                    current.text.push_str(&unescaped);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    root.ok_or_else(|| CastError::Xml("document has no root element".into()))
}

fn start_node(start: &BytesStart<'_>) -> Result<Node> {
    let name = local_name(start.name().as_ref());
    let mut attrs = Vec::new();
    for attr in start.attributes() {
        let attr = attr.map_err(xml_err)?;
        let key = local_name(attr.key.as_ref());
        let value = attr
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(xml_err)?
            .into_owned();
        attrs.push((key, value));
    }
    Ok(Node {
        name,
        attrs,
        children: Vec::new(),
        text: String::new(),
    })
}

/// Strip an XML namespace prefix from a raw qualified name.
fn local_name(raw: &[u8]) -> String {
    let name = String::from_utf8_lossy(raw);
    match name.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => name.into_owned(),
    }
}

fn xml_err(error: impl std::fmt::Display) -> CastError {
    CastError::Xml(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOAP: &str = r#"<?xml version="1.0"?>
        <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
          <s:Body>
            <u:GetTransportInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
              <CurrentTransportState>PLAYING</CurrentTransportState>
              <CurrentTransportStatus>OK</CurrentTransportStatus>
            </u:GetTransportInfoResponse>
          </s:Body>
        </s:Envelope>"#;

    #[test]
    fn parses_namespaced_elements_by_local_name() {
        let root = parse(SOAP).unwrap();
        assert_eq!(root.name(), "Envelope");
        let body = root.find("Body").unwrap();
        let response = &body.children[0];
        assert_eq!(response.name(), "GetTransportInfoResponse");
        assert_eq!(
            response.child_text("CurrentTransportState"),
            Some("PLAYING")
        );
    }

    #[test]
    fn collects_attributes_and_unescapes_them() {
        let root = parse(r#"<res protocolInfo="a&amp;b">x</res>"#).unwrap();
        assert_eq!(root.attr("protocolInfo"), Some("a&b"));
        assert_eq!(root.text(), "x");
    }

    #[test]
    fn find_all_returns_every_descendant() {
        let root = parse("<a><s>1</s><s>2</s></a>").unwrap();
        let all: Vec<_> = root.find_all("s").iter().map(|n| n.text()).collect();
        assert_eq!(all, vec!["1", "2"]);
    }

    #[test]
    fn rejects_documents_without_a_root() {
        assert!(parse("   ").is_err());
    }
}
