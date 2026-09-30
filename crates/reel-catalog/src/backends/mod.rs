//! Search backends that ship with reel.
//!
//! There is exactly one, and it was chosen deliberately: `archive_org` indexes
//! the Internet Archive, which serves public-domain and Creative Commons film.
//! It is here to do two jobs — make search usable out of the box for material
//! anyone may share, and serve as the reference implementation of
//! [`crate::search::SearchBackend`] for anything else you want to plug in.
//!
//! Everything about how to add your own lives in `docs/ADDING_A_SOURCE.md`.

pub mod archive_org;

pub use archive_org::ArchiveOrgBackend;
