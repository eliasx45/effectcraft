//! Opaque GPU ownership. Payloads leave all locks before their destructors run.
use super::{Inner, ResidentToken};
use crate::allocation::{GpuAllocation, GpuGeneration};
use crate::retained::{AdmissionError, MAX_RECORDS, Reservation};
use effectcraft_effects::Buf;
use std::any::Any;
use std::sync::{Arc, Mutex, Weak};

/// Exact representation stage; a CPU mirror is identified by its source Arc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuStage {
    CpuMirror,
    Processed,
    Blend,
}

/// Backend readback must contain allocation and mapping failures. Called without cache locks.
pub trait ResidentReadback: Send + Sync {
    fn readback(&self, slot: &GpuSlot) -> Option<Buf>;
}

pub struct GpuSlot {
    payload: Arc<dyn Any + Send + Sync>,
    allocation: Arc<GpuAllocation>,
    stage: GpuStage,
    persistence: Option<crate::disk_cache::LayerWriteIntent>,
    ready: Mutex<Weak<Buf>>,
    content: Option<u64>,
    readback: Option<Arc<dyn ResidentReadback>>,
}
impl GpuSlot {
    pub(super) fn persist(&self, buf: &Buf) {
        if let Some(intent) = &self.persistence {
            intent.put(buf);
        }
    }
    pub(super) fn ready(&self) -> Option<Arc<Buf>> {
        self.ready.lock().ok()?.upgrade()
    }
    pub(super) fn handoff(&self, buf: &Arc<Buf>) {
        if let Ok(mut ready) = self.ready.lock() {
            *ready = Arc::downgrade(buf);
        }
    }
    pub(super) fn materialize(&self) -> Option<Buf> {
        if self.stage != GpuStage::Processed || !self.allocation.generation().valid() {
            return None;
        }
        self.readback.as_ref()?.readback(self)
    }
    pub fn payload<T: Any>(&self) -> Option<&T> {
        self.payload.downcast_ref()
    }
    pub fn allocation(&self) -> &Arc<GpuAllocation> {
        &self.allocation
    }
    pub fn stage(&self) -> GpuStage {
        self.stage
    }
    pub(super) fn content(&self) -> Option<u64> {
        self.content
    }
}
pub(super) struct GpuEntry {
    pub(super) slot: Arc<GpuSlot>,
    pub(super) last_use: u64,
    /// Published and not used since: evicted before admitted entries (see `evict_one`).
    pub(super) probation: bool,
}

/// A weak per-operation capability. Never install one globally in a shared backend.
#[derive(Clone)]
pub struct GpuOwner {
    inner: Weak<Mutex<Inner>>,
    token: ResidentToken,
}
impl GpuOwner {
    pub(super) fn new(inner: Weak<Mutex<Inner>>, token: ResidentToken) -> Self {
        Self { inner, token }
    }
    pub(super) fn token(&self) -> &ResidentToken {
        &self.token
    }
    pub fn identity(&self) -> usize {
        self.token.identity()
    }
    pub fn valid(&self) -> bool {
        self.token.valid() && self.inner.strong_count() > 0
    }

    pub fn prepare(
        &self,
        payload: Arc<dyn Any + Send + Sync>,
        allocation: Arc<GpuAllocation>,
        stage: GpuStage,
        device_cap: usize,
    ) -> Result<GpuPending, AdmissionError> {
        if !allocation.generation().valid() {
            return Err(AdmissionError::Retired);
        }
        let inner = self.inner.upgrade().ok_or(AdmissionError::Retired)?;
        let (extra, claim) = {
            let g = inner.lock().map_err(|_| AdmissionError::Retired)?;
            if !self.valid() {
                return Err(AdmissionError::Retired);
            }
            if allocation.bytes() > g.budget || allocation.bytes() > device_cap {
                return Err(AdmissionError::Capacity);
            }
            let claim = g.physical.claim_gpu(&allocation)?;
            (if g.physical.tracks_gpu(&allocation) { 0 } else { allocation.bytes() }, claim)
        };
        relieve(&inner, Some(&self.token), extra, Some((allocation.generation(), device_cap)), true);
        let reservation = {
            let g = inner.lock().map_err(|_| AdmissionError::Retired)?;
            if !self.valid() {
                return Err(AdmissionError::Retired);
            }
            if g.map.len().saturating_add(g.gpu.len()) >= MAX_RECORDS {
                return Err(AdmissionError::Capacity);
            }
            if g.physical.gpu_bytes(allocation.generation()).checked_add(extra).is_none_or(|n| n > device_cap) {
                return Err(AdmissionError::Capacity);
            }
            g.physical.reserve_gpu(&allocation)?
        };
        let device = allocation.generation().reserve(&allocation, device_cap)?;
        Ok(GpuPending {
            device,
            claim,
            owner: self.clone(),
            reservation,
            slot: Arc::new(GpuSlot { payload, allocation, stage, persistence: None, ready: Mutex::new(Weak::new()), content: None, readback: None }),
        })
    }

