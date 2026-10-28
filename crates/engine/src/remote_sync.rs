//! Fast project sync for a frame server's replica: composition- and layer-level patches.
//!
//! Every undoable edit clones the project, and its compositions (`Arc<Comp>`) and their layers
//! (`Arc<Layer>`) stay shared copy-on-write between the snapshot before and after the edit. So
//! the sender ([`crate::remote::Mirror`]) finds what changed by pointer ([`Arc::ptr_eq`])
//! instead of serializing the whole project: a keyframe drag on a 40 MB project costs one layer.
//!
//! - [`diff_projects`]: the [`Patch`] from the snapshot the replica holds to the new one. Unchanged
//!   comps cost a pointer compare; a changed comp sends its shell (the item without its layers)
//!   only when that changed, and its stack as runs of kept layers, whole new layers, and edited
//!   layers as a structural JSON diff ([`crate::remote::diff`]) of the one layer.
//! - [`apply`]: the receiver applies it in place with [`Arc::make_mut`] (copy-on-write against a
//!   frame still rendering the previous revision); kept layers stay shared.
//! - Hashes: every op carries a [`StableHasher`] hash of what it produces (a comp's: its shell
//!   and each layer's, cached per layer by pointer in [`SyncCache`]). The receiver checks them;
//!   a mismatch (or any failure) makes it answer [`crate::remote::FrameReply::Resync`] and the
//!   sender sends the whole project again. Never an error for the user.
//! - [`encode_msg`] / [`decode_msg`]: the bytes of a message on a byte transport. A whole project
//!   travels zstd-compressed (pure Rust, [`ruzstd`]), as do other long messages.
//! - [`Coalescer`]: while dragging, at most one patch per interval, the latest revision wins.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Weak};

use effectcraft_project::{Comp, Item, ItemId, ItemKind, Layer, LayerId, Layers, Project};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::remote::{FrameMsg, Op, diff, patch};

/// The changes from the replica's revision `from_rev` to `to_rev`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Patch {
    pub from_rev: u64,
    pub to_rev: u64,
    pub ops: Vec<PatchOp>,
}

/// One change of a [`Patch`]. Each carries the [`StableHasher`] hash of what it produces.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum PatchOp {
    /// The project's fields other than its items (settings, render queue, next id…): a project
    /// with no items.
    Head { head: Value, hash: u64 },
    /// An item was deleted.
    Remove { id: ItemId },
    /// An item, whole: a new one, or a changed footage, solid or folder (or a comp that was
    /// another kind of item before).
    Item { item: Value, hash: u64 },
    /// A changed composition.
    Comp {
        id: ItemId,
        /// The item with its comp's layers left out, when that changed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell: Option<Value>,
        /// The new layer stack, when it changed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        layers: Option<Vec<LayerOp>>,
        /// [`SyncCache::comp_hash`] of the result.
        hash: u64,
    },
}

/// One run of a comp's new layer stack, in order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "l", rename_all = "camelCase")]
pub enum LayerOp {
    /// `count` layers of the old stack from index `from`, unchanged.
    Keep { from: usize, count: usize },
    /// A layer, whole.
    New { layer: Value },
    /// Old layer `from` with these changes to its JSON.
    Edit { from: usize, ops: Vec<Op> },
}

/// A stable 64-bit hash (the same on every platform and build, unlike `DefaultHasher`): eight
/// bytes at a time, with a final avalanche. A consistency check between two copies of a
/// project, not a cryptographic hash. It is an [`std::io::Write`] so JSON can stream into it.
#[derive(Clone, Copy)]
pub struct StableHasher {
    h: u64,
    tail: [u8; 8],
    n: usize,
    len: u64,
}

impl Default for StableHasher {
    fn default() -> Self {
        StableHasher { h: 0x243f_6a88_85a3_08d3, tail: [0; 8], n: 0, len: 0 }
    }
}

