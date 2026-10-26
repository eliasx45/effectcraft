//! Who a layer-cache entry belongs to, for the Memory panel (Window ▸ Memory) and
//! `memory.stats` / `memory.purge`.
//!
//! Cache keys are content hashes and say nothing about the layer they came from. The renderer
//! enters an [`OwnerScope`] (comp, layer, and for a layer's input the effect it feeds) at the top
//! of each per-layer step; [`super::LayerCache`] stamps every buffer it keeps with the
//! innermost scope of the inserting thread. A scope is a thread-local `Cell` swap: no lock, no
//! allocation. Nested precomps enter their own scopes, so their buffers belong to the nested
//! comp's layers and the precomp layer's own buffer (inserted after the nested render returns)
//! to the precomp layer.
//!
//! Buffers inserted outside any scope (a GPU readback finished on another thread, a footage
//! frame read for an expression) are "unattributed": the panel shows them as their own row.

use std::cell::Cell;

/// The comp, layer and effect a cached buffer was rendered for. `None` parts are unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Owner {
    /// The comp's item id.
    pub comp: Option<u64>,
    /// The layer's id in that comp.
    pub layer: Option<u64>,
    /// A layer *input* (source → masks → the first `effect` effects, see
    /// `Renderer::layer_input`): the 0-based index of the effect it feeds.
    pub effect: Option<u32>,
}

impl Owner {
    /// Unattributed.
    pub const NONE: Owner = Owner { comp: None, layer: None, effect: None };

    /// A layer's processed buffer.
    pub const fn layer(comp: u64, layer: u64) -> Owner {
        Owner { comp: Some(comp), layer: Some(layer), effect: None }
    }

    /// Whether this owner is in `comp` (and, when given, is `layer`).
    pub fn matches(&self, comp: Option<u64>, layer: Option<u64>) -> bool {
        comp.is_none_or(|c| self.comp == Some(c)) && layer.is_none_or(|l| self.layer == Some(l))
    }
}

thread_local! {
    static CURRENT: Cell<Owner> = const { Cell::new(Owner::NONE) };
}

/// The owner the current thread renders for ([`Owner::NONE`] outside any scope, or while the
/// thread is being torn down).
pub fn current() -> Owner {
    CURRENT.try_with(Cell::get).unwrap_or_default()
}

/// While alive, buffers this thread inserts into a layer cache belong to its owner; dropping it
/// restores the enclosing scope. Keep it in a local (`let _owner = …`): it is not `Send`.
#[must_use = "the scope ends when this is dropped"]
pub struct OwnerScope {
    prev: Owner,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl OwnerScope {
    pub fn enter(owner: Owner) -> OwnerScope {
        let prev = CURRENT.try_with(|c| c.replace(owner)).unwrap_or_default();
        OwnerScope { prev, _not_send: std::marker::PhantomData }
    }

    /// A layer's processed buffers (content, styles, blend conversions).
    pub fn layer(comp: u64, layer: u64) -> OwnerScope {
        Self::enter(Owner::layer(comp, layer))
    }

    /// A layer's input to effect `effect` (0-based).
    pub fn effect(comp: u64, layer: u64, effect: usize) -> OwnerScope {
        Self::enter(Owner { comp: Some(comp), layer: Some(layer), effect: Some(u32::try_from(effect).unwrap_or(u32::MAX)) })
    }
}

impl Drop for OwnerScope {
    fn drop(&mut self) {
        let prev = self.prev;
        let _ = CURRENT.try_with(|c| c.set(prev));
    }
}

/// Where one cached buffer's bytes are (see [`super::LayerCache::usage`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageKind {
    /// CPU pixels (`f32`, or packed).
    Cpu,
    /// A texture the cache keeps on the GPU.
    Gpu,
}

/// One cached buffer: who it belongs to, its bytes, where they are and whether it is still on
/// probation (a first render the cache may drop soon).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryUsage {
    pub owner: Owner,
    pub bytes: usize,
    pub kind: UsageKind,
    pub probation: bool,
}