    /// Exact content stage lookup. The bounded owner map remains the only strong store.
    pub fn resident(&self, key: u64, stage: GpuStage) -> Option<Arc<GpuSlot>> {
        self.resident_in(key, stage, None)
    }
    pub fn resident_in(&self, key: u64, stage: GpuStage, generation: Option<&GpuGeneration>) -> Option<Arc<GpuSlot>> {
        let weak = {
            let inner = self.inner.upgrade()?;
            let g = inner.lock().ok()?;
            if !self.valid() {
                return None;
            }
            g.gpu_content
                .get(&key)?
                .iter()
                .filter_map(|id| g.gpu.get(id))
                .find(|e| {
                    e.slot.content == Some(key)
                        && e.slot.stage == stage
                        && e.slot.allocation.generation().valid()
                        && generation.is_none_or(|g| e.slot.allocation.generation().same(g))
                })
                .map(|e| Arc::downgrade(&e.slot))?
        };
        let slot = weak.upgrade()?;
        self.get(&weak, slot.allocation.generation(), stage)
    }

    pub fn get(&self, weak: &Weak<GpuSlot>, generation: &GpuGeneration, stage: GpuStage) -> Option<Arc<GpuSlot>> {
        let slot = weak.upgrade()?; // declared before guards: drop always occurs after them
        if !generation.valid() || !slot.allocation.generation().same(generation) || slot.stage != stage {
            return None;
        }
        let inner = self.inner.upgrade()?;
        let mut g = inner.lock().ok()?;
        if !self.valid() {
            return None;
        }
        let id = Arc::as_ptr(&slot) as usize;
        if !g.gpu.get(&id).is_some_and(|e| Arc::ptr_eq(&e.slot, &slot)) {
            return None;
        }
        let now = g.tick();
        g.gpu_touch(id, now);
        Some(slot)
    }

    pub fn set_device_cap(&self, generation: &GpuGeneration, cap: usize) {
        generation.set_cap(cap);
        self.enforce_device_cap(generation, cap);
    }
    pub fn enforce_device_cap(&self, generation: &GpuGeneration, cap: usize) {
        if let Some(inner) = self.inner.upgrade() {
            relieve(&inner, Some(&self.token), 0, Some((generation, cap)), false);
        }
    }
    pub fn release(&self, weak: &Weak<GpuSlot>) {
        let Some(inner) = self.inner.upgrade() else { return };
        let retired = {
            let Ok(mut g) = inner.lock() else { return };
            g.gpu_remove(weak.as_ptr() as usize).map(|e| e.slot)
        };
        drop(retired);
    }

    /// Remove only this device incarnation; CPU allocations and other devices survive.
    pub fn retire_generation(&self, generation: &GpuGeneration) {
        generation.retire();
        let Some(inner) = self.inner.upgrade() else { return };
        loop {
            let retired = {
                let Ok(mut g) = inner.lock() else { return };
                let key = g.gpu.iter().find(|(_, e)| e.slot.allocation.generation().same(generation)).map(|(k, _)| *k);
                key.and_then(|k| g.gpu_remove(k)).map(|e| e.slot)
            };
            if retired.is_none() {
                break;
            }
            drop(retired);
        }
    }
}

/// Reservation and transient payload awaiting a successful producer submission.
pub struct GpuPending {
    device: crate::allocation::DeviceReservation,
    claim: crate::allocation::AllocationClaim,
    owner: GpuOwner,
    reservation: Reservation,
    slot: Arc<GpuSlot>,
}
impl GpuPending {
    pub fn with_persistence(mut self, intent: Option<crate::disk_cache::LayerWriteIntent>) -> Self {
        if let Some(slot) = Arc::get_mut(&mut self.slot) {
            slot.persistence = intent.map(crate::disk_cache::LayerWriteIntent::produced);
        }
        self
    }
    /// Attach the exact processed-content key before publication. Blend slots never materialize.
    pub fn with_content(mut self, key: u64, readback: Option<Arc<dyn ResidentReadback>>) -> Self {
        if let Some(slot) = Arc::get_mut(&mut self.slot) {
            slot.content = Some(key);
            slot.readback = readback;
        }
        self
    }