impl StableHasher {
    fn word(&mut self, w: u64) {
        self.h = (self.h.rotate_left(23) ^ w).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        self.h ^= self.h >> 29;
    }
    pub fn write(&mut self, mut bytes: &[u8]) {
        self.len = self.len.wrapping_add(bytes.len() as u64);
        if self.n > 0 {
            let take = (8 - self.n).min(bytes.len());
            let (head, rest) = bytes.split_at(take);
            if let Some(dst) = self.tail.get_mut(self.n..self.n + take) {
                dst.copy_from_slice(head);
            }
            self.n += take;
            bytes = rest;
            if self.n < 8 {
                return;
            }
            self.word(u64::from_le_bytes(self.tail));
            self.n = 0;
        }
        let (words, rest) = bytes.as_chunks::<8>();
        for w in words {
            self.word(u64::from_le_bytes(*w));
        }
        if let Some(dst) = self.tail.get_mut(..rest.len()) {
            dst.copy_from_slice(rest);
        }
        self.n = rest.len();
    }
    pub fn write_u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }
    pub fn finish(&self) -> u64 {
        let mut s = *self;
        if s.n > 0 {
            let mut b = [0u8; 8];
            if let (Some(dst), Some(src)) = (b.get_mut(..s.n), s.tail.get(..s.n)) {
                dst.copy_from_slice(src);
            }
            s.word(u64::from_le_bytes(b) ^ 0x80);
        }
        s.word(s.len);
        // splitmix64's finalizer.
        let mut z = s.h;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

impl Write for StableHasher {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        StableHasher::write(self, buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// [`StableHasher`] hash of `v`'s JSON.
pub fn hash_json<T: Serialize + ?Sized>(v: &T) -> u64 {
    let mut h = StableHasher::default();
    {
        let mut w = std::io::BufWriter::with_capacity(16 << 10, &mut h);
        // Serializing a project value can't fail (no non-string map keys); a failure would only
        // make the hashes differ, which resyncs.
        let _ = serde_json::to_writer(&mut w, v);
        let _ = w.flush();
    }
    h.finish()
}

/// A project without its items: what [`PatchOp::Head`] carries. Every field is named, so a new
/// project field can't be left out of the sync unnoticed.
fn head(p: &Project) -> Project {
    let Project { schema, settings, items: _, next_id, render_queue, render_templates, render_prefs, freeze, shadows } = p;
    Project {
        schema: *schema,
        settings: settings.clone(),
        items: Default::default(),
        next_id: *next_id,
        render_queue: render_queue.clone(),
        render_templates: render_templates.clone(),
        render_prefs: render_prefs.clone(),
        freeze: freeze.clone(),
        shadows: shadows.clone(),
    }
}

/// A comp item with its comp's layers left out.
fn shell(item: &Item, comp: &Comp) -> Item {
    Item { kind: ItemKind::Comp(Arc::new(Comp { layers: Layers::new(), ..comp.clone() })), ..item.clone() }
}

/// Hashes and layer JSON kept between syncs (one per side).
#[derive(Default)]
pub struct SyncCache {
    /// Layer hashes by address. The entry holds a `Weak` to the layer, so the address can't be
    /// reused while it is cached, and it is current while the layer lives (layers are only
    /// changed copy-on-write, never in place while shared).
    hashes: HashMap<usize, (Weak<Layer>, u64)>,
    /// The JSON of the layers the last patch edited (a drag edits the same layer again).
    values: HashMap<usize, (Weak<Layer>, Arc<Value>)>,
}

fn addr(l: &Arc<Layer>) -> usize {
    Arc::as_ptr(l) as usize
}

impl SyncCache {
    pub fn clear(&mut self) {
        self.hashes.clear();
        self.values.clear();
    }

    /// Hash of a layer's JSON (cached).
    pub fn layer_hash(&mut self, l: &Arc<Layer>) -> u64 {
        if let Some((w, h)) = self.hashes.get(&addr(l))
            && w.strong_count() > 0
        {
            return *h;
        }
        let h = hash_json(&**l);
        self.hashes.insert(addr(l), (Arc::downgrade(l), h));
        h
    }

    /// Hash of a comp item: its shell, then each layer's hash.
    pub fn comp_hash(&mut self, item: &Item, comp: &Comp) -> u64 {
        let mut h = StableHasher::default();
        h.write_u64(hash_json(&shell(item, comp)));
        for l in comp.layers.arcs() {
            h.write_u64(self.layer_hash(l));
        }
        h.finish()
    }

    /// The JSON of a layer (cached when the last patch made it).
    fn layer_value(&self, l: &Arc<Layer>) -> Result<Arc<Value>, String> {
        if let Some((w, v)) = self.values.get(&addr(l))
            && w.strong_count() > 0
        {
            return Ok(v.clone());
        }
        serde_json::to_value(&**l).map(Arc::new).map_err(|e| e.to_string())
    }

    /// Forget the layers nothing else holds any more (old versions of an edited layer).
    pub fn trim(&mut self) {
        self.hashes.retain(|_, (l, _)| l.strong_count() > 0);
        self.values.retain(|_, (l, _)| l.strong_count() > 0);
    }

    /// Cached layer hashes (tests, logs).
    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }
}

/// [`StableHasher`] hash of the whole project: what a replica holding the same project has too.
pub fn project_hash(p: &Project, cache: &mut SyncCache) -> u64 {
    let mut h = StableHasher::default();
    h.write_u64(hash_json(&head(p)));
    for (id, item) in &p.items {
        h.write_u64(id.0);
        h.write_u64(match &item.kind {
            ItemKind::Comp(c) => cache.comp_hash(item, c),
            _ => hash_json(item),
        });
    }
    h.finish()
}

fn to_value<T: Serialize>(v: &T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

/// Bytes of `v`'s JSON, without building the text.
fn json_len<T: Serialize + ?Sized>(v: &T) -> usize {
    struct Count(usize);
    impl Write for Count {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0 += b.len();
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut c = Count(0);
    let _ = serde_json::to_writer(&mut c, v);
    c.0
}

/// The changes that turn `old` into `new`. Comps and layers shared by both are skipped by
/// pointer, so the cost follows the size of the edit, not of the project.
pub fn diff_projects(old: &Project, new: &Project, cache: &mut SyncCache) -> Result<Vec<PatchOp>, String> {
    let mut ops = vec![];
    let (ho, hn) = (head(old), head(new));
    if ho != hn {
        ops.push(PatchOp::Head { head: to_value(&hn)?, hash: hash_json(&hn) });
    }
    for id in old.items.keys() {
        if !new.items.contains_key(id) {
            ops.push(PatchOp::Remove { id: *id });
        }
    }
    let mut values = HashMap::new();
    for (id, b) in &new.items {
        let a = old.items.get(id);
        match (a.map(|a| &a.kind), &b.kind) {
            (Some(ItemKind::Comp(ca)), ItemKind::Comp(cb)) => {
                let Some(a) = a else { continue };
                if Arc::ptr_eq(ca, cb) && a == b {
                    continue;
                }
                let (sa, sb) = (shell(a, ca), shell(b, cb));
                let shell_op = if sa != sb { Some(to_value(&sb)?) } else { None };
                let same_stack = ca.layers.len() == cb.layers.len() && (0..cb.layers.len()).all(|i| ca.layers.shares(&cb.layers, i));
                let layers = if same_stack { None } else { Some(layer_ops(&ca.layers, &cb.layers, cache, &mut values)?) };
                if shell_op.is_none() && layers.is_none() {
                    continue;
                }
                ops.push(PatchOp::Comp { id: *id, shell: shell_op, layers, hash: cache.comp_hash(b, cb) });
            }
            _ => {
                if a == Some(b) {
                    continue;
                }
                ops.push(PatchOp::Item { item: to_value(b)?, hash: item_hash(b, cache) });
            }
        }
    }
    cache.values = values;
    Ok(ops)
}

fn item_hash(item: &Item, cache: &mut SyncCache) -> u64 {
    match &item.kind {
        ItemKind::Comp(c) => cache.comp_hash(item, c),
        _ => hash_json(item),
    }
}

/// A comp's new stack as runs over the old one (see [`LayerOp`]).
fn layer_ops(old: &Layers, new: &Layers, cache: &SyncCache, values: &mut HashMap<usize, (Weak<Layer>, Arc<Value>)>) -> Result<Vec<LayerOp>, String> {
    let by_addr: HashMap<usize, usize> = old.arcs().iter().enumerate().map(|(i, l)| (addr(l), i)).collect();
    let by_id: HashMap<LayerId, usize> = old.iter().enumerate().map(|(i, l)| (l.id, i)).collect();
    let mut ops: Vec<LayerOp> = vec![];
    for l in new.arcs() {
        if let Some(&i) = by_addr.get(&addr(l)) {
            if let Some(LayerOp::Keep { from, count }) = ops.last_mut()
                && *from + *count == i
            {
                *count += 1;
            } else {
                ops.push(LayerOp::Keep { from: i, count: 1 });
            }
            continue;
        }
        let nv = Arc::new(to_value(&**l)?);
        let edit = match by_id.get(&l.id).and_then(|&i| Some((i, old.arcs().get(i)?))) {
            Some((i, ol)) => {
                let ov = cache.layer_value(ol)?;
                let d = diff(&ov, &nv);
                // An edit unless the changes outweigh the layer (most of it rewritten).
                (d.len() <= 8 || json_len(&d) < json_len(&*nv)).then_some(LayerOp::Edit { from: i, ops: d })
            }
            None => None,
        };
        ops.push(edit.unwrap_or_else(|| LayerOp::New { layer: (*nv).clone() }));
        values.insert(addr(l), (Arc::downgrade(l), nv));
    }
    Ok(ops)
}

/// Apply `ops` to the replica `project` (copy-on-write: a frame still rendering the previous
/// revision keeps it). Checks every hash; an error means the replica can't be trusted any more
/// and needs the whole project again.
pub fn apply(project: &mut Arc<Project>, ops: &[PatchOp], cache: &mut SyncCache) -> Result<(), String> {
    let p = Arc::make_mut(project);
    let mut values = HashMap::new();
    for op in ops {
        match op {
            PatchOp::Head { head: h, hash } => {
                let mut np = Project::deserialize(h).map_err(|e| format!("project head: {e}"))?;
                if hash_json(&np) != *hash {
                    return Err("the project head's hash differs".into());
                }
                np.items = std::mem::take(&mut p.items);
                *p = np;
            }
            PatchOp::Remove { id } => {
                p.items.remove(id).ok_or_else(|| format!("no item {}", id.0))?;
            }
            PatchOp::Item { item, hash } => {
                let it = Item::deserialize(item).map_err(|e| format!("item: {e}"))?;
                if item_hash(&it, cache) != *hash {
                    return Err(format!("item {}: the hash differs", it.id.0));
                }
                p.items.insert(it.id, it);
            }
            PatchOp::Comp { id, shell: sh, layers, hash } => {
                let old = p.items.get(id).ok_or_else(|| format!("no item {}", id.0))?;
                let ItemKind::Comp(oc) = &old.kind else { return Err(format!("item {} is not a composition", id.0)) };
                let stack = match layers {
                    Some(l) => Some(build_layers(&oc.layers, l, cache, &mut values)?),
                    None => None,
                };
                let item = match sh {
                    Some(v) => {
                        let mut it = Item::deserialize(v).map_err(|e| format!("comp {}: {e}", id.0))?;
                        let ItemKind::Comp(c) = &mut it.kind else { return Err(format!("item {} is not a composition", id.0)) };
                        Arc::make_mut(c).layers = stack.unwrap_or_else(|| oc.layers.clone());
                        it
                    }
                    None => {
                        let mut it = old.clone();
                        if let (ItemKind::Comp(c), Some(s)) = (&mut it.kind, stack) {
                            Arc::make_mut(c).layers = s;
                        }
                        it
                    }
                };
                if item.id != *id {
                    return Err(format!("comp {}: the item says {}", id.0, item.id.0));
                }
                if item_hash(&item, cache) != *hash {
                    return Err(format!("comp {}: the hash differs", id.0));
                }
                p.items.insert(*id, item);
            }
        }
    }
    cache.values = values;
    Ok(())
}

fn build_layers(old: &Layers, ops: &[LayerOp], cache: &SyncCache, values: &mut HashMap<usize, (Weak<Layer>, Arc<Value>)>) -> Result<Layers, String> {
    let mut out = Layers::new();
    let v = out.arcs_mut();
    for op in ops {
        match op {
            LayerOp::Keep { from, count } => {
                let run = old.arcs().get(*from..from.saturating_add(*count)).ok_or_else(|| format!("no layers {from}+{count}"))?;
                v.extend(run.iter().cloned());
            }
            LayerOp::New { layer } => {
                v.push(Arc::new(Layer::deserialize(layer).map_err(|e| format!("layer: {e}"))?));
            }
            LayerOp::Edit { from, ops } => {
                let ol = old.arcs().get(*from).ok_or_else(|| format!("no layer {from}"))?;
                let mut val = (*cache.layer_value(ol)?).clone();
                patch(&mut val, ops)?;
                let l = Arc::new(Layer::deserialize(&val).map_err(|e| format!("layer: {e}"))?);
                values.insert(addr(&l), (Arc::downgrade(&l), Arc::new(val)));
                v.push(l);
            }
        }
    }
    Ok(out)
}

/// A whole project for a replica: the JSON read as it is (no repairs, so the replica is an exact
/// copy of the sender's project).
pub fn read_project(json: &[u8]) -> Result<Project, String> {
    let p: Project = serde_json::from_slice(json).map_err(|e| e.to_string())?;
    if p.schema > effectcraft_project::SCHEMA_VERSION {
        return Err(format!("project schema {} is newer than this version supports", p.schema));
    }
    Ok(p)
}

// Byte encoding.

/// Messages from this many bytes of JSON are compressed.
pub const COMPRESS_FROM: usize = 16 << 10;
/// The most a message may decompress to (a damaged or hostile message can't exhaust memory).
pub const MAX_DECODED: u64 = 4 << 30;

const TAG_JSON: u8 = b'j';
const TAG_ZSTD: u8 = b'z';
const TAG_PROJECT: u8 = b'p';

fn zstd(data: &[u8]) -> Option<Vec<u8>> {
    // The encoder is a dependency: should it ever panic, send the bytes uncompressed.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest))).ok()
}

fn unzstd(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut src = data;
    let dec = ruzstd::decoding::StreamingDecoder::new(&mut src).map_err(|e| format!("zstd: {e}"))?;
    let mut out = vec![];
    dec.take(MAX_DECODED + 1).read_to_end(&mut out).map_err(|e| format!("zstd: {e}"))?;
    if out.len() as u64 > MAX_DECODED {
        return Err("the message is too large".into());
    }
    Ok(out)
}

/// The bytes of `msg` for a byte transport: a tag byte, then the message's JSON (`j`), its JSON
/// zstd-compressed (`z`, from [`COMPRESS_FROM`] bytes), or for a whole project the revision (8
/// bytes, little-endian) and the project's JSON zstd-compressed (`p`: not escaped as a string).
pub fn encode_msg(msg: &FrameMsg) -> Vec<u8> {
    if let FrameMsg::Project { revision, project } = msg
        && let Some(z) = zstd(project.as_bytes())
    {
        let mut out = Vec::with_capacity(9 + z.len());
        out.push(TAG_PROJECT);
        out.extend_from_slice(&revision.to_le_bytes());
        out.extend_from_slice(&z);
        return out;
    }
    let json = serde_json::to_vec(msg).unwrap_or_default();
    if json.len() >= COMPRESS_FROM
        && let Some(z) = zstd(&json)
        && z.len() < json.len()
    {
        let mut out = Vec::with_capacity(1 + z.len());
        out.push(TAG_ZSTD);
        out.extend_from_slice(&z);
        return out;
    }
    let mut out = Vec::with_capacity(1 + json.len());
    out.push(TAG_JSON);
    out.extend_from_slice(&json);
    out
}

/// Read [`encode_msg`]'s bytes.
pub fn decode_msg(bytes: &[u8]) -> Result<FrameMsg, String> {
    let (&tag, rest) = bytes.split_first().ok_or("an empty message")?;
    let json = |b: &[u8]| serde_json::from_slice::<FrameMsg>(b).map_err(|e| e.to_string());
    match tag {
        TAG_JSON => json(rest),
        TAG_ZSTD => json(&unzstd(rest)?),
        TAG_PROJECT => {
            let (rev, z) = rest.split_first_chunk::<8>().ok_or("a project message without its revision")?;
            let project = String::from_utf8(unzstd(z)?).map_err(|e| e.to_string())?;
            Ok(FrameMsg::Project { revision: u64::from_le_bytes(*rev), project })
        }
        t => Err(format!("unknown message tag {t}")),
    }
}

/// Latest-wins rate limit for syncs while dragging: [`Coalescer::offer`] every revision,
/// [`Coalescer::take`] each UI frame; a revision is handed out at most once per `interval`, and
/// the ones in between are skipped (the patch from the replica's revision to the latest covers
/// them). Before a render request for the latest revision, [`Coalescer::flush`].
pub struct Coalescer {
    interval: web_time::Duration,
    last: Option<web_time::Instant>,
    pending: Option<(u64, Arc<Project>)>,
}

impl Default for Coalescer {
    /// One sync per 60 Hz frame.
    fn default() -> Self {
        Coalescer::new(web_time::Duration::from_micros(16_667))
    }
}

impl Coalescer {
    pub fn new(interval: web_time::Duration) -> Coalescer {
        Coalescer { interval, last: None, pending: None }
    }

    /// A new revision (replaces one not yet taken).
    pub fn offer(&mut self, revision: u64, project: &Arc<Project>) {
        self.pending = Some((revision, project.clone()));
    }

    /// The revision to sync now, if one is waiting and the interval since the last has passed.
    pub fn take(&mut self, now: web_time::Instant) -> Option<(u64, Arc<Project>)> {
        if self.last.is_some_and(|t| now.saturating_duration_since(t) < self.interval) {
            return None;
        }
        let p = self.pending.take()?;
        self.last = Some(now);
        Some(p)
    }

    /// The waiting revision, now (a render needs it, or the drag ended).
    pub fn flush(&mut self, now: web_time::Instant) -> Option<(u64, Arc<Project>)> {
        let p = self.pending.take()?;
        self.last = Some(now);
        Some(p)
    }

    /// When [`Coalescer::take`] will hand out the waiting revision.
    pub fn due(&self) -> Option<web_time::Instant> {
        self.pending.as_ref()?;
        Some(self.last.map_or_else(web_time::Instant::now, |t| t + self.interval))
    }
}
