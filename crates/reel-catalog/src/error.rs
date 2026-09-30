//! Errors from the catalog layer.

/// Anything that can go wrong while resolving metadata.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// No provider is configured, so nothing can be looked up.
    #[error("no metadata provider is configured")]
    NotConfigured,
    /// The provider answered, but with an error (bad key, rate limit, ...).
    #[error("metadata provider error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),
    /// The provider's response did not look like what we expect.
    #[error("could not read the provider's response: {0}")]
    Decode(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl CatalogError {
    /// A short message suitable for a status line in the UI.
    pub fn user_message(&self) -> String {
        match self {
            CatalogError::NotConfigured => {
                "No metadata provider configured. Add a TMDB API key in Settings to get \
                 posters and synopses."
                    .to_string()
            }
            CatalogError::Api { status: 401, .. } => {
                "The metadata provider rejected the API key (401). Check it in Settings."
                    .to_string()
            }
            CatalogError::Api { status: 429, .. } => {
                "The metadata provider is rate limiting us. Try again shortly.".to_string()
            }
            other => other.to_string(),
        }
    }

    /// Whether retrying later could plausibly help.
    pub fn is_transient(&self) -> bool {
        match self {
            CatalogError::Http(e) => e.is_timeout() || e.is_connect(),
            CatalogError::Api { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_failures_are_explained_in_plain_language() {
        let error = CatalogError::Api {
            status: 401,
            message: "Invalid API key".into(),
        };
        assert!(error.user_message().contains("API key"));
        // A bad key is not worth retrying.
        assert!(!error.is_transient());

        let rate_limited = CatalogError::Api {
            status: 429,
            message: "slow down".into(),
        };
        assert!(rate_limited.is_transient());

        let server_error = CatalogError::Api {
            status: 503,
            message: "later".into(),
        };
        assert!(server_error.is_transient());
    }
}
