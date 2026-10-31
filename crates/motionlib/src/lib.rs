//! Motion Library: a catalog of motion cards (compositions with their thumbnails, hover previews
//! and the small files they use) kept in a remote bucket and pulled only when needed.
//!
//! The bucket holds content-addressed blobs and two small documents:
//!
//! - `head.json` ([`Head`]): the catalog's version and where its manifest is. Read when the panel
//!   opens; a newer version than the cached one shows the "Library updated" chip.
//! - the manifest ([`Manifest`], a gzipped blob): every card with its blob references.
//! - `blobs/<sha256>[.gz]`: thumbnails, previews, `.ecproj` projects and their small files.
//!
//! Where a client runs (desktop, a rented GPU, the browser) it keeps a local [`Store`] keyed by
//! content hash, so each blob is downloaded once. Big footage is never uploaded: such files stay
//! in the project as missing footage ([`Placeholder`]), to be relinked with `footage.relink`.
//!
//! The publisher ([`publish`], desktop only) reads a library folder's `library.json` and uploads
//! only new blobs, then bumps the version.
//!
//! Nothing here panics: a bad manifest, a failed download or no network is an error message.

mod client;
mod hash;
mod manifest;
mod remote;
mod search;
mod store;

#[cfg(not(target_arch = "wasm32"))]
mod http;
#[cfg(not(target_arch = "wasm32"))]
pub mod publish;

pub use client::{BlobState, Client, Prepared, Status, UpdateInfo, rewrite_paths};
pub use hash::sha256_hex;
pub use manifest::{
    BlobRef, Card, CardFile, CardVariant, Changes, Collection, Crop, FORMAT, FolderRow, HEAD_KEY, Head, Manifest, Placeholder, decode_blob, encode_blob,
};
pub use remote::{DirRemote, Done, Remote};
pub use search::{score, search, tokens};
pub use store::{DirStore, MemStore, Store, valid_key};

#[cfg(not(target_arch = "wasm32"))]
pub use http::HttpRemote;
#[cfg(not(target_arch = "wasm32"))]
pub use remote::get_blocking;

/// Make a remote from a configured location: an `https://` (or `http://`) URL is the Worker in
/// front of the bucket (bearer `token`), anything else a folder (the local backend).
pub fn remote_for(location: &str, token: Option<&str>) -> Result<std::sync::Arc<dyn Remote>, String> {
    let location = location.trim();
    if location.is_empty() {
        return Err("no Motion Library location configured".into());
    }
    if location.starts_with("https://") || location.starts_with("http://") {
        #[cfg(not(target_arch = "wasm32"))]
        {
            return Ok(std::sync::Arc::new(HttpRemote::new(location, token.unwrap_or_default())?));
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = token;
            return Err("the browser build reads the Motion Library through the page (no HTTP client here)".into());
        }
    }
    Ok(std::sync::Arc::new(DirRemote::new(location)))
}
