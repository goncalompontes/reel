//! Search backends that ship with reel.
//!
//! `ext_to` scrapes ext.to, a general torrent index behind a Cloudflare
//! interactive challenge, which [`solver`] clears with the FlareSolverr
//! installed beside the binary. New sources implement
//! [`crate::search::SearchBackend`].

pub mod ext_to;
pub mod solver;

pub use ext_to::ExtToBackend;

use crate::search::SearchBackend;
use crate::settings::CatalogSettings;

/// The bundled source a settings file asks for.
///
/// Settings are canonical, so this reads nothing from the environment; each
/// source is registered only when it is switched on, and ext.to carries the
/// cookie / user-agent / solver the user saved.
pub fn bundled(settings: &CatalogSettings) -> Vec<Box<dyn SearchBackend>> {
    let mut backends: Vec<Box<dyn SearchBackend>> = Vec::new();
    if settings.ext_to_enabled {
        backends.push(Box::new(ExtToBackend::from_settings(settings)));
    }
    backends
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::CatalogSettings;

    fn names(backends: &[Box<dyn SearchBackend>]) -> Vec<&str> {
        backends.iter().map(|backend| backend.name()).collect()
    }

    #[test]
    fn bundled_registers_only_ext_to_when_enabled() {
        assert_eq!(names(&bundled(&CatalogSettings::default())), ["ext.to"]);
        assert!(bundled(&CatalogSettings {
            ext_to_enabled: false,
            ..Default::default()
        })
        .is_empty());
    }
}
