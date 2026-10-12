//! The LAN-facing media surface used for casting.
//!
//! This is a second, deliberately small router — separate from the loopback
//! API. It serves **only** media bytes, requires a per-run token in the path
//! (so only a device we handed a URL to can fetch), and adds the DLNA response
//! headers that make TVs willing to stream and seek.
//!
//! The token and the DLNA `contentFeatures` value are supplied by the caller
//! (the desktop), which keeps this crate free of casting-protocol knowledge: it
//! only builds HTTP responses.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use reel_core::Engine;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::error::HttpError;
use crate::handlers::sanitize_filename;
use crate::range;

/// Read buffer per streamed chunk.
const STREAM_CHUNK: usize = 64 * 1024;

/// `transferMode.dlna.org`: streamed (not interactive) media.
const TRANSFER_MODE: &str = "Streaming";

/// State for the LAN media router.
#[derive(Clone)]
pub struct MediaState {
    /// The engine that owns the download.
    pub engine: Arc<Engine>,
    /// Per-run bearer token; every media URL carries it.
    pub token: String,
    /// Where converted files live (named by the conversion cache key).
    pub converted_dir: PathBuf,
    /// Value for `contentFeatures.dlna.org` (built by the caller from the
    /// casting layer, e.g. `reel_cast::dlna::content_features`).
    pub content_features: String,
}

impl MediaState {
    /// Assemble the media state.
    pub fn new(
        engine: Arc<Engine>,
        token: impl Into<String>,
        converted_dir: impl Into<PathBuf>,
        content_features: impl Into<String>,
    ) -> MediaState {
        MediaState {
            engine,
            token: token.into(),
            converted_dir: converted_dir.into(),
            content_features: content_features.into(),
        }
    }

    /// Whether a request token matches, in constant time.
    fn authorized(&self, token: &str) -> bool {
        tokens_match(&self.token, token)
    }
}

/// Compare two tokens without an early-exit, so timing does not leak the value.
fn tokens_match(expected: &str, given: &str) -> bool {
    let expected = expected.as_bytes();
    let given = given.as_bytes();
    if expected.is_empty() || expected.len() != given.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in expected.iter().zip(given) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Build the LAN media router.
pub fn build_media_router(state: MediaState) -> Router {
    Router::new()
        .route("/media/{token}/original/{id}/{file_id}", get(original))
        .route("/media/{token}/converted/{name}", get(converted))
        .with_state(state)
}

/// Serve an original downloaded file, honouring `Range`.
async fn original(
    State(st): State<MediaState>,
    AxumPath((token, id, file_id)): AxumPath<(String, usize, usize)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    if !st.authorized(&token) {
        return Err(HttpError::not_found("no such media"));
    }

    let probe = st.engine.probe_file(id, file_id)?;
    let total = probe.length;

    let Some((status, offset, length)) = resolve_range(&headers, total)? else {
        return Ok(unsatisfiable(total));
    };

    let stream = st.engine.stream_from(id, file_id, offset).await?;
    let body = Body::from_stream(ReaderStream::with_capacity(
        stream.take(length),
        STREAM_CHUNK,
    ));
    Ok(respond(
        &st.content_features,
        status,
        total,
        offset,
        length,
        &probe.mime,
        &probe.name,
        body,
    ))
}

/// Serve a converted file from the cache directory, honouring `Range`.
async fn converted(
    State(st): State<MediaState>,
    AxumPath((token, name)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    if !st.authorized(&token) {
        return Err(HttpError::not_found("no such media"));
    }

    let path = converted_path(&st.converted_dir, &name)?;
    let not_converted = || HttpError::not_found("that file has not been converted");
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|_| not_converted())?;
    if !metadata.is_file() {
        return Err(not_converted());
    }
    let total = metadata.len();

    let Some((status, offset, length)) = resolve_range(&headers, total)? else {
        return Ok(unsatisfiable(total));
    };

    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| not_converted())?;
    file.seek(SeekFrom::Start(offset))
        .await
        .map_err(|error| HttpError::internal(format!("seeking converted file: {error}")))?;
    let body = Body::from_stream(ReaderStream::with_capacity(file.take(length), STREAM_CHUNK));
    Ok(respond(
        &st.content_features,
        status,
        total,
        offset,
        length,
        "video/mp4",
        &name,
        body,
    ))
}

/// `(status, offset, length)`, or `None` for an unsatisfiable range.
#[allow(clippy::type_complexity)]
fn resolve_range(
    headers: &HeaderMap,
    total: u64,
) -> Result<Option<(StatusCode, u64, u64)>, HttpError> {
    let header = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    Ok(match range::parse_range(header, total) {
        Ok(Some(range)) => Some((StatusCode::PARTIAL_CONTENT, range.start, range.len())),
        Ok(None) => Some((StatusCode::OK, 0, total)),
        Err(range::RangeError::Unsatisfiable) => None,
        // A malformed header is ignored, per RFC 9110.
        Err(range::RangeError::Malformed) => Some((StatusCode::OK, 0, total)),
    })
}

fn unsatisfiable(total: u64) -> Response {
    (
        StatusCode::RANGE_NOT_SATISFIABLE,
        [(header::CONTENT_RANGE, format!("bytes */{total}"))],
    )
        .into_response()
}

#[allow(clippy::too_many_arguments)]
fn respond(
    content_features: &str,
    status: StatusCode,
    total: u64,
    offset: u64,
    length: u64,
    mime: &str,
    name: &str,
    body: Body,
) -> Response {
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, length.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "no-store")
        .header(
            header::CONTENT_DISPOSITION,
            format!("inline; filename=\"{}\"", sanitize_filename(name)),
        )
        .header("contentFeatures.dlna.org", content_features)
        .header("transferMode.dlna.org", TRANSFER_MODE);

    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", offset, offset + length - 1, total),
        );
    }

    builder.body(body).unwrap_or_else(|error| {
        HttpError::internal(format!("building response: {error}")).into_response()
    })
}

/// Resolve a converted file name against the cache directory, refusing
/// anything that could escape it.
fn converted_path(dir: &Path, name: &str) -> Result<PathBuf, HttpError> {
    let safe = !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        && !name.contains("..");
    if !safe {
        return Err(HttpError::not_found("no such media"));
    }
    Ok(dir.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_compared_exactly() {
        assert!(tokens_match("secret", "secret"));
        assert!(!tokens_match("secret", "secre"));
        assert!(!tokens_match("secret", "Secret"));
        assert!(!tokens_match("secret", ""));
    }

    #[test]
    fn an_empty_token_authorizes_nothing() {
        assert!(!tokens_match("", ""));
        assert!(!tokens_match("", "anything"));
    }

    #[test]
    fn converted_names_cannot_escape_the_directory() {
        let dir = Path::new("/cache");
        assert_eq!(
            converted_path(dir, "abc-0-1-remux.mp4").unwrap(),
            PathBuf::from("/cache/abc-0-1-remux.mp4")
        );
        assert!(converted_path(dir, "../secret").is_err());
        assert!(converted_path(dir, "a/b.mp4").is_err());
        assert!(converted_path(dir, "a\\b.mp4").is_err());
        assert!(converted_path(dir, "").is_err());
    }
}
