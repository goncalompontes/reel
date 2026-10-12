//! End-to-end cast: the real engine, the real LAN media server, and a fake DLNA
//! renderer, driven through `EngineBackend::start_cast`.
//!
//! This proves the whole path: ffprobe -> plan -> LAN media URL -> DIDL metadata
//! -> `SetAVTransportURI`/`Play`, then the renderer fetching the bytes from the
//! LAN server. It also checks that a renderer's capabilities are queried once,
//! not per media switch.
//!
//! The torrent is seeded locally (a second engine) and streamed by a leech
//! backend, the same shape `app_player_resume` uses, so the cast has real bytes
//! to serve. Skipped when ffmpeg/ffprobe (or libx264) are not installed.

use std::net::SocketAddr;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use reel_catalog::CatalogSettings;
use reel_core::{AddOptions, AddSource, Engine, EngineConfig};
use reel_desktop::backend::{Backend, CatalogOptions, EngineBackend};
use reel_desktop::cast::CastStatus;

#[derive(Default)]
struct Renderer {
    uri: Option<String>,
    state: String,
    actions: Vec<String>,
    capability_queries: usize,
}
type Shared = Arc<Mutex<Renderer>>;

const DESCRIPTION: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>Fake TV</friendlyName>
    <UDN>uuid:fake-tv</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <controlURL>/control</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
        <controlURL>/volume</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>
        <controlURL>/connection</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

const AV: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const CM: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";
const RC: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

async fn start_renderer() -> (SocketAddr, Shared) {
    let state: Shared = Arc::new(Mutex::new(Renderer {
        state: "STOPPED".to_string(),
        ..Default::default()
    }));
    let app = Router::new()
        .route(
            "/description.xml",
            get(|| async { ([(header::CONTENT_TYPE, "text/xml")], DESCRIPTION) }),
        )
        .route("/control", post(control))
        .route("/connection", post(connection))
        .route("/volume", post(volume))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, state)
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
            ok(&av_response("Stop", ""))
        }
        "GetTransportInfo" => {
            let inner = format!(
                "<CurrentTransportState>{}</CurrentTransportState>\
                 <CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>",
                renderer.state
            );
            ok(&av_response("GetTransportInfo", &inner))
        }
        "GetPositionInfo" => ok(&av_response(
            "GetPositionInfo",
            "<Track>1</Track><TrackDuration>0:00:30</TrackDuration>\
             <RelTime>0:00:00</RelTime><AbsTime>0:00:00</AbsTime>",
        )),
        other => ok(&av_response(other, "")),
    }
}

async fn connection(State(state): State<Shared>, body: Bytes) -> Response {
    let action = action_name(&String::from_utf8_lossy(&body));
    let mut renderer = state.lock().unwrap();
    renderer.actions.push(action.clone());
    renderer.capability_queries += 1;
    ok(&format!(
        "<u:GetProtocolInfoResponse xmlns:u=\"{CM}\">\
         <Source></Source>\
         <Sink>http-get:*:video/mp4:*,http-get:*:video/x-matroska:*</Sink>\
         </u:GetProtocolInfoResponse>"
    ))
}

async fn volume(State(state): State<Shared>, _body: Bytes) -> Response {
    state.lock().unwrap().actions.push("SetVolume".to_string());
    ok(&format!("<u:SetVolumeResponse xmlns:u=\"{RC}\"/>"))
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
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
         <s:Body>{response_element}</s:Body></s:Envelope>"
    );
    (
        [(header::CONTENT_TYPE, "text/xml; charset=\"utf-8\"")],
        body,
    )
        .into_response()
}

