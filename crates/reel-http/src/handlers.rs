use std::convert::Infallible;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::IntervalStream;
use tokio_util::io::ReaderStream;
use tracing::{debug, info, warn};

use reel_core::model::AddOutcome;
use reel_core::{AddOptions, AddRequest, AddSource, FileView, StatsView, TorrentView};

use crate::error::HttpError;
use crate::range;
use crate::router::AppState;

/// Read buffer per stream chunk. 64 KiB keeps syscall count low without
/// holding pieces in memory for long.
const STREAM_CHUNK: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
pub struct DeleteQuery {
    #[serde(default)]
    pub files: bool,
}

#[derive(Debug, Deserialize)]
pub struct SetFilesBody {
    pub only_files: Vec<usize>,
}

/// Options for raw `.torrent` uploads, which have no JSON body to carry them.
#[derive(Debug, Default, Deserialize)]
pub struct AddQuery {
    #[serde(default)]
    pub media_only: Option<bool>,
    #[serde(default)]
    pub paused: Option<bool>,
    #[serde(default)]
    pub allow_overwrite: Option<bool>,
}

// ------------------------------------------------------------------ metadata

pub async fn health(State(st): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "version": crate::VERSION,
        "download_dir": st.engine.config().download_dir,
        "base_url": st.base_url,
    }))
}

pub async fn get_config(State(st): State<AppState>) -> impl IntoResponse {
    let cfg = st.engine.config();
    Json(json!({
        "version": crate::VERSION,
        "download_dir": cfg.download_dir,
        "persist_session": cfg.persist_session,
        "client_name": cfg.client_name,
        "ipv4_only": cfg.ipv4_only,
        "disable_dht": cfg.disable_dht,
        "disable_trackers": cfg.disable_trackers,
        "listen_port": cfg.listen_port,
        "base_url": st.base_url,
    }))
}

// ------------------------------------------------------------------ torrents

pub async fn list_torrents(State(st): State<AppState>) -> Json<Vec<TorrentView>> {
    let mut views = st.engine.list();
    for v in &mut views {
        v.with_base_url(&st.base_url);
    }
    Json(views)
}

