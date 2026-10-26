//! A catalog client: the adopted catalog, the update check and downloads into the local store.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use serde_json::Value;

use crate::manifest::{BlobRef, Card, Changes, HEAD_KEY, Head, Manifest, decode_blob};
use crate::remote::Remote;
use crate::store::Store;

/// The local key of the adopted catalog's manifest (by hash, so an update doesn't overwrite it).
fn manifest_key(head: &Head) -> String {
    BlobRef { name: "manifest.json".into(), ..head.manifest.clone() }.local_key()
}

/// A newer catalog seen in the remote, not adopted yet (the "Library updated" chip).
struct Pending {
    head: Head,
    manifest: Arc<Manifest>,
    changes: Changes,
}

#[derive(Default)]
struct Inner {
    head: Option<Head>,
    catalog: Option<Arc<Manifest>>,
    update: Option<Pending>,
    refreshing: bool,
    /// A check of the remote finished (successfully or not).
    checked: bool,
    error: Option<String>,
    /// Local keys being downloaded.
    loading: BTreeSet<String>,
    /// Local keys whose download failed, with the reason (cleared by a refresh).
    failed: BTreeMap<String, String>,
    /// Bumped whenever the catalog or a download changes (UI caches compare it).
    generation: u64,
}

struct Shared {
    remote: Arc<dyn Remote>,
    store: Arc<dyn Store>,
    inner: Mutex<Inner>,
    notify: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic elsewhere while holding the lock leaves plain data: keep using it.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn notify(&self) {
        let f = self.notify.lock().ok().and_then(|n| n.clone());
        if let Some(f) = f {
            f();
        }
    }
}

/// The update the chip offers.
#[derive(Clone, Debug, Serialize)]
pub struct UpdateInfo {
    pub version: u64,
    pub cards: usize,
    #[serde(flatten)]
    pub changes: Changes,
    /// "Library updated: 3 new · 2 changed".
    pub label: String,
}

/// What `library.status` reports.
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub remote: String,
    pub cache: String,
    /// The adopted catalog's version (0 = none yet).
    pub version: u64,
    pub cards: usize,
    #[serde(rename = "needsFootage")]
    pub needs_footage: usize,
    pub update: Option<UpdateInfo>,
    pub refreshing: bool,
    pub checked: bool,
    pub error: Option<String>,
    pub downloading: usize,
    pub failed: usize,
}

/// A blob in the local store.
#[derive(Clone, Debug, PartialEq)]
pub enum BlobState {
    /// Cached (with its file when the store is a folder).
    Ready(Option<PathBuf>),
    Loading,
    Failed(String),
}

/// A card made ready to apply.
#[derive(Clone, Debug)]
pub enum Prepared {
    Ready {
        card: Box<Card>,
        /// The project's (or footage file's) bytes, as published.
        project: Vec<u8>,
        /// Each uploaded file with its cached copy's file (folder stores) and bytes otherwise.
        files: Vec<(String, BlobRef, Option<PathBuf>)>,
    },
    Loading {
        done: usize,
        total: usize,
    },
    Failed(String),
}

/// A Motion Library client. Cheap to clone; every clone shares the state.
#[derive(Clone)]
pub struct Client(Arc<Shared>);

impl Client {
    /// A client over `remote`, starting from the catalog cached in `store` (if any): the library
    /// browses offline.
    pub fn new(remote: Arc<dyn Remote>, store: Arc<dyn Store>) -> Client {
        let shared = Shared { remote, store, inner: Mutex::default(), notify: Mutex::new(None) };
        let cached = shared.store.get(HEAD_KEY).and_then(|b| Head::parse(&b).ok()).and_then(|head| {
            let m = Manifest::parse(&shared.store.get(&manifest_key(&head))?).ok()?;
            Some((head, m))
        });
        if let Some((head, m)) = cached {
            let mut inner = shared.lock();
            inner.head = Some(head);
            inner.catalog = Some(Arc::new(m));
        }
        Client(Arc::new(shared))
    }