fn scratch_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("reel-cast-e2e-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn has(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

/// Wait for the cast to become active, failing loudly on a reported failure.
fn wait_for_cast(backend: &EngineBackend) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        match backend.cast_status() {
            Some(CastStatus::Active { .. }) => return,
            Some(CastStatus::Failed { message }) => panic!("cast failed: {message}"),
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    panic!(
        "timed out waiting for the cast to become active (last status: {:?})",
        backend.cast_status()
    );
}

/// Fetch a URL with a hand-written request, so the test needs no HTTP client.
fn get_url(runtime: &tokio::runtime::Runtime, url: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    runtime.block_on(async {
        let rest = url.strip_prefix("http://").expect("http url");
        let (host, path) = rest.split_once('/').expect("host/path");
        let mut stream = tokio::net::TcpStream::connect(host).await.expect("connect");
        stream
            .write_all(
                format!("GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut buffer = Vec::new();
        stream.read_to_end(&mut buffer).await.unwrap();
        String::from_utf8_lossy(&buffer).into_owned()
    })
}

#[test]
fn a_cast_reaches_the_renderer_and_serves_media() {
    if !has("ffmpeg") || !has("ffprobe") {
        eprintln!("skipping: ffmpeg/ffprobe are not installed");
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    // A runtime for the fake renderer and the media fetch. The backend starts
    // its own runtime, so it must not be called from inside one.
    let runtime = tokio::runtime::Runtime::new().unwrap();

    let dir = scratch_dir("media");
    std::fs::create_dir_all(&dir).unwrap();
    let video = dir.join("Movie.mp4");
    // A small H.264/AAC MP4, faststart so ffprobe can read it forward-only.
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x48:rate=5",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440",
            "-t",
            "3",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-movflags",
            "+faststart",
            "-y",
        ])
        .arg(&video)
        .status()
        .expect("spawn ffmpeg");
    if !status.success() {
        eprintln!("skipping: ffmpeg has no libx264");
        return;
    }

    // One file, seeded locally, so the leech can actually stream it.
    let seed_dir = dir.join("seed");
    std::fs::create_dir_all(&seed_dir).unwrap();
    std::fs::copy(&video, seed_dir.join("Movie.mp4")).unwrap();
    let created = runtime
        .block_on(reel_core::create_torrent_file(
            &seed_dir,
            None,
            Vec::new(),
            Some(16 * 1024),
        ))
        .expect("create torrent");
    let torrent_path = dir.join("movie.torrent");
    std::fs::write(&torrent_path, &created.bytes).unwrap();

    let (addr, renderer) = runtime.block_on(start_renderer());

    // A seeder with the data, so the leech has a peer to stream from.
    let seed_engine = runtime.block_on(async {
        let engine = Engine::new(EngineConfig {
            disable_dht: true,
            disable_trackers: true,
            persist_session: false,
            disable_upload: false,
            ..EngineConfig::new(&dir)
        })
        .await
        .expect("seed engine");
        engine
            .add(
                AddSource::File(created.bytes.clone()),
                AddOptions {
                    media_only: false,
                    allow_overwrite: true,
                    ..Default::default()
                },
            )
            .await
            .expect("seed add");
        engine
    });
    wait_until("the seeder to have the data", || {
        seed_engine.list().iter().any(|view| view.finished)
    });
    let seed_addr = seed_engine.session().listen_addr().expect("seed address");

    let settings = CatalogSettings {
        allow_lan_streaming: true,
        // Loopback, so the fake renderer (also loopback) can fetch the media.
        cast_lan_address: Some("127.0.0.1".to_string()),
        cast_media_port: 0,
        ..CatalogSettings::default()
    };

    let leech = dir.join("leech");
    let scratch = scratch_dir("state");
    let backend = EngineBackend::start_with_options(
        EngineConfig {
            disable_dht: true,
            disable_trackers: true,
            persist_session: false,
            initial_peers: vec![seed_addr],
            stream_scratch_dir: Some(scratch.clone()),
            ..EngineConfig::new(&leech)
        },
        CatalogOptions {
            api_key: None,
            settings,
            data_dir: dir.join("catalog"),
            disable_bundled_sources: true,
        },
    )
    .expect("start the engine backend");

    assert!(
        backend.cast_unavailable().is_none(),
        "casting must be available: {:?}",
        backend.cast_unavailable()
    );

    backend.add(torrent_path.to_str().unwrap(), true);
    wait_until("the title to appear", || !backend.library().is_empty());
    let item = backend.library().into_iter().next().unwrap();
    let torrent_id = item.torrent.id;
    let info_hash = item.torrent.info_hash.clone();
    let file_id = item
        .torrent
        .files
        .iter()
        .find(|file| file.name.ends_with(".mp4"))
        .map(|file| file.id)
        .expect("the mp4 file");

    backend.add_cast_device(format!("127.0.0.1:{}", addr.port()));
    wait_until("the renderer to appear", || {
        backend.cast_devices().iter().any(|d| d.udn == "uuid:fake-tv")
    });

    backend.start_cast(
        "uuid:fake-tv".to_string(),
        torrent_id,
        info_hash.clone(),
        file_id,
        "Movie".to_string(),
        None,
        false,
        None,
    );
    wait_for_cast(&backend);

    // The renderer was pointed at a LAN media URL and told to play it.
    let uri = {
        let renderer = renderer.lock().unwrap();
        assert!(
            renderer.actions.iter().any(|a| a == "SetAVTransportURI"),
            "actions: {:?}",
            renderer.actions
        );
        assert!(
            renderer.actions.iter().any(|a| a == "Play"),
            "actions: {:?}",
            renderer.actions
        );
        renderer.uri.clone().expect("a media URL")
    };
    assert!(
        uri.contains("/media/"),
        "the URL should be the LAN surface: {uri}"
    );

    // The bytes the renderer would fetch really are served, with DLNA headers.
    let response = get_url(&runtime, &uri);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "the renderer could not fetch the media:\n{}",
        response.lines().next().unwrap_or_default()
    );
    assert!(
        response.to_ascii_lowercase().contains("contentfeatures.dlna.org"),
        "the DLNA headers must be present:\n{}",
        response.lines().take(12).collect::<Vec<_>>().join("\n")
    );

    // Switching media must not re-query the renderer's capabilities.
    assert_eq!(
        renderer.lock().unwrap().capability_queries,
        1,
        "capabilities are queried once per device"
    );
    backend.start_cast(
        "uuid:fake-tv".to_string(),
        torrent_id,
        info_hash,
        file_id,
        "Movie again".to_string(),
        None,
        false,
        None,
    );
    wait_for_cast(&backend);
    assert_eq!(
        renderer.lock().unwrap().capability_queries,
        1,
        "switching media must reuse the memoized capabilities"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&scratch);
}
