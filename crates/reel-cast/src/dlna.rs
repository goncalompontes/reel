//! DLNA content features and `protocolInfo`.
//!
//! `protocolInfo` is the four-field string DLNA receivers use to decide
//! whether they can play a resource: `http-get:*:<mime>:<content features>`.
//! The content-features field carries `DLNA.ORG_OP` (seek operations),
//! `DLNA.ORG_CI` (whether the bytes were converted) and `DLNA.ORG_FLAGS` (a
//! bitfield that must advertise "streaming" for many TVs to allow playback).

/// `DLNA.ORG_FLAGS` advertising a streamable, content-length-known resource.
///
/// Bit 24 (`0x01000000`) is `senderPaced`/streaming, which is what sets such as
/// Samsung's look for before they will play a URL at all.
pub const FLAGS_STREAMING: &str = "01700000000000000000000000000000";

/// `contentFeatures.dlna.org` response-header name.
pub const CONTENT_FEATURES_HEADER: &str = "contentFeatures.dlna.org";

/// `transferMode.dlna.org` response-header name.
pub const TRANSFER_MODE_HEADER: &str = "transferMode.dlna.org";

/// The `transferMode.dlna.org` value for streamed (not interactive) media.
pub const TRANSFER_MODE_STREAMING: &str = "Streaming";

/// Build a `protocolInfo` string.
///
/// `converted` should be true when the bytes were remuxed or transcoded, which
/// sets `DLNA.ORG_CI=1` and tells the receiver the content is not the original.
pub fn protocol_info(mime: &str, seekable: bool, converted: bool) -> String {
    format!(
        "http-get:*:{mime}:{}",
        content_features(seekable, converted)
    )
}

/// Build the content-features field (also the body of the
/// `contentFeatures.dlna.org` header).
pub fn content_features(seekable: bool, converted: bool) -> String {
    // DLNA.ORG_OP: bit 1 = byte-range seek supported by the server.
    let op = if seekable { "01" } else { "00" };
    let ci = if converted { "1" } else { "0" };
    format!("DLNA.ORG_OP={op};DLNA.ORG_CI={ci};DLNA.ORG_FLAGS={FLAGS_STREAMING}")
}

/// Best-effort MIME type from a URL's file extension.
///
/// Renderers are forgiving about `*`, but advertising the real type in
/// `protocolInfo` avoids "format not supported" refusals on stricter sets.
pub fn mime_for_url(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let extension = path.rsplit('.').next().unwrap_or_default();
    mime_for_container(extension)
}

/// Best-effort MIME type from a container name or file extension.
pub fn mime_for_container(container: &str) -> &'static str {
    match container.to_ascii_lowercase().as_str() {
        "mp4" | "m4v" | "mov" => "video/mp4",
        "m4a" | "aac" => "audio/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "ts" | "m2ts" | "mpegts" => "video/mp2t",
        "mpg" | "mpeg" => "video/mpeg",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_info_carries_the_features() {
        let info = protocol_info("video/mp4", true, false);
        assert_eq!(
            info,
            "http-get:*:video/mp4:DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000"
        );
    }

    #[test]
    fn converted_and_unseekable_change_the_features() {
        assert!(content_features(true, true).contains("DLNA.ORG_OP=01"));
        assert!(content_features(true, true).contains("DLNA.ORG_CI=1"));
        assert!(content_features(false, false).contains("DLNA.ORG_OP=00"));
    }

    #[test]
    fn infers_mime_from_urls_and_containers() {
        assert_eq!(mime_for_url("http://h/a/b.mkv"), "video/x-matroska");
        assert_eq!(mime_for_url("http://h/stream.mp4?token=abc"), "video/mp4");
        assert_eq!(mime_for_container("MP4"), "video/mp4");
        assert_eq!(mime_for_container("weird"), "application/octet-stream");
    }
}