/// Add a torrent.
///
/// Accepts either a JSON body ([`AddRequest`]) or a raw `.torrent` file posted
/// with `Content-Type: application/x-bittorrent`.
pub async fn add_torrent(
    State(st): State<AppState>,
    Query(q): Query<AddQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HttpError> {
    if body.is_empty() {
        return Err(HttpError::bad_request(
            "empty body: send JSON {\"source\": \"magnet:...\"} or a raw .torrent file",
        ));
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let is_torrent_upload = content_type.contains("application/x-bittorrent")
        || content_type.contains("application/octet-stream");

    let (source, opts) = if is_torrent_upload {
        (
            AddSource::File(body.to_vec()),
            AddOptions {
                media_only: q.media_only.unwrap_or(true),
                paused: q.paused.unwrap_or(false),
                allow_overwrite: q.allow_overwrite.unwrap_or(false),
                ..Default::default()
            },
        )
    } else {
        let req: AddRequest = serde_json::from_slice(&body)
            .map_err(|e| HttpError::bad_request(format!("invalid JSON body: {e}")))?;

        let source = req.source.trim().to_string();
        if !looks_like_source(&source) {
            return Err(HttpError::bad_request(
                "source must be a magnet: URI, an http(s):// .torrent URL, or a 40-character info hash",
            ));
        }

        let mut initial_peers = Vec::new();
        for peer in req.initial_peers.unwrap_or_default() {
            let addr = peer
                .parse()
                .map_err(|_| HttpError::bad_request(format!("invalid peer address `{peer}`")))?;
            initial_peers.push(addr);
        }

        (
            AddSource::Url(source),
            AddOptions {
                media_only: req.media_only.unwrap_or(true),
                paused: req.paused.unwrap_or(false),
                output_folder: req.output_folder,
                allow_overwrite: req.allow_overwrite.unwrap_or(false),
                upload_limit_bps: req.upload_limit_bps,
                download_limit_bps: req.download_limit_bps,
                initial_peers,
            },
        )
    };

    info!("adding torrent via API");
    let outcome = st.engine.add(source, opts).await?;
    let status = if outcome.was_new {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };

    let mut view = outcome.torrent;
    view.with_base_url(&st.base_url);
    Ok((
        status,
        Json(AddOutcome {
            torrent: view,
            was_new: outcome.was_new,
        }),
    )
        .into_response())
}

pub async fn get_torrent(
    State(st): State<AppState>,
    Path(id): Path<usize>,
) -> Result<Json<TorrentView>, HttpError> {
    let mut view = st.engine.view(id)?;
    view.with_base_url(&st.base_url);
    Ok(Json(view))
}

pub async fn get_files(
    State(st): State<AppState>,
    Path(id): Path<usize>,
) -> Result<Json<Vec<FileView>>, HttpError> {
    let view = st.engine.view(id)?;
    Ok(Json(view.files))
}

pub async fn set_files(
    State(st): State<AppState>,
    Path(id): Path<usize>,
    Json(body): Json<SetFilesBody>,
) -> Result<Json<TorrentView>, HttpError> {
    if body.only_files.is_empty() {
        return Err(HttpError::bad_request("only_files must not be empty"));
    }
    st.engine.set_only_files(id, &body.only_files).await?;
    let mut view = st.engine.view(id)?;
    view.with_base_url(&st.base_url);
    Ok(Json(view))
}

pub async fn delete_torrent(
    State(st): State<AppState>,
    Path(id): Path<usize>,
    Query(q): Query<DeleteQuery>,
) -> Result<StatusCode, HttpError> {
    st.engine.remove(id, q.files).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn pause_torrent(
    State(st): State<AppState>,
    Path(id): Path<usize>,
) -> Result<Json<StatsView>, HttpError> {
    st.engine.pause(id).await?;
    stats_for(&st, id)
}

pub async fn resume_torrent(
    State(st): State<AppState>,
    Path(id): Path<usize>,
) -> Result<Json<StatsView>, HttpError> {
    st.engine.resume(id).await?;
    stats_for(&st, id)
}

pub async fn get_stats(
    State(st): State<AppState>,
    Path(id): Path<usize>,
) -> Result<Json<StatsView>, HttpError> {
    stats_for(&st, id)
}

fn stats_for(st: &AppState, id: usize) -> Result<Json<StatsView>, HttpError> {
    st.engine
        .stats(id)
        .map(Json)
        .ok_or_else(|| HttpError::not_found(format!("torrent {id} is not in the session")))
}

/// Server-sent events: a full snapshot of every torrent once per second.
///
/// This is what keeps the GUI's progress bars and peer counts live without
/// polling from the frontend.
pub async fn events(State(st): State<AppState>) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    // Never burst to "catch up" after a stalled client.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let stream = IntervalStream::new(ticker).map(move |_| {
        let mut views = st.engine.list();
        for v in &mut views {
            v.with_base_url(&st.base_url);
        }
        let event = match Event::default().event("torrents").json_data(&views) {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "failed to serialise torrent snapshot");
                Event::default().event("error").data("serialisation failed")
            }
        };
        Ok::<_, Infallible>(event)
    });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

// ---------------------------------------------------------------- streaming

pub async fn stream_file(
    State(st): State<AppState>,
    Path((id, file_id)): Path<(usize, usize)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    serve_file(st, id, file_id, &headers).await
}