    /// Called (on any thread) when a download finishes or the catalog changes: the UI repaints.
    pub fn set_notify(&self, f: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut n) = self.0.notify.lock() {
            *n = Some(f);
        }
    }

    pub fn store(&self) -> Arc<dyn Store> {
        self.0.store.clone()
    }

    pub fn catalog(&self) -> Option<Arc<Manifest>> {
        self.0.lock().catalog.clone()
    }

    pub fn generation(&self) -> u64 {
        self.0.lock().generation
    }

    pub fn status(&self) -> Status {
        let inner = self.0.lock();
        let cat = inner.catalog.as_deref();
        Status {
            remote: self.0.remote.describe(),
            cache: self.0.store.describe(),
            version: inner.head.as_ref().map_or(0, |h| h.version),
            cards: cat.map_or(0, |c| c.cards.len()),
            needs_footage: cat.map_or(0, |c| c.cards.iter().filter(|c| c.needs_footage()).count()),
            update: inner.update.as_ref().map(|p| UpdateInfo {
                version: p.head.version,
                cards: p.manifest.cards.len(),
                changes: p.changes.clone(),
                label: format!("Library updated: {}", p.changes.label()),
            }),
            refreshing: inner.refreshing,
            checked: inner.checked,
            error: inner.error.clone(),
            downloading: inner.loading.len(),
            failed: inner.failed.len(),
        }
    }

    /// Check the remote for a newer catalog. The first catalog is adopted at once; a newer one
    /// waits as an update ([`Client::adopt_update`]). Offline, the cached catalog stays.
    pub fn refresh(&self) {
        {
            let mut inner = self.0.lock();
            if inner.refreshing {
                return;
            }
            inner.refreshing = true;
            inner.failed.clear();
        }
        let shared = self.0.clone();
        self.0.remote.get(
            HEAD_KEY,
            Box::new(move |r| {
                let head = r.and_then(|b| Head::parse(&b).map(|h| (h, b)));
                match head {
                    Err(e) => finish_refresh(&shared, Err(e)),
                    Ok((head, raw)) => {
                        let current = shared.lock().head.as_ref().map(|h| h.manifest.hash.clone());
                        if current.as_deref() == Some(head.manifest.hash.as_str()) {
                            return finish_refresh(&shared, Ok(None));
                        }
                        let key = manifest_key(&head);
                        if let Some(m) = shared.store.get(&key).and_then(|b| Manifest::parse(&b).ok()) {
                            return finish_refresh(&shared, Ok(Some((head, raw, m))));
                        }
                        let (blob, shared2) = (head.manifest.clone(), shared.clone());
                        shared.remote.get(
                            &blob.remote_key(),
                            Box::new(move |r| {
                                let m = r.and_then(|stored| decode_blob(&blob, &stored)).and_then(|bytes| {
                                    let m = Manifest::parse(&bytes)?;
                                    // Cached by hash; a failed write only costs a download later.
                                    let _ = shared2.store.put(&key, &bytes);
                                    Ok(m)
                                });
                                finish_refresh(&shared2, m.map(|m| Some((head, raw, m))));
                            }),
                        );
                    }
                }
            }),
        );
    }

    /// Switch to the newer catalog the remote has (the chip's click). `false`: there is none.
    pub fn adopt_update(&self) -> bool {
        let pending = self.0.lock().update.take();
        let Some(p) = pending else { return false };
        if let Ok(raw) = serde_json::to_vec(&p.head) {
            let _ = self.0.store.put(HEAD_KEY, &raw);
        }
        let mut inner = self.0.lock();
        inner.head = Some(p.head);
        inner.catalog = Some(p.manifest);
        inner.generation += 1;
        drop(inner);
        self.0.notify();
        true
    }

    /// Where a blob is, starting its download when it's not cached (thumbnails as they scroll
    /// into view, previews on hover, project files on apply).
    pub fn blob(&self, blob: &BlobRef) -> BlobState {
        if !blob.valid() {
            return BlobState::Failed("bad blob reference".into());
        }
        let key = blob.local_key();
        if self.0.store.has(&key) {
            return BlobState::Ready(self.0.store.path(&key));
        }
        {
            let mut inner = self.0.lock();
            if let Some(e) = inner.failed.get(&key) {
                return BlobState::Failed(e.clone());
            }
            if !inner.loading.insert(key.clone()) {
                return BlobState::Loading;
            }
        }
        let (shared, blob2, key2) = (self.0.clone(), blob.clone(), key.clone());
        self.0.remote.get(
            &blob.remote_key(),
            Box::new(move |r| {
                let r = r.and_then(|stored| decode_blob(&blob2, &stored)).and_then(|bytes| shared.store.put(&key2, &bytes));
                let mut inner = shared.lock();
                inner.loading.remove(&key2);
                if let Err(e) = r {
                    inner.failed.insert(key2, e);
                }
                inner.generation += 1;
                drop(inner);
                shared.notify();
            }),
        );
        // A synchronous remote (the folder backend) may have finished already.
        if self.0.store.has(&key) {
            return BlobState::Ready(self.0.store.path(&key));
        }
        match self.0.lock().failed.get(&key) {
            Some(e) => BlobState::Failed(e.clone()),
            None => BlobState::Loading,
        }
    }

    /// A cached blob's bytes.
    pub fn blob_bytes(&self, blob: &BlobRef) -> Option<Vec<u8>> {
        self.0.store.get(&blob.local_key())
    }

    /// Download what applying `id` needs; `Ready` with the project and files once all is cached.
    pub fn prepare(&self, id: &str) -> Prepared {
        let Some(cat) = self.catalog() else { return Prepared::Failed("the Motion Library has no catalog yet (library.sync)".into()) };
        let Some(card) = cat.card(id) else { return Prepared::Failed(format!("no Motion Library card `{id}`")) };
        if card.on_mac {
            return Prepared::Failed(format!("“{}” is on the Mac: apply it in After Effects there (here: Devin Jatho and SceneYard cards)", card.name));
        }
        let Some(project) = card.project.as_ref() else { return Prepared::Failed(format!("“{}” has no file to apply", card.name)) };
        let blobs = card.apply_blobs();
        let total = blobs.len();
        let mut done = 0;
        for b in &blobs {
            match self.blob(b) {
                BlobState::Ready(_) => done += 1,
                BlobState::Loading => {}
                BlobState::Failed(e) => return Prepared::Failed(format!("“{}”: {e}", card.name)),
            }
        }
        if done < total {
            return Prepared::Loading { done, total };
        }
        let Some(bytes) = self.blob_bytes(project) else { return Prepared::Failed(format!("“{}”: the cached project can't be read", card.name)) };
        let files = card.files.iter().map(|f| (f.path.clone(), f.blob.clone(), self.0.store.path(&f.blob.local_key()))).collect();
        Prepared::Ready { card: Box::new(card.clone()), project: bytes, files }
    }

    /// [`Client::prepare`], waiting up to `timeout` for the downloads (desktop; agents).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn prepare_blocking(&self, id: &str, timeout: std::time::Duration) -> Prepared {
        let until = std::time::Instant::now() + timeout;
        loop {
            match self.prepare(id) {
                Prepared::Loading { done, total } if std::time::Instant::now() < until => {
                    let _ = (done, total);
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                other => return other,
            }
        }
    }

    /// [`Client::refresh`], waiting up to `timeout` for the answer (desktop; agents).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn refresh_blocking(&self, timeout: std::time::Duration) {
        self.refresh();
        let until = std::time::Instant::now() + timeout;
        while self.0.lock().refreshing && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

