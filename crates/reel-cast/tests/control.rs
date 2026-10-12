//! Integration test for the cast control path against an in-process fake
//! renderer.
//!
//! Real SSDP multicast is not CI-friendly, so discovery parsing is unit-tested
//! in `ssdp`; here we exercise everything above it — description fetch, service
//! resolution, SOAP control and fault handling — against a local HTTP server
//! that implements just enough of `AVTransport`/`RenderingControl`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use url::Url;

use reel_cast::CastError;
use reel_cast::target::{CastTarget, describe};

const AV: &str = "urn:schemas-upnp-org:service:AVTransport:1";

const DESCRIPTION: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>Fake TV</friendlyName>
    <manufacturer>reel</manufacturer>
    <modelName>Test Renderer</modelName>
    <UDN>uuid:fake-tv</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <controlURL>/control</controlURL>
        <eventSubURL>/event</eventSubURL>
        <SCPDURL>/av.xml</SCPDURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <controlURL>/volume</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

/// The mutable state of the fake device.
struct Renderer {
    state: String,
    uri: Option<String>,
    rel_time: u64,
    duration: u64,
    fail_seek: bool,
    actions: Vec<String>,
}

impl Default for Renderer {
    fn default() -> Self {
        Renderer {
            state: "STOPPED".to_string(),
            uri: None,
            rel_time: 0,
            duration: 42 * 60 + 31,
            fail_seek: false,
            actions: Vec::new(),
        }
    }
}

type Shared = Arc<Mutex<Renderer>>;

async fn start() -> (SocketAddr, Shared) {
    let renderer: Shared = Arc::new(Mutex::new(Renderer::default()));
    let app = Router::new()
        .route("/description.xml", get(description))
        .route("/control", post(control))
        .route("/volume", post(volume))
        .with_state(renderer.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, renderer)
}

async fn description() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/xml")], DESCRIPTION)
}

async fn control(State(state): State<Shared>, body: Bytes) -> Response {
    let text = String::from_utf8_lossy(&body);
    let action = action_name(&text);

    let mut renderer = state.lock().unwrap();
    renderer.actions.push(action.clone());

    match action.as_str() {
        "SetAVTransportURI" => {
            renderer.uri = element_text(&text, "CurrentURI");
            renderer.state = "STOPPED".to_string();
            ok(&av_response("SetAVTransportURI", ""))
        }
        "Play" => {
            renderer.state = "PLAYING".to_string();
            ok(&av_response("Play", ""))
        }
        "Pause" => {
            renderer.state = "PAUSED_PLAYBACK".to_string();
            ok(&av_response("Pause", ""))
        }
        "Stop" => {
            renderer.state = "STOPPED".to_string();
            renderer.rel_time = 0;
            ok(&av_response("Stop", ""))
        }
        "Seek" => {
            if renderer.fail_seek {
                return fault(711, "Illegal seek target");
            }
            if let Some(seconds) = element_text(&text, "Target")
                .and_then(|target| reel_cast::soap::parse_rel_time(&target))
            {
                renderer.rel_time = seconds.as_secs();
            }
            ok(&av_response("Seek", ""))
        }
        "GetTransportInfo" => {
            let inner = format!(
                "<CurrentTransportState>{}</CurrentTransportState>\
                 <CurrentTransportStatus>OK</CurrentTransportStatus>\
                 <CurrentSpeed>1</CurrentSpeed>",
                renderer.state
            );
            ok(&av_response("GetTransportInfo", &inner))
        }
        "GetPositionInfo" => {
            let rel = fmt_time(renderer.rel_time);
            let inner = format!(
                "<Track>1</Track><TrackDuration>{}</TrackDuration>\
                 <RelTime>{rel}</RelTime><AbsTime>{rel}</AbsTime><TrackURI>{}</TrackURI>",
                fmt_time(renderer.duration),
                renderer.uri.as_deref().unwrap_or_default(),
            );
            ok(&av_response("GetPositionInfo", &inner))
        }
        "GetMediaInfo" => {
            let inner = format!(
                "<NrTracks>1</NrTracks><MediaDuration>{}</MediaDuration><CurrentURI>{}</CurrentURI>",
                fmt_time(renderer.duration),
                renderer.uri.as_deref().unwrap_or_default(),
            );
            ok(&av_response("GetMediaInfo", &inner))
        }
        other => ok(&av_response(other, "")),
    }
}