/// Same as [`stream_file`], but with a cosmetic file name as the last path
/// segment so players and "save as" dialogs show something human.
pub async fn stream_file_named(
    State(st): State<AppState>,
    Path((id, file_id, _name)): Path<(usize, usize, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    serve_file(st, id, file_id, &headers).await
}

async fn serve_file(
    st: AppState,
    id: usize,
    file_id: usize,
    headers: &HeaderMap,
) -> Result<Response, HttpError> {
    let probe = st.engine.probe_file(id, file_id)?;

    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let parsed = match range::parse_range(range_header, probe.length) {
        Ok(r) => r,
        Err(range::RangeError::Unsatisfiable) => {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{}", probe.length))],
            )
                .into_response());
        }
        Err(range::RangeError::Malformed) => None,
    };

    let (status, offset, length) = match parsed {
        Some(r) => (StatusCode::PARTIAL_CONTENT, r.start, r.len()),
        None => (StatusCode::OK, 0, probe.length),
    };

    debug!(
        torrent_id = id,
        file_id,
        %probe.name,
        offset,
        length,
        "serving file bytes"
    );

    let stream = st.engine.stream_from(id, file_id, offset).await?;

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, probe.mime.as_str())
        .header(header::CONTENT_LENGTH, length.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "no-store")
        .header(
            header::CONTENT_DISPOSITION,
            format!("inline; filename=\"{}\"", sanitize_filename(&probe.name)),
        );

    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", offset, offset + length - 1, probe.length),
        );
    }

    let body = Body::from_stream(ReaderStream::with_capacity(
        stream.take(length),
        STREAM_CHUNK,
    ));

    builder
        .body(body)
        .map_err(|e| HttpError::internal(format!("building response: {e}")))
}

fn sanitize_filename(name: &str) -> String {
    name.replace(['"', '\\', '\n', '\r', '/'], "_")
}

fn looks_like_source(source: &str) -> bool {
    source.starts_with("magnet:")
        || source.starts_with("http://")
        || source.starts_with("https://")
        || (source.len() == 40 && source.chars().all(|c| c.is_ascii_hexdigit()))
}

// --------------------------------------------------------------------- index

/// A tiny human-facing page. Not the GUI — just enough to click a title and
/// watch it stream in a browser or hand the URL to `mpv`.
pub async fn index(State(st): State<AppState>) -> Html<String> {
    let mut html = String::from(
        r#"<!doctype html><html><head><meta charset="utf-8">
<title>reel</title>
<style>
 body{font:15px/1.5 system-ui,sans-serif;margin:2rem auto;max-width:960px;background:#101014;color:#e8e8ef}
 h1{font-weight:600} a{color:#7cc4ff} code{color:#9ef}
 li{margin:.4rem 0} .muted{color:#8a8a99} table{border-collapse:collapse;width:100%}
 td,th{text-align:left;padding:.35rem .6rem;border-bottom:1px solid #22222c}
</style></head><body><h1>reel</h1>"#,
    );

    let mut views = st.engine.list();
    for v in &mut views {
        v.with_base_url(&st.base_url);
    }

    if views.is_empty() {
        html.push_str("<p class=\"muted\">No torrents yet. POST one to <code>/api/torrents</code>:</p>");
        html.push_str("<pre><code>curl -s localhost:3030/api/torrents -H 'content-type: application/json' \\\n  -d '{\"source\":\"magnet:?xt=urn:btih:...\"}'</code></pre>");
    } else {
        html.push_str("<table><tr><th>#</th><th>Title</th><th>State</th><th>Progress</th><th>Play</th></tr>");
        for v in &views {
            let name = v.name.clone().unwrap_or_else(|| v.info_hash.clone());
            html.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{:.1}%</td><td>{}</td></tr>",
                v.id,
                escape_html(&name),
                escape_html(&v.state),
                v.stats.percent,
                match v.primary_file() {
                    Some(f) => format!(
                        "<a href=\"{}\">{}</a>",
                        escape_html(&f.stream.path),
                        escape_html(&f.name)
                    ),
                    None => "<span class=\"muted\">no playable file</span>".to_string(),
                }
            ));
        }
        html.push_str("</table>");
    }

    html.push_str("</body></html>");
    Html(html)
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