/// The end of a [`Client::refresh`]: adopt the first catalog, hold a newer one as an update.
fn finish_refresh(shared: &Shared, r: Result<Option<(Head, Vec<u8>, Manifest)>, String>) {
    let mut inner = shared.lock();
    inner.refreshing = false;
    inner.checked = true;
    match r {
        Err(e) => {
            inner.error = Some(if inner.catalog.is_some() { format!("Offline: showing the cached library ({e})") } else { e });
        }
        Ok(None) => {
            inner.error = None;
            inner.update = None;
        }
        Ok(Some((head, raw, m))) => {
            inner.error = None;
            match inner.catalog.as_deref() {
                None => {
                    let _ = shared.store.put(HEAD_KEY, &raw);
                    inner.head = Some(head);
                    inner.catalog = Some(Arc::new(m));
                    inner.update = None;
                }
                Some(cat) => {
                    let changes = cat.changes_to(&m);
                    inner.update = Some(Pending { head, manifest: Arc::new(m), changes });
                }
            }
        }
    }
    inner.generation += 1;
    drop(inner);
    shared.notify();
}

/// A project's JSON with every string naming one of `paths` replaced by its new value (the cached
/// copies of the files the publisher uploaded). Strings elsewhere are left alone.
pub fn rewrite_paths(project: &[u8], paths: &BTreeMap<String, String>) -> Result<Vec<u8>, String> {
    fn walk(v: &mut Value, paths: &BTreeMap<String, String>, depth: usize) {
        if depth > 256 {
            return;
        }
        match v {
            Value::String(s) => {
                if let Some(new) = paths.get(s.as_str()) {
                    *s = new.clone();
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|i| walk(i, paths, depth + 1)),
            Value::Object(map) => map.values_mut().for_each(|i| walk(i, paths, depth + 1)),
            _ => {}
        }
    }
    let mut v: Value = serde_json::from_slice(project).map_err(|e| format!("the card's project doesn't read: {e}"))?;
    walk(&mut v, paths, 0);
    serde_json::to_vec(&v).map_err(|e| e.to_string())
}
