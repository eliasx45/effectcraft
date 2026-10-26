//! The session's Motion Library client ([`effectcraft_motionlib`]): where its catalog is, where
//! it caches, and the token.
//!
//! Configuration, first found wins:
//! - the host's own client ([`Handle::set`]: the web app reads through the page's fetch);
//! - the environment: `EFFECTCRAFT_LIBRARY_REMOTE` (a Worker URL or a folder),
//!   `EFFECTCRAFT_LIBRARY_TOKEN`, `EFFECTCRAFT_LIBRARY_CACHE`;
//! - `motion-library.json` in the settings folder (`library.configure` writes it):
//!   `{"remote": "https://…", "token": "…", "cache": "…"}`.
//!
//! The token lives only there or in the environment, never in a project or a reply.

use std::sync::{Arc, Mutex};

use effectcraft_motionlib::{Client, DirStore, MemStore, Store};
use serde::{Deserialize, Serialize};

use crate::Session;
pub use crate::commands::library::{LocalEntry, LocalIndex};

/// The settings file (in the session's config store).
pub const CONFIG: &str = "motion-library.json";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub remote: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub token: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cache: String,
}

/// Makes the remote for a Worker URL and token where the host has its own HTTP (the browser's
/// fetch).
pub type RemoteFactory = Arc<dyn Fn(&str, &str) -> Result<Arc<dyn effectcraft_motionlib::Remote>, String> + Send + Sync>;

/// The client, made on first use (and again after `library.configure`).
#[derive(Clone, Default)]
pub struct Handle {
    client: Arc<Mutex<Option<Client>>>,
    factory: Arc<Mutex<Option<RemoteFactory>>>,
    store: Arc<Mutex<Option<Arc<dyn Store>>>>,
}

impl Handle {
    /// Use `client` (hosts with their own transport).
    pub fn set(&self, client: Option<Client>) {
        if let Ok(mut c) = self.client.lock() {
            *c = client;
        }
    }
    pub fn get(&self) -> Option<Client> {
        self.client.lock().ok()?.clone()
    }
    /// Reach `https://` locations through `factory`, caching in `store` (the web app).
    pub fn set_transport(&self, factory: RemoteFactory, store: Arc<dyn Store>) {
        if let Ok(mut f) = self.factory.lock() {
            *f = Some(factory);
        }
        if let Ok(mut s) = self.store.lock() {
            *s = Some(store);
        }
    }
    fn factory(&self) -> Option<RemoteFactory> {
        self.factory.lock().ok()?.clone()
    }
    fn host_store(&self) -> Option<Arc<dyn Store>> {
        self.store.lock().ok()?.clone()
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// The operating system's cache folder for the library (desktop).
fn default_cache() -> Option<std::path::PathBuf> {
    if cfg!(target_arch = "wasm32") {
        return None;
    }
    let home = env("HOME").map(std::path::PathBuf::from);
    if cfg!(target_os = "macos") {
        return home.map(|h| h.join("Library/Caches/EffectCraft/Motion Library"));
    }
    if cfg!(windows) {
        return env("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("EffectCraft").join("Motion Library"));
    }
    env("XDG_CACHE_HOME").map(std::path::PathBuf::from).or_else(|| home.map(|h| h.join(".cache"))).map(|d| d.join("effectcraft").join("motion-library"))
}

impl Session {
    /// The settings in `motion-library.json`, with the environment on top.
    pub fn motion_library_config(&self) -> Config {
        let mut c: Config = self.config.as_ref().and_then(|s| s.read(CONFIG)).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        if let Some(r) = env("EFFECTCRAFT_LIBRARY_REMOTE") {
            c.remote = r;
        }
        if let Some(t) = env("EFFECTCRAFT_LIBRARY_TOKEN") {
            c.token = t;
        }
        if let Some(d) = env("EFFECTCRAFT_LIBRARY_CACHE") {
            c.cache = d;
        }
        c
    }

    /// The Motion Library client; an error says how to set one up.
    pub fn motion_library(&self) -> Result<Client, String> {
        if let Some(c) = self.motion_library.get() {
            return Ok(c);
        }
        let cfg = self.motion_library_config();
        if cfg.remote.is_empty() {
            return Err(
                "The Motion Library isn't set up here: run library.configure {remote, token} (or set EFFECTCRAFT_LIBRARY_REMOTE and EFFECTCRAFT_LIBRARY_TOKEN)"
                    .into(),
            );
        }
        let http = cfg.remote.starts_with("https://") || cfg.remote.starts_with("http://");
        let remote = match self.motion_library.factory().filter(|_| http) {
            Some(make) => make(&cfg.remote, &cfg.token)?,
            None => effectcraft_motionlib::remote_for(&cfg.remote, Some(&cfg.token))?,
        };
        let dir = if cfg.cache.is_empty() { default_cache() } else { Some(std::path::PathBuf::from(&cfg.cache)) };
        let store: Arc<dyn Store> = match (self.motion_library.host_store(), dir) {
            (Some(s), _) => s,
            (None, Some(d)) => Arc::new(DirStore::new(d)),
            (None, None) => Arc::new(MemStore::default()),
        };
        let client = Client::new(remote, store);
        self.motion_library.set(Some(client.clone()));
        Ok(client)
    }
}

impl Session {
    /// The configured library folders' entries by id: Motion Library cards found here apply and
    /// preview from the local files (the Mac that keeps the library).
    pub fn motion_library_local(&self) -> Arc<LocalIndex> {
        crate::commands::library::local_entries(self)
    }

    /// The local entry for card `id` whose file is there.
    pub fn motion_library_local_entry(&self, id: &str) -> Option<LocalEntry> {
        self.motion_library_local().get(id).filter(|e| !e.file.is_empty() && self.services.exists(&e.file)).cloned()
    }
}
