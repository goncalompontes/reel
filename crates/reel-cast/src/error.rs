//! Errors for the casting crate.

use thiserror::Error;
use url::Url;

/// Result alias for casting operations.
pub type Result<T, E = CastError> = std::result::Result<T, E>;

/// Anything that can go wrong talking to a renderer.
#[derive(Debug, Error)]
pub enum CastError {
    /// A socket or file operation failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// A document was not well-formed, or a field could not be read.
    #[error("xml error: {0}")]
    Xml(String),

    /// An SSDP datagram was not a valid discovery message.
    #[error("malformed SSDP message: {0}")]
    Ssdp(String),

    /// A URL could not be parsed or resolved.
    #[error("invalid url {url:?}: {message}")]
    Url {
        /// The offending string.
        url: String,
        /// Why it failed.
        message: String,
    },

    /// A required element or attribute was absent.
    #[error("missing {0}")]
    Missing(String),

    /// The device answered with a UPnP fault.
    #[error("upnp fault {code}: {description}", code = .code.map_or_else(|| "?".to_string(), |code| code.to_string()))]
    Fault {
        /// Numeric UPnP error code, when the device provided one.
        code: Option<i32>,
        /// Human-readable description.
        description: String,
    },

    /// The renderer did not reach an expected state in time.
    #[error("the renderer did not {expected} (it reports {reported})")]
    Stuck {
        /// What was expected, e.g. `stop`.
        expected: &'static str,
        /// The transport state the renderer actually reported.
        reported: String,
    },

    /// A discovery search failed.
    #[error("discovery failed: {0}")]
    Discovery(String),

    /// A request to the device failed at the transport level.
    #[error("request to the device failed: {0}")]
    Request(#[from] reqwest::Error),

    /// The device answered with a non-success HTTP status and no SOAP fault.
    #[error("device returned HTTP {status}: {body}")]
    HttpStatus {
        /// HTTP status code.
        status: u16,
        /// Response body, for context.
        body: String,
    },
}

impl CastError {
    /// Wrap a URL error with the string that produced it.
    pub fn url(value: impl Into<String>, error: impl std::fmt::Display) -> Self {
        CastError::Url {
            url: value.into(),
            message: error.to_string(),
        }
    }

    /// Parse a URL with context on failure.
    pub fn parse_url(value: &str) -> Result<Url> {
        Url::parse(value).map_err(|error| CastError::url(value, error))
    }
}