async fn volume(State(state): State<Shared>, _body: Bytes) -> Response {
    state.lock().unwrap().actions.push("SetVolume".to_string());
    ok("<u:SetVolumeResponse xmlns:u=\"urn:schemas-upnp-org:service:RenderingControl:1\"/>")
}

fn action_name(document: &str) -> String {
    reel_cast::xml::parse(document)
        .ok()
        .and_then(|root| {
            let body = root.find("Body")?;
            body.children.first().map(|node| node.name().to_string())
        })
        .unwrap_or_default()
}

fn element_text(document: &str, name: &str) -> Option<String> {
    reel_cast::xml::parse(document)
        .ok()?
        .find_text(name)
        .map(str::to_string)
}

fn av_response(name: &str, inner: &str) -> String {
    format!("<u:{name}Response xmlns:u=\"{AV}\">{inner}</u:{name}Response>")
}

fn ok(response_element: &str) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body>{response_element}</s:Body></s:Envelope>"
    );
    xml_response(StatusCode::OK, body)
}

fn fault(code: i32, description: &str) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
         <s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring>\
         <detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
         <errorCode>{code}</errorCode><errorDescription>{description}</errorDescription>\
         </UPnPError></detail></s:Fault></s:Body></s:Envelope>"
    );
    xml_response(StatusCode::INTERNAL_SERVER_ERROR, body)
}

fn xml_response(status: StatusCode, body: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/xml; charset=\"utf-8\"")],
        body,
    )
        .into_response()
}

fn fmt_time(seconds: u64) -> String {
    reel_cast::soap::format_rel_time(Duration::from_secs(seconds))
}

async fn connect(addr: SocketAddr) -> reel_cast::CastDevice {
    let client = reqwest::Client::new();
    let location = Url::parse(&format!("http://{addr}/description.xml")).unwrap();
    describe(&client, &location).await.unwrap()
}

#[tokio::test]
async fn describes_a_device_and_resolves_its_services() {
    let (addr, _renderer) = start().await;
    let device = connect(addr).await;

    assert_eq!(device.name(), "Fake TV");
    assert_eq!(device.udn(), "uuid:fake-tv");
    assert_eq!(device.model(), Some("Test Renderer"));

    let target = device.target().unwrap();
    assert_eq!(
        target.control_url().as_str(),
        format!("http://{addr}/control")
    );
    assert!(target.has_volume());
}

#[tokio::test]
async fn drives_transport_and_reports_position() {
    let (addr, renderer) = start().await;
    let target = connect(addr).await.target().unwrap();

    target
        .set_uri("http://10.0.0.1/media/x.mp4", "<DIDL-Lite/>")
        .await
        .unwrap();
    target.play().await.unwrap();
    assert_eq!(target.transport_info().await.unwrap().state, "PLAYING");

    target.seek(Duration::from_secs(65)).await.unwrap();
    let position = target.position_info().await.unwrap();
    assert_eq!(position.rel_time.as_deref(), Some("0:01:05"));
    assert_eq!(position.track_duration.as_deref(), Some("0:42:31"));

    target.pause().await.unwrap();
    assert_eq!(
        target.transport_info().await.unwrap().state,
        "PAUSED_PLAYBACK"
    );

    target.set_volume(42).await.unwrap();
    target.stop().await.unwrap();
    assert_eq!(target.transport_info().await.unwrap().state, "STOPPED");

    let actions = renderer.lock().unwrap().actions.clone();
    assert_eq!(actions[0], "SetAVTransportURI");
    assert_eq!(actions[1], "Play");
    assert!(actions.contains(&"SetVolume".to_string()));
    assert_eq!(
        renderer.lock().unwrap().uri.as_deref(),
        Some("http://10.0.0.1/media/x.mp4")
    );
}

#[tokio::test]
async fn surfaces_a_upnp_fault() {
    let (addr, renderer) = start().await;
    renderer.lock().unwrap().fail_seek = true;

    let target = connect(addr).await.target().unwrap();
    match target.seek(Duration::from_secs(10)).await {
        Err(CastError::Fault { code, description }) => {
            assert_eq!(code, Some(711));
            assert_eq!(description, "Illegal seek target");
        }
        other => panic!("expected a fault, got {other:?}"),
    }
}
