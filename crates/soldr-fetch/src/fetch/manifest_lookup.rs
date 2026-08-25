//! Public compatibility facade for the toolchain catalogue lookup API.
//!
//! The implementation is split by concern: model/binding validation,
//! multipart transport and cache materialization, and network lookup.

#[path = "catalogue_lookup.rs"]
mod catalogue_lookup;
#[path = "catalogue_model.rs"]
mod catalogue_model;
#[path = "catalogue_transport.rs"]
mod catalogue_transport;

// Historical test paths remain crate-private while implementation concerns
// live in their own sibling modules.
#[cfg(test)]
pub(crate) use super::manifest_v6;
pub(crate) use super::{retry, segmented_download, stream_download, trust};

pub use catalogue_lookup::*;
pub use catalogue_model::*;
pub(crate) use catalogue_transport::materialize_catalogue_entry;

/// Return the URL Soldr will actually request for this catalogue entry.
///
/// Callers may use a legacy source URL only as an identity key when resolving
/// an entry. Progress output must use this label so a multipart download never
/// misleadingly reports that it is fetching the old Git LFS object.
pub fn resolved_download_label(entry: &ManifestEntry) -> &str {
    entry.display_url()
}

#[cfg(test)]
#[path = "manifest_lookup_tests.rs"]
mod tests;
