//! A local fake DLNA TV, so casting can be tested without a real device.
//!
//! It answers SSDP `M-SEARCH`, serves a device description, accepts
//! `AVTransport` and `RenderingControl` SOAP, and — when told to play — fetches
//! the media URL it was handed, so you can see the LAN media server is actually
//! reachable and inspect its DLNA headers.
//!
//! ```text
//! cargo run -p reel-cast --example fake_tv
//! ```
//!
//! Then enable **Settings → Casting → Stream to DLNA/UPnP devices**, restart
//! reel, and use the Cast button. The device appears as **reel fake TV**. Press
//! `Ctrl-C` here when you are done.
//!
//! An optional port may be passed as the first argument (default: an ephemeral
//! one).

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use reel_cast::ssdp::{MEDIA_RENDERER, MULTICAST_ADDR, SSDP_PORT};

const AV: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RENDERING: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const UDN: &str = "uuid:reel-fake-tv";

const DESCRIPTION: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>reel fake TV</friendlyName>
    <manufacturer>reel</manufacturer>
    <modelName>Local test renderer</modelName>
    <UDN>uuid:reel-fake-tv</UDN>
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

/// The fake device's state.
struct Tv {
    state: String,
    uri: Option<String>,
    started: Option<Instant>,
    /// Position at the last pause/stop.
    position: f64,
}

impl Tv {
    fn new() -> Tv {
        Tv {
            state: "STOPPED".to_string(),
            uri: None,
            started: None,
            position: 0.0,
        }
    }

    fn position(&self) -> f64 {
        match self.started {
            Some(at) if self.state == "PLAYING" => self.position + at.elapsed().as_secs_f64(),
            _ => self.position,
        }
    }

    fn freeze(&mut self) {
        self.position = self.position();
        self.started = None;
    }
}

#[derive(Clone)]
struct AppState {
    tv: Arc<Mutex<Tv>>,
    client: Arc<reqwest::Client>,
}

#[tokio::main]
async fn main() {
    let port = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let ip = local_ip().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));

    let state = AppState {
        tv: Arc::new(Mutex::new(Tv::new())),
        client: Arc::new(reqwest::Client::new()),
    };

    let app = Router::new()
        .route("/description.xml", get(description))
        .route("/control", post(control))
        .route("/volume", post(volume))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))
        .await
        .expect("bind the fake TV's HTTP port");
    let http_port = listener.local_addr().unwrap().port();
    let location = format!("http://{ip}:{http_port}/description.xml");

    println!("reel fake TV");
    println!("  device:      reel fake TV ({UDN})");
    println!("  description: {location}");
    println!("  control:     http://{ip}:{http_port}/control");
    println!();
    println!("Enable Settings -> Casting in reel, then use the Cast button.");
    println!("Playback actions and media fetches are logged below. Ctrl-C to stop.");

    spawn_ssdp(location);

    axum::serve(listener, app).await.expect("serve the fake TV");
}

async fn description() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/xml")], DESCRIPTION)
}

async fn control(State(state): State<AppState>, body: Bytes) -> Response {
    let text = String::from_utf8_lossy(&body).into_owned();
    let action = action_name(&text);
    println!("AVTransport {action}");

    let mut tv = state.tv.lock().unwrap();
    match action.as_str() {
        "SetAVTransportURI" => {
            let uri = element_text(&text, "CurrentURI");
            tv.uri = uri.clone();
            tv.state = "STOPPED".to_string();
            tv.position = 0.0;
            tv.started = None;
            if let Some(uri) = uri {
                println!("  set URI: {uri}");
                probe_media(state.client.clone(), uri);
            }
            ok(&av_response("SetAVTransportURI", ""))
        }
        "Play" => {
            tv.state = "PLAYING".to_string();
            tv.started = Some(Instant::now());
            println!("  play: {}", tv.uri.as_deref().unwrap_or("(no URI)"));
            ok(&av_response("Play", ""))
        }
        "Pause" => {
            tv.freeze();
            tv.state = "PAUSED_PLAYBACK".to_string();
            ok(&av_response("Pause", ""))
        }
        "Stop" => {
            tv.freeze();
            tv.state = "STOPPED".to_string();
            tv.position = 0.0;
            ok(&av_response("Stop", ""))
        }
        "Seek" => {
            let target = element_text(&text, "Target")
                .and_then(|value| reel_cast::soap::parse_rel_time(&value));
            if let Some(target) = target {
                tv.position = target.as_secs_f64();
                if tv.state == "PLAYING" {
                    tv.started = Some(Instant::now());
                }
                println!("  seek to {}", reel_cast::soap::format_rel_time(target));
            }
            ok(&av_response("Seek", ""))
        }
        "GetTransportInfo" => {
            let inner = format!(
                "<CurrentTransportState>{}</CurrentTransportState>\
                 <CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>",
                tv.state
            );
            ok(&av_response("GetTransportInfo", &inner))
        }
        "GetPositionInfo" => {
            let rel = reel_cast::soap::format_rel_time(Duration::from_secs_f64(tv.position()));
            let inner = format!(
                "<Track>1</Track><TrackDuration>0:00:00</TrackDuration>\
                 <RelTime>{rel}</RelTime><AbsTime>{rel}</AbsTime><TrackURI>{}</TrackURI>",
                tv.uri.as_deref().unwrap_or_default()
            );
            ok(&av_response("GetPositionInfo", &inner))
        }
        "GetMediaInfo" => {
            let inner = format!(
                "<NrTracks>1</NrTracks><MediaDuration>0:00:00</MediaDuration><CurrentURI>{}</CurrentURI>",
                tv.uri.as_deref().unwrap_or_default()
            );
            ok(&av_response("GetMediaInfo", &inner))
        }
        other => ok(&av_response(other, "")),
    }
}