    pub fn publish(self) -> Result<Weak<GpuSlot>, AdmissionError> {
        let inner = self.owner.inner.upgrade().ok_or(AdmissionError::Retired)?;
        let mut g = inner.lock().map_err(|_| AdmissionError::Retired)?;
        if !self.owner.valid() || !self.slot.allocation.generation().valid() {
            return Err(AdmissionError::Retired);
        }
        if g.map.len().saturating_add(g.gpu.len()) >= MAX_RECORDS {
            return Err(AdmissionError::Capacity);
        }
        g.gpu.try_reserve(1).map_err(|_| AdmissionError::Allocation)?;
        self.device.commit(self.reservation)?;
        self.claim.commit();
        let now = g.tick();
        let weak = Arc::downgrade(&self.slot);
        let old = g.gpu_insert(Arc::as_ptr(&self.slot) as usize, GpuEntry { slot: self.slot, last_use: now, probation: true });
        drop(g);
        drop(old);
        Ok(weak)
    }
}

/// A common recency eviction; each removed backend payload is destroyed after unlock.
pub(super) fn relieve(inner: &Arc<Mutex<Inner>>, token: Option<&ResidentToken>, extra: usize, device: Option<(&GpuGeneration, usize)>, slot: bool) {
    loop {
        let (progress, retired) = {
            let Ok(mut g) = inner.lock() else { return };
            if token.is_some_and(|t| !t.valid()) {
                return;
            }
            let stats = g.physical.stats();
            let bytes_fit = stats.live_bytes.checked_add(stats.pending_bytes).and_then(|n| n.checked_add(extra)).is_some_and(|n| n <= g.budget);
            let device_fit =
                device.is_none_or(|(generation, cap)| generation.within_cap() && g.physical.gpu_bytes(generation).checked_add(extra).is_some_and(|n| n <= cap));
            let count_fit = !slot || g.map.len().saturating_add(g.gpu.len()) < MAX_RECORDS;
            if bytes_fit && device_fit && count_fit {
                return;
            }
            // Making room for a new entry keeps the current frames' working set (see `evict_one`).
            let protect = token.is_some() && (extra > 0 || slot);
            if bytes_fit
                && count_fit
                && let Some((generation, _)) = device
            {
                // A device ceiling cannot be relieved by discarding CPU pixels or
                // allocations from another context. Preserve those useful entries
                // when a pinned allocation prevents admission on this device.
                let since = if protect { g.working_since() } else { None };
                let on =
                    |id: &usize, probation: bool| g.gpu.get(id).is_some_and(|e| (!probation || e.probation) && e.slot.allocation.generation().same(generation));
                // Slots on probation first (see `evict_one`), then the least recently used.
                let key = g
                    .gpu_probation_lru
                    .iter()
                    .find(|(_, id)| on(id, true))
                    .filter(|(age, _)| since.is_none_or(|since| *age <= since))
                    .or_else(|| g.gpu_lru.iter().find(|(_, id)| on(id, false)).filter(|(age, _)| since.is_none_or(|since| *age <= since)))
                    .map(|(_, id)| *id);
                match key {
                    Some(key) => (true, g.gpu_remove(key).map(|e| e.slot)),
                    None => (false, None),
                }
            } else {
                super::evict_one(&mut g, protect)
            }
        };
        drop(retired);
        if !progress {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::LayerCache;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Probe {
        inner: Weak<Mutex<Inner>>,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            if let Some(inner) = self.inner.upgrade() {
                let g = inner.try_lock().expect("backend drop outside owner lock");
                let _ = g.physical.stats();
            }
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn allocation(g: &GpuGeneration) -> Arc<GpuAllocation> {
        GpuAllocation::rgba_f32(1, 1, g.clone()).unwrap()
    }
    fn probe(cache: &LayerCache, drops: &Arc<AtomicUsize>) -> Arc<dyn Any + Send + Sync> {
        Arc::new(Probe { inner: Arc::downgrade(&cache.inner), drops: drops.clone() })
    }
    struct Readback {
        cache: Weak<LayerCache>,
        calls: AtomicUsize,
        fail: bool,
        clear: bool,
    }
    impl ResidentReadback for Readback {
        fn readback(&self, _: &GpuSlot) -> Option<Buf> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let c = self.cache.upgrade().unwrap();
            assert!(c.inner.try_lock().is_ok(), "no owner lock during readback");
            assert!(c.inflight.try_lock().is_ok(), "no flight lock during readback");
            assert!(c.get(99).is_none(), "same-thread recursion must fall back");
            if self.clear {
                c.clear();
            }
            if self.fail {
                return None;
            }
            Some(Buf { img: effectcraft_raster::Image::new(1, 1), offset: [3.0, 4.0], scale: 0.5 })
        }
    }
    fn lazy(c: &Arc<LayerCache>, stage: GpuStage, readback: Arc<dyn ResidentReadback>) {
        let g = GpuGeneration::default();
        c.gpu_owner().unwrap().prepare(Arc::new(()), allocation(&g), stage, 1024).unwrap().with_content(99, Some(readback)).publish().unwrap();
    }
    #[test]
    fn one_use_slots_cannot_evict_established_ones() {
        // Room for four 16-byte slots. Two layers are used on two frames (admitted), then a
        // run of one-use results (a moving video's frames) is published and never used again:
        // they evict each other, never the established layers (the 4:29 cut, A8).
        let c = LayerCache::new(64);
        let owner = c.gpu_owner().unwrap();
        let g = GpuGeneration::default();
        let publish = || owner.prepare(Arc::new(()), allocation(&g), GpuStage::Blend, 1024).unwrap().publish().unwrap();
        let stable = [publish(), publish()];
        assert_eq!(c.gpu_admission(), (0, 2), "first publications are on probation");
        for s in &stable {
            assert!(owner.get(s, &g, GpuStage::Blend).is_some());
        }
        assert_eq!(c.gpu_admission(), (2, 0), "used again: admitted");
        let mut last = None;
        for _ in 0..32 {
            last = Some(publish());
        }
        for s in &stable {
            assert!(owner.get(s, &g, GpuStage::Blend).is_some(), "a one-use slot evicted an established one");
        }
        assert!(last.is_some_and(|l| owner.get(&l, &g, GpuStage::Blend).is_some()), "the newest one-use slot is kept");
        assert_eq!(c.gpu_admission(), (3, 1));
        assert_eq!(c.retained_stats().live_bytes, 64);
    }

    #[test]
    fn device_ceiling_evicts_one_use_slots_first() {
        let c = LayerCache::new(1 << 20);
        let owner = c.gpu_owner().unwrap();
        let g = GpuGeneration::default();
        let publish = || owner.prepare(Arc::new(()), allocation(&g), GpuStage::Blend, 48).unwrap().publish().unwrap();
        let stable = publish();
        assert!(owner.get(&stable, &g, GpuStage::Blend).is_some());
        for _ in 0..8 {
            publish();
        }
        assert!(owner.get(&stable, &g, GpuStage::Blend).is_some(), "the device ceiling evicted an established slot first");
        assert_eq!(c.stats().gpu_entries, 3);
    }

    #[test]
    fn device_only_pressure_preserves_cpu_and_other_generation_entries() {
        let c = LayerCache::new(1024);
        let cpu = Arc::new(Buf { img: effectcraft_raster::Image::new(1, 1), offset: [0.0; 2], scale: 1.0 });
        c.insert(7, cpu);
        let owner = c.gpu_owner().unwrap();
        let other = GpuGeneration::default();
        let other_slot = owner.prepare(Arc::new(()), allocation(&other), GpuStage::Blend, 1024).unwrap().publish().unwrap();
        let generation = GpuGeneration::default();
        let pinned = allocation(&generation);
        owner.prepare(Arc::new(()), pinned.clone(), GpuStage::Blend, 16).unwrap().publish().unwrap();
        assert!(matches!(owner.prepare(Arc::new(()), allocation(&generation), GpuStage::Blend, 16), Err(AdmissionError::Capacity)));
        assert!(c.get(7).is_some(), "evicting CPU pixels cannot relieve a device-only ceiling");
        assert!(owner.get(&other_slot, &other, GpuStage::Blend).is_some(), "another device's pixels cannot relieve this device ceiling");
        drop(pinned);
    }

    #[test]
    fn lazy_processed_is_demand_driven_reentrant_and_reuses_cpu() {
        let c = Arc::new(LayerCache::new(1024));
        let read = Arc::new(Readback { cache: Arc::downgrade(&c), calls: AtomicUsize::new(0), fail: false, clear: false });
        lazy(&c, GpuStage::Processed, read.clone());
        assert_eq!(read.calls.load(Ordering::Relaxed), 0);
        let b = c.get(99).unwrap();
        assert_eq!(b.offset, [3.0, 4.0]);
        assert_eq!(b.scale, 0.5);
        assert!(Arc::ptr_eq(&b, &c.get(99).unwrap()));
        assert_eq!(read.calls.load(Ordering::Relaxed), 1);
        assert_eq!(c.retained_stats().live_bytes, 32);
    }
    #[test]
    fn lazy_failure_releases_flight_and_blend_never_materializes() {
        let c = Arc::new(LayerCache::new(1024));
        let read = Arc::new(Readback { cache: Arc::downgrade(&c), calls: AtomicUsize::new(0), fail: true, clear: false });
        lazy(&c, GpuStage::Processed, read.clone());
        assert!(c.get(99).is_none());
        assert!(c.get(99).is_none());
        assert_eq!(read.calls.load(Ordering::Relaxed), 2);
        assert!(c.inflight.lock().unwrap().map.is_empty());
        c.clear();
        lazy(&c, GpuStage::Blend, read.clone());
        assert!(c.get(99).is_none());
        assert_eq!(read.calls.load(Ordering::Relaxed), 2);
    }
    #[test]
    fn lazy_clear_during_readback_cannot_republish() {
        let c = Arc::new(LayerCache::new(1024));
        let read = Arc::new(Readback { cache: Arc::downgrade(&c), calls: AtomicUsize::new(0), fail: false, clear: true });
        lazy(&c, GpuStage::Processed, read);
        let transient = c.get(99).unwrap();
        assert_eq!(transient.offset, [3.0, 4.0]);
        assert!(c.get(99).is_none());
        assert_eq!(c.retained_stats().live_bytes, 0);
    }
    #[test]
    fn lazy_weak_handoff_survives_cpu_admission_denial() {
        let c = Arc::new(LayerCache::new(32));
        let read = Arc::new(Readback { cache: Arc::downgrade(&c), calls: AtomicUsize::new(0), fail: false, clear: false });
        lazy(&c, GpuStage::Processed, read.clone());
        let first = c.get(99).unwrap();
        assert!(c.peek(99).is_none(), "CPU exceeds quarter-budget admission limit");
        let second = c.get(99).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(read.calls.load(Ordering::Relaxed), 1);
        assert_eq!(c.retained_stats().live_bytes, 16, "handoff is weak; CPU caller-only transient is not retained");
        drop(first);
        drop(second);
        assert!(c.get(99).is_some());
        assert_eq!(read.calls.load(Ordering::Relaxed), 2);
    }
    #[test]
    fn resident_lookup_selects_exact_device_generation() {
        let c = LayerCache::new(1024);
        let owner = c.gpu_owner().unwrap();
        let a = GpuGeneration::default();
        let b = GpuGeneration::default();
        for (generation, value) in [(&a, 1_u32), (&b, 2_u32)] {
            owner.prepare(Arc::new(value), allocation(generation), GpuStage::Processed, 1024).unwrap().with_content(99, None).publish().unwrap();
        }
        assert_eq!(owner.resident_in(99, GpuStage::Processed, Some(&a)).unwrap().payload::<u32>(), Some(&1));
        assert_eq!(owner.resident_in(99, GpuStage::Processed, Some(&b)).unwrap().payload::<u32>(), Some(&2));
        assert!(owner.resident_in(99, GpuStage::Blend, Some(&b)).is_none());
    }
    #[test]
    fn lazy_disk_write_is_demand_only_and_purge_cancels_denied_cpu_result() {
        struct DiskRead {
            read: Readback,
            disk: Arc<crate::disk_cache::DiskCache>,
            purge: bool,
        }
        impl ResidentReadback for DiskRead {
            fn readback(&self, slot: &GpuSlot) -> Option<Buf> {
                if self.purge {
                    self.disk.clear();
                }
                self.read.readback(slot)
            }
        }
        for purge in [false, true] {
            let dir = std::env::temp_dir().join(format!("rt1-lazy-disk-{}-{purge}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let disk = crate::disk_cache::DiskCache::open(&dir, 1 << 20).unwrap();
            disk.set_min_layer_ms(0);
            let c = Arc::new(LayerCache::new(32));
            c.set_disk(Some((disk.clone(), 17)));
            let intent = c.disk_intent(99).unwrap();
            let read = Arc::new(DiskRead {
                read: Readback { cache: Arc::downgrade(&c), calls: AtomicUsize::new(0), fail: false, clear: false },
                disk: disk.clone(),
                purge,
            });
            let generation = GpuGeneration::default();
            c.gpu_owner()
                .unwrap()
                .prepare(Arc::new(()), allocation(&generation), GpuStage::Processed, 1024)
                .unwrap()
                .with_content(99, Some(read.clone()))
                .with_persistence(Some(intent))
                .publish()
                .unwrap();
            disk.flush();
            assert_eq!(read.read.calls.load(Ordering::Relaxed), 0);
            assert_eq!(disk.stats().writes, 0, "GPU retention does not populate disk");
            let result = c.get(99).unwrap();
            disk.flush();
            assert!(c.peek(99).is_none(), "CPU admission denied");
            assert_eq!(Arc::strong_count(&result), 1, "disk queue never owns pixels");
            assert_eq!(disk.stats().writes, u64::from(!purge));
            let saved = disk.get_layer((17_u128 << 64) | 99);
            assert_eq!(saved.is_some(), !purge);
            if let Some(saved) = saved {
                assert_eq!(saved.img.data, result.img.data);
                assert_eq!(saved.offset, result.offset);
            }
            c.clear();
            disk.clear();
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn lazy_postwake_competing_failed_flights_never_wait_again() {
        let c = Arc::new(LayerCache::new(1024));
        let read = Arc::new(Readback { cache: Arc::downgrade(&c), calls: AtomicUsize::new(0), fail: true, clear: false });
        lazy(&c, GpuStage::Processed, read.clone());
        // Model an adversarial producer winning the key immediately after every
        // wake. Post-wake acquisition must return while that producer still holds
        // its guard, without starting a fresh 2/30-second wait or recursive call.
        for _ in 0..16 {
            let guard = match c.claim_gpu(99) {
                super::super::Claim::Owner(g) => g,
                _ => panic!("owner"),
            };
            let (done, done_rx) = std::sync::mpsc::channel();
            let other = c.clone();
            let waiter = std::thread::spawn(move || {
                let result = other.materialize_resident_inner(99, false);
                done.send(result.is_none()).unwrap();
            });
            assert!(done_rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap());
            drop(guard); // failed producer wakes all waiters
            waiter.join().unwrap();
        }
        assert_eq!(read.calls.load(Ordering::Relaxed), 0);
        assert!(c.get(99).is_none());
        assert_eq!(read.calls.load(Ordering::Relaxed), 1);
        assert!(c.inflight.lock().unwrap().map.is_empty());
    }
    #[test]
    fn lazy_concurrent_cpu_demand_coalesces() {
        struct Blocking {
            entered: std::sync::mpsc::Sender<()>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
            calls: AtomicUsize,
        }
        impl ResidentReadback for Blocking {
            fn readback(&self, _: &GpuSlot) -> Option<Buf> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                Some(Buf { img: effectcraft_raster::Image::new(1, 1), offset: [0.0; 2], scale: 1.0 })
            }
        }
        let c = Arc::new(LayerCache::new(1024));
        let (entered, entered_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let read = Arc::new(Blocking { entered, release: Mutex::new(release_rx), calls: AtomicUsize::new(0) });
        lazy(&c, GpuStage::Processed, read.clone());
        let a = c.clone();
        let first = std::thread::spawn(move || a.get(99).unwrap());
        entered_rx.recv().unwrap();
        let b = c.clone();
        let second = std::thread::spawn(move || b.get(99).unwrap());
        release.send(()).unwrap();
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(read.calls.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn gpu_alias_pins_and_clear_drop_after_unlock() {
        let c = LayerCache::new(64);
        let o = c.gpu_owner().unwrap();
        let g = GpuGeneration::default();
        let a = allocation(&g);
        let drops = Arc::new(AtomicUsize::new(0));
        let x = o.prepare(probe(&c, &drops), a.clone(), GpuStage::Blend, 64).unwrap().publish().unwrap();
        let y = o.prepare(probe(&c, &drops), a.clone(), GpuStage::Blend, 64).unwrap().publish().unwrap();
        assert_eq!(c.retained_stats().live_bytes, 16);
        let pin = o.get(&x, &g, GpuStage::Blend).unwrap();
        c.clear();
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(o.get(&y, &g, GpuStage::Blend).is_none());
        assert_eq!(c.retained_stats().live_bytes, 16);
        drop(a);
        drop(pin);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        assert_eq!(c.retained_stats().live_bytes, 0);
    }
    #[test]
    fn gpu_canceled_and_stale_publication_release_outside_locks() {
        let c = LayerCache::new(64);
        let o = c.gpu_owner().unwrap();
        let g = GpuGeneration::default();
        let drops = Arc::new(AtomicUsize::new(0));
        let p = o.prepare(probe(&c, &drops), allocation(&g), GpuStage::Blend, 64).unwrap();
        assert_eq!(c.retained_stats().pending_bytes, 16);
        drop(p);
        assert_eq!(c.retained_stats().pending_bytes, 0);
        let p = o.prepare(probe(&c, &drops), allocation(&g), GpuStage::Blend, 64).unwrap();
        c.clear();
        assert!(matches!(p.publish(), Err(AdmissionError::Retired)));
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        assert_eq!(c.retained_stats().pending_bytes, 0);
    }
    #[test]
    fn gpu_cross_owner_denial_preserves_existing_slots() {
        let a = LayerCache::new(16);
        let b = LayerCache::new(16);
        let g = GpuGeneration::default();
        let ao = a.gpu_owner().unwrap();
        let bo = b.gpu_owner().unwrap();
        let allocation = allocation(&g);
        let drops = Arc::new(AtomicUsize::new(0));
        ao.prepare(probe(&a, &drops), allocation.clone(), GpuStage::Blend, 32).unwrap().publish().unwrap();
        let old = bo.prepare(probe(&b, &drops), super::tests::allocation(&g), GpuStage::Blend, 32).unwrap().publish().unwrap();
        assert!(matches!(bo.prepare(probe(&b, &drops), allocation, GpuStage::Blend, 32), Err(AdmissionError::ForeignOwner)));
        assert!(bo.get(&old, &g, GpuStage::Blend).is_some());
    }
    #[test]
    fn gpu_eviction_shrink_and_generation_retirement_preserve_cpu() {
        let c = LayerCache::new(64);
        let o = c.gpu_owner().unwrap();
        let g = GpuGeneration::default();
        let drops = Arc::new(AtomicUsize::new(0));
        let cpu = Arc::new(crate::Buf { img: effectcraft_raster::Image::new(1, 1), offset: [0.0; 2], scale: 1.0 });
        c.insert(7, cpu.clone());
        let x = o.prepare(probe(&c, &drops), allocation(&g), GpuStage::Blend, 64).unwrap().publish().unwrap();
        o.retire_generation(&g);
        assert!(o.get(&x, &g, GpuStage::Blend).is_none());
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(c.get(7).is_some());
        let g = GpuGeneration::default();
        let x = o.prepare(probe(&c, &drops), allocation(&g), GpuStage::Blend, 64).unwrap().publish().unwrap();
        let pin = o.get(&x, &g, GpuStage::Blend).unwrap();
        c.set_budget(0);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert_eq!(c.retained_stats().debt_bytes, 32);
        drop(pin);
        drop(cpu);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        assert_eq!(c.retained_stats().live_bytes, 0);
    }
    #[test]
    fn gpu_generation_rejects_pending_and_wrong_stage() {
        let c = LayerCache::new(64);
        let o = c.gpu_owner().unwrap();
        let g = GpuGeneration::default();
        let drops = Arc::new(AtomicUsize::new(0));
        let x = o.prepare(probe(&c, &drops), allocation(&g), GpuStage::Blend, 64).unwrap().publish().unwrap();
        assert!(o.get(&x, &g, GpuStage::Processed).is_none());
        assert!(o.get(&x, &GpuGeneration::default(), GpuStage::Blend).is_none());
        let pending = o.prepare(probe(&c, &drops), allocation(&g), GpuStage::Blend, 64).unwrap();
        o.retire_generation(&g);
        assert!(matches!(pending.publish(), Err(AdmissionError::Retired)));
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }
    #[test]
    fn gpu_shared_recency_and_pending_cap() {
        let c = LayerCache::new(32);
        let o = c.gpu_owner().unwrap();
        let g = GpuGeneration::default();
        let drops = Arc::new(AtomicUsize::new(0));
        let a = o.prepare(probe(&c, &drops), allocation(&g), GpuStage::Blend, 32).unwrap().publish().unwrap();
        let b = o.prepare(probe(&c, &drops), allocation(&g), GpuStage::Blend, 32).unwrap().publish().unwrap();
        drop(o.get(&a, &g, GpuStage::Blend).unwrap());
        let pending = o.prepare(probe(&c, &drops), allocation(&g), GpuStage::Blend, 32).unwrap();
        assert!(o.get(&b, &g, GpuStage::Blend).is_none());
        assert!(o.get(&a, &g, GpuStage::Blend).is_some());
        assert_eq!(c.retained_stats().pending_bytes, 16);
        pending.publish().unwrap();
        assert_eq!(c.retained_stats().live_bytes, 32);
    }
    #[test]
    fn gpu_pending_device_cap_and_canceled_owner_claim() {
        let a = LayerCache::new(128);
        let b = LayerCache::new(128);
        let ao = a.gpu_owner().unwrap();
        let bo = b.gpu_owner().unwrap();
        let g = GpuGeneration::default();
        let drops = Arc::new(AtomicUsize::new(0));
        let shared = GpuAllocation::rgba_f32(2, 1, g.clone()).unwrap();
        let old = bo.prepare(probe(&b, &drops), allocation(&g), GpuStage::Blend, 48).unwrap().publish().unwrap();
        let pending = ao.prepare(probe(&a, &drops), shared.clone(), GpuStage::Blend, 48).unwrap();
        assert!(matches!(bo.prepare(probe(&b, &drops), shared.clone(), GpuStage::Blend, 48), Err(AdmissionError::ForeignOwner)));
        assert!(bo.get(&old, &g, GpuStage::Blend).is_some());
        let second = GpuAllocation::rgba_f32(2, 1, g.clone()).unwrap();
        assert!(matches!(ao.prepare(probe(&a, &drops), second, GpuStage::Blend, 48), Err(AdmissionError::Capacity)));
        assert_eq!(a.retained_stats().pending_bytes, 32);
        drop(pending);
        assert_eq!(a.retained_stats().pending_bytes, 0);
        bo.prepare(probe(&b, &drops), shared, GpuStage::Blend, 48).unwrap().publish().unwrap();
    }
    #[test]
    fn gpu_global_ceiling_shrink_aliases_and_rollback() {
        let a = LayerCache::new(128);
        let b = LayerCache::new(128);
        let ao = a.gpu_owner().unwrap();
        let bo = b.gpu_owner().unwrap();
        let g = GpuGeneration::default();
        let drops = Arc::new(AtomicUsize::new(0));
        let image = GpuAllocation::rgba_f32(3, 1, g.clone()).unwrap();
        let p = ao.prepare(probe(&a, &drops), image.clone(), GpuStage::Blend, 64).unwrap();
        let alias = ao.prepare(probe(&a, &drops), image.clone(), GpuStage::Blend, 64).unwrap();
        assert_eq!(a.retained_stats().pending_bytes, 48);
        let other = GpuAllocation::rgba_f32(3, 1, g.clone()).unwrap();
        assert!(matches!(bo.prepare(probe(&b, &drops), other.clone(), GpuStage::Blend, 64), Err(AdmissionError::Capacity)));
        assert_eq!(b.retained_stats().pending_bytes, 0);
        drop(p);
        assert_eq!(a.retained_stats().pending_bytes, 48);
        alias.publish().unwrap();
        assert_eq!(a.retained_stats().live_bytes, 48);
        a.clear();
        drop(image);
        let pending = bo.prepare(probe(&b, &drops), other, GpuStage::Blend, 64).unwrap();
        bo.set_device_cap(&g, 16);
        assert!(matches!(pending.publish(), Err(AdmissionError::Capacity)));
        assert_eq!(b.retained_stats().pending_bytes, 0);
        assert!(g.within_cap());
        g.set_cap(64);
        let pending = bo.prepare(probe(&b, &drops), allocation(&g), GpuStage::Blend, 64).unwrap();
        b.clear();
        assert!(matches!(pending.publish(), Err(AdmissionError::Retired)));
        assert!(g.within_cap());
    }
    #[test]
    fn gpu_published_alias_discharges_pending_bytes_once() {
        for publish_alias in [false, true] {
            let c = LayerCache::new(64);
            let o = c.gpu_owner().unwrap();
            let g = GpuGeneration::default();
            let a = GpuAllocation::rgba_f32(3, 1, g.clone()).unwrap();
            let drops = Arc::new(AtomicUsize::new(0));
            let first = o.prepare(probe(&c, &drops), a.clone(), GpuStage::Blend, 64).unwrap();
            let second = o.prepare(probe(&c, &drops), a.clone(), GpuStage::Blend, 64).unwrap();
            assert_eq!(c.retained_stats().pending_bytes, 48);
            first.publish().unwrap();
            assert_eq!((c.retained_stats().live_bytes, c.retained_stats().pending_bytes), (48, 0));
            if publish_alias {
                second.publish().unwrap();
            } else {
                drop(second);
            }
            assert_eq!((c.retained_stats().live_bytes, c.retained_stats().pending_bytes), (48, 0));
            assert!(g.within_cap());
        }
    }
}