/// What [`super::LayerCache::purge_owned`] freed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Purged {
    pub entries: usize,
    pub bytes: usize,
    pub gpu_slots: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_nest_and_restore() {
        assert_eq!(current(), Owner::NONE);
        {
            let _a = OwnerScope::layer(1, 10);
            assert_eq!(current(), Owner::layer(1, 10));
            {
                let _b = OwnerScope::effect(2, 20, 3);
                assert_eq!(current(), Owner { comp: Some(2), layer: Some(20), effect: Some(3) });
            }
            assert_eq!(current(), Owner::layer(1, 10));
        }
        assert_eq!(current(), Owner::NONE);
    }

    #[test]
    fn scopes_are_per_thread() {
        let _a = OwnerScope::layer(1, 10);
        let other = std::thread::spawn(current).join().unwrap_or(Owner::layer(9, 9));
        assert_eq!(other, Owner::NONE);
        assert_eq!(current(), Owner::layer(1, 10));
    }

    fn pixels() -> std::sync::Arc<effectcraft_effects::Buf> {
        std::sync::Arc::new(effectcraft_effects::Buf { img: effectcraft_raster::Image::new(8, 8), offset: [0.0; 2], scale: 1.0 })
    }

    fn owned_bytes(c: &super::super::LayerCache, owner: Owner) -> usize {
        c.usage().iter().filter(|u| u.owner == owner && u.kind == UsageKind::Cpu).map(|u| u.bytes).sum()
    }

    #[test]
    fn inserts_are_stamped_and_the_meter_follows_insert_evict_purge() {
        let c = super::super::LayerCache::new(1 << 24);
        {
            let _o = OwnerScope::layer(1, 10);
            c.insert(1, pixels());
            c.insert(2, pixels());
        }
        {
            let _o = OwnerScope::layer(1, 11);
            c.insert(3, pixels());
            let _e = OwnerScope::effect(1, 11, 0);
            c.insert(4, pixels());
        }
        {
            let _o = OwnerScope::layer(2, 20);
            c.insert(5, pixels());
        }
        // Outside any scope: unattributed.
        c.insert(6, pixels());
        let st = c.stats();
        assert_eq!(st.entries, 6);
        assert_eq!(c.bytes_now() as usize, st.bytes);
        let one = st.bytes / 6;
        assert_eq!(owned_bytes(&c, Owner::layer(1, 10)), 2 * one);
        assert_eq!(owned_bytes(&c, Owner::layer(1, 11)), one);
        assert_eq!(owned_bytes(&c, Owner { comp: Some(1), layer: Some(11), effect: Some(0) }), one);
        assert_eq!(owned_bytes(&c, Owner::NONE), one);
        // The usage list adds up to the cache's own count.
        assert_eq!(c.usage().iter().filter(|u| u.kind == UsageKind::Cpu).map(|u| u.bytes).sum::<usize>(), st.bytes);

        // Purge one layer: its entries go, the meter follows.
        let p = c.purge_owned(|o| o.matches(Some(1), Some(10)));
        assert_eq!((p.entries, p.bytes), (2, 2 * one));
        assert_eq!(c.stats().entries, 4);
        assert_eq!(c.bytes_now() as usize, c.stats().bytes);
        assert_eq!(owned_bytes(&c, Owner::layer(1, 10)), 0);

        // Purge a comp: its layers and their effect inputs go.
        let p = c.purge_owned(|o| o.matches(Some(1), None));
        assert_eq!(p.entries, 2);
        assert_eq!(c.stats().entries, 2);
        assert_eq!(c.bytes_now() as usize, c.stats().bytes);

        // Eviction (a smaller budget) keeps the meter exact.
        c.set_budget(one);
        assert_eq!(c.bytes_now() as usize, c.stats().bytes);

        c.clear();
        assert_eq!(c.bytes_now(), 0);
        assert!(c.usage().is_empty());
    }

    #[test]
    fn matches_comp_and_layer() {
        let o = Owner::layer(1, 10);
        assert!(o.matches(None, None));
        assert!(o.matches(Some(1), None));
        assert!(o.matches(Some(1), Some(10)));
        assert!(!o.matches(Some(2), None));
        assert!(!o.matches(Some(1), Some(11)));
        assert!(!Owner::NONE.matches(Some(1), None));
    }
}