async fn volume(State(_state): State<AppState>, body: Bytes) -> Response {
    let text = String::from_utf8_lossy(&body);
    println!("RenderingControl {}", action_name(&text));
    ok(&format!("<u:SetVolumeResponse xmlns:u=\"{RENDERING}\"/>"))
}

/// Fetch the start of the media URL to prove the LAN surface is reachable, and
/// report what it said.
fn probe_media(client: Arc<reqwest::Client>, uri: String) {
    tokio::spawn(async move {
        let response = client
            .get(&uri)
            .header(header::RANGE, "bytes=0-65535")
            .send()
            .await;
        match response {
            Ok(response) => {
                let status = response.status();
                let get = |name: &str| {
                    response
                        .headers()
                        .get(name)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("-")
                        .to_string()
                };
                let content_type = get("content-type");
                let features = get("contentFeatures.dlna.org");
                let length = response.content_length();
                // Drain a little so we can confirm bytes actually flow.
                let read = response.bytes().await.map(|bytes| bytes.len()).unwrap_or(0);
                println!(
                    "  media OK: {status}, type={content_type}, length={length:?}, read={read} bytes"
                );
                println!("  dlna: {features}");
            }
            Err(error) => println!("  media fetch FAILED: {error}"),
        }
    });
}

/// Answer SSDP `M-SEARCH` so the app discovers the fake TV.
fn spawn_ssdp(location: String) {
    let spawned = std::thread::Builder::new()
        .name("fake-tv-ssdp".to_string())
        .spawn(move || {
            let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, SSDP_PORT)) {
                Ok(socket) => socket,
                Err(error) => {
                    eprintln!(
                        "SSDP: could not bind port {SSDP_PORT} ({error}).\n\
                         Discovery will not work; another UPnP daemon may hold it."
                    );
                    return;
                }
            };
            let _ = socket.join_multicast_v4(&MULTICAST_ADDR, &Ipv4Addr::UNSPECIFIED);
            let _ = socket.set_read_timeout(Some(Duration::from_secs(1)));
            println!("  SSDP: listening on {MULTICAST_ADDR}:{SSDP_PORT}");

            let mut buffer = [0u8; 4096];
            loop {
                let Ok((len, from)) = socket.recv_from(&mut buffer) else {
                    continue;
                };
                let message = String::from_utf8_lossy(&buffer[..len]);
                if !message.starts_with("M-SEARCH") {
                    continue;
                }
                let target = header_value(&message, "ST").unwrap_or_default();
                if target != MEDIA_RENDERER && target != "ssdp:all" && target != "upnp:rootdevice" {
                    continue;
                }
                let reply = format!(
                    "HTTP/1.1 200 OK\r\n\
                     CACHE-CONTROL: max-age=1800\r\n\
                     EXT:\r\n\
                     LOCATION: {location}\r\n\
                     SERVER: reel-fake-tv/1.0 UPnP/1.0\r\n\
                     ST: {MEDIA_RENDERER}\r\n\
                     USN: {UDN}::{MEDIA_RENDERER}\r\n\
                     \r\n"
                );
                let _ = socket.send_to(reply.as_bytes(), from);
            }
        });
    if spawned.is_err() {
        eprintln!("SSDP: could not start the responder thread");
    }
}

/// The primary outbound IPv4, without sending a packet.
fn local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(8, 8, 8, 8), 80)).ok()?;
    Some(socket.local_addr().ok()?.ip())
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

fn header_value(message: &str, name: &str) -> Option<String> {
    message.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
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
    (
        [(header::CONTENT_TYPE, "text/xml; charset=\"utf-8\"")],
        body,
    )
        .into_response()
}
