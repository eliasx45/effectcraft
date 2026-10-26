//! Event-driven Auto scheduling. Manual sections never enter the verifier. The UI holds
//! only accepted proofs; filesystem checks and property serialization run on a worker.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use effectcraft_project::Project;
use serde_json::json;

use crate::{Session, freeze::Capture};

pub struct Proof {
    pub revision: u64,
    pub capture: Arc<Capture>,
    pub checked: web_time::Instant,
}

pub struct Runtime {
    pub latest_source: Arc<Mutex<(u64, Arc<Project>)>>,
    pub feedback: crate::freeze::Feedback,
    pub bad_versions: BTreeSet<String>,
    pub proofs: BTreeMap<u64, Proof>,
    pub reasons: BTreeMap<u64, String>,
    pub generation: u64,
    pub pixel_epochs: BTreeMap<u64, u64>,
    pixel_keys: Mutex<BTreeMap<(usize, u64, effectcraft_project::ItemId), Option<u64>>>,
    pub pause_until: Arc<AtomicU64>,
    pub render_slot: Arc<Mutex<()>>,
    pub changed: web_time::Instant,
    pub activity: web_time::Instant,
    pub retry_after: BTreeMap<u64, web_time::Instant>,
    /// Render times of the layers the viewer drew, for `freeze.suggest`.
    pub cost: crate::freeze_cost::CostBook,
    font_revision: u64,
    view: Mutex<Option<(u64, usize, Arc<Project>)>>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            latest_source: Default::default(),
            feedback: Default::default(),
            bad_versions: Default::default(),
            proofs: BTreeMap::new(),
            reasons: BTreeMap::new(),
            generation: 0,
            pixel_epochs: BTreeMap::new(),
            pixel_keys: Mutex::new(BTreeMap::new()),
            pause_until: Arc::default(),
            render_slot: Arc::default(),
            changed: web_time::Instant::now(),
            activity: web_time::Instant::now(),
            retry_after: BTreeMap::new(),
            cost: Default::default(),
            font_revision: effectcraft_text::fonts::revision(),
            view: Mutex::new(None),
        }
    }
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64).unwrap_or(0)
}

impl Runtime {
    pub fn changed(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.changed = web_time::Instant::now();
    }

    pub fn activity(&mut self) {
        self.activity = web_time::Instant::now();
        self.pause_until.store(now_millis().saturating_add(2500), Ordering::Relaxed);
    }

    pub fn accept(&mut self, id: u64, revision: u64, capture: Arc<Capture>) {
        self.proofs.insert(id, Proof { revision, capture, checked: web_time::Instant::now() });
        self.reasons.remove(&id);
        self.invalidate_section(id);
    }

    pub fn invalidate_section(&mut self, id: u64) {
        let epoch = self.pixel_epochs.entry(id).or_default();
        *epoch = epoch.saturating_add(1);
        self.generation = self.generation.saturating_add(1);
    }

    /// Pixel changes outside document edits, scoped to the viewer's reachable sections.
    /// Memoization keeps dependency walks out of steady-state UI frames.
    pub fn pixel_epoch(&self, project: &Arc<Project>, comp: effectcraft_project::ItemId) -> Option<u64> {
        if self.pixel_epochs.is_empty() || project.freeze.sections.is_empty() {
            return None;
        }
        let fonts = effectcraft_text::fonts::revision();
        let key = (Arc::as_ptr(project) as usize, self.generation.saturating_add(fonts), comp);
        let mut keys = self.pixel_keys.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(value) = keys.get(&key) {
            return *value;
        }
        use std::hash::{Hash, Hasher};
        let scope = crate::freeze::reachable_comps(project, comp);
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        let mut any = false;
        for section in project.freeze.sections.iter().filter(|s| s.enabled && scope.contains(&s.target.comp())) {
            if let Some(epoch) = self.pixel_epochs.get(&section.id) {
                (section.id, epoch, if section.auto { fonts } else { 0 }).hash(&mut hash);
                any = true;
            }
        }
        let value = any.then(|| hash.finish());
        if keys.len() >= 64 {
            keys.clear();
        }
        keys.insert(key, value);
        value
    }

    pub fn preview(&self, project: &Arc<Project>, revision: u64) -> Arc<Project> {
        if !project.freeze.sections.iter().any(|s| s.auto || s.cache.as_ref().is_some_and(|v| self.bad_versions.contains(&v.directory))) {
            return project.clone();
        }
        let address = Arc::as_ptr(project) as usize;
        let view_generation = self.generation.saturating_add(effectcraft_text::fonts::revision());
        let mut view = self.view.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((generation, original, projected)) = view.as_ref()
            && *generation == view_generation
            && *original == address
        {
            return projected.clone();
        }
        let mut projected = (**project).clone();
        for section in &mut projected.freeze.sections {
            if section.cache.as_ref().is_some_and(|v| self.bad_versions.contains(&v.directory)) {
                section.cache = None;
            }
            if !section.auto {
                continue;
            }
            let valid = self.proofs.get(&section.id).is_some_and(|proof| {
                proof.revision == revision && proof.capture.fonts_current() && section.cache.as_ref().is_some_and(|v| v.identity == proof.capture.identity)
            });
            section.auto = false;
            if !valid {
                section.cache = None;
            }
        }
        let projected = Arc::new(projected);
        *view = Some((view_generation, address, projected.clone()));
        projected
    }
}

impl Session {
    /// Automatic caches are derived preview data, not an authoring undo step. Preserve the
    /// editor's history/author and its verified analyses while publishing the new cache.
    pub(crate) fn attach_automatic_freeze(&mut self, id: u64, version: effectcraft_project::freeze::Version) -> crate::Result<()> {
        let before = self.project.clone();
        let intent = before.freeze.sections.iter().find(|s| s.id == id).ok_or(crate::EngineError::NoComp)?;
        let (start, end) = intent.range(before.comp(intent.target.comp()).ok_or(crate::EngineError::NoComp)?.duration).map_err(crate::EngineError::Other)?;
        let revision = self.revision;
        let author = self.history.author(&before).map(str::to_owned);
        let mut project = (*before).clone();
        let section = project.freeze.sections.iter_mut().find(|s| s.id == id).ok_or(crate::EngineError::NoComp)?;
        section.start = start;
        section.end = end;
        section.previous.retain(|v| v.directory != version.directory);
        if let Some(old) = section.cache.replace(version) {
            section.previous.push(old);
        }
        if section.previous.len() > 32 {
            section.previous.remove(0);
        }
        section.enabled = true;
        self.project = Arc::new(project);
        self.history.set_author(&self.project, author);
        self.analyses_checked = Arc::downgrade(&self.project);
        self.bump();
        for proof in self.freeze_runtime.proofs.values_mut().filter(|p| p.revision == revision) {
            proof.revision = self.revision;
        }
        Ok(())
    }
    pub fn freeze_preview_project(&self) -> Arc<Project> {
        self.freeze_runtime.preview(&self.project, self.revision)
    }

    pub fn freeze_preview_revision(&self) -> u64 {
        self.freeze_runtime.generation.saturating_add(effectcraft_text::fonts::revision())
    }

    /// Call from the UI heartbeat (or a headless controller). No recurring work in manual mode.
    pub fn poll_freeze(&mut self) {
        let faults = {
            let mut queue = match self.freeze_runtime.feedback.try_lock() {
                Ok(q) => q,
                Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => return,
            };
            std::mem::take(&mut *queue)
        };
        for fault in faults {
            if fault.namespace == self.project.freeze.namespace
                && self.project.freeze.sections.iter().any(|s| s.id == fault.section && s.cache.as_ref().is_some_and(|v| v.directory == fault.directory))
                && self.freeze_runtime.bad_versions.insert(fault.directory)
            {
                self.freeze_runtime.reasons.insert(fault.section, fault.message);
                self.freeze_runtime.proofs.remove(&fault.section);
                self.freeze_runtime.invalidate_section(fault.section);
                if self.freeze_runtime.bad_versions.len() > 32_768 {
                    self.freeze_runtime.bad_versions.pop_first();
                }
            }
        }
        let fonts = effectcraft_text::fonts::revision();
        if self.freeze_runtime.font_revision != fonts {
            self.freeze_runtime.font_revision = fonts;
            if self.freeze_runtime.proofs.values().any(|p| p.capture.font_generation.is_some()) {
                self.layer_cache.clear();
            }
        }
        self.auto_freeze_heavy();
        if cfg!(target_arch = "wasm32") || !self.project.freeze.sections.iter().any(|s| s.auto && s.enabled) {
            return;
        }
        if self.tasks.iter().any(|t| matches!(t.kind, "smartFreeze" | "freezeCheck" | "freezeStorage")) {
            return;
        }
        let now = web_time::Instant::now();
        if now.duration_since(self.freeze_runtime.activity) < Duration::from_millis(2500) {
            return;
        }
        let section = self
            .project
            .freeze
            .sections
            .iter()
            .find(|s| {
                if !s.auto || !s.enabled || self.freeze_runtime.retry_after.get(&s.id).is_some_and(|until| now < *until) {
                    return false;
                }
                if self.claims.get(s.target.comp()).is_some() {
                    return false;
                }
                self.freeze_runtime.proofs.get(&s.id).is_none_or(|proof| {
                    proof.revision != self.revision
                        || !proof.capture.fonts_current()
                        || !proof.capture.media.is_empty() && now.duration_since(proof.checked) >= Duration::from_secs(15)
                })
            })
            .cloned();
        let Some(section) = section else {
            return;
        };
        // Stop using media-backed versions before statting files, never while a stale proof
        // remains visible. Graphics-only versions have no periodic checks.
        let previous = self.freeze_runtime.proofs.remove(&section.id);
        self.freeze_runtime.reasons.insert(section.id, "Checking dependencies".into());
        self.freeze_runtime.invalidate_section(section.id);
        let (project, revision, epoch) = (self.project.clone(), self.revision, self.epoch);
        let footage = self.footage.clone();
        let result = self.spawn_task("freezeCheck", format!("Check {}", section.name), false, move |ctl| {
            if ctl.cancelled() {
                return Err("Smart Freeze check cancelled".into());
            }
            let captured = Capture::new(project, &section).map(Arc::new);
            let mut media_changed = false;
            if let Ok(capture) = &captured {
                let old: BTreeMap<_, _> = previous.as_ref().into_iter().flat_map(|p| &p.capture.media).map(|s| (s.path.as_str(), s)).collect();
                for stamp in &capture.media {
                    if old.get(stamp.path.as_str()).is_none_or(|before| **before != *stamp) {
                        footage.forget(&stamp.path);
                        media_changed = true;
                    }
                }
            }
            Ok(Box::new(move |s| {
                if s.epoch != epoch || s.revision != revision {
                    return Ok(json!({"stale":true}));
                }
                if s.claims.get(section.target.comp()).is_some() {
                    s.freeze_runtime.reasons.insert(section.id, "Auto waits while another editor owns this composition".into());
                    return Ok(json!({"paused":"claimed"}));
                }
                match captured {
                    Ok(capture) => {
                        if media_changed {
                            s.layer_cache.clear();
                            s.events.push(crate::Event::PurgeCaches);
                            s.freeze_runtime.invalidate_section(section.id);
                        }
                        if section.cache.as_ref().is_some_and(|v| v.identity == capture.identity && !s.freeze_runtime.bad_versions.contains(&v.directory)) {
                            s.freeze_runtime.accept(section.id, revision, capture);
                        } else if let Some(version) = section
                            .previous
                            .iter()
                            .rev()
                            .find(|v| !v.draft && v.identity == capture.identity && !s.freeze_runtime.bad_versions.contains(&v.directory))
                            .cloned()
                        {
                            s.attach_automatic_freeze(section.id, version)?;
                            s.freeze_runtime.accept(section.id, s.revision, capture);
                        } else {
                            s.freeze_runtime.reasons.insert(section.id, "Content changed · Refresh".into());
                            if section.cache.as_ref().is_some_and(|v| v.sample.as_ref().is_some_and(|sample| !sample.worth_freezing(v.frame_rate))) {
                                s.freeze_runtime
                                    .reasons
                                    .insert(section.id, "Last sample did not justify Auto; originals stay live. Refresh to measure again.".into());
                                s.freeze_runtime.retry_after.insert(section.id, web_time::Instant::now() + Duration::from_secs(60));
                                return Ok(json!({"section":section.id,"skipped":"measured cost"}));
                            }
                            if s.freeze_runtime.changed.elapsed() >= Duration::from_secs(8) && s.freeze_runtime.activity.elapsed() >= Duration::from_secs(8) {
                                let result = s.execute("freeze.refresh", json!({"section":section.id,"automatic":true}));
                                // Avoid a failing Auto target retrying every UI frame.
                                s.freeze_runtime.retry_after.insert(section.id, web_time::Instant::now() + Duration::from_secs(60));
                                result?;
                            } else {
                                s.freeze_runtime.retry_after.insert(section.id, web_time::Instant::now() + Duration::from_secs(1));
                            }
                        }
                    }
                    Err(reason) => {
                        s.freeze_runtime.reasons.insert(section.id, reason);
                        s.freeze_runtime.retry_after.insert(section.id, web_time::Instant::now() + Duration::from_secs(60));
                    }
                }
                Ok(json!({"section":section.id}))
            }))
        });
        if let Err(e) = result {
            self.toast(format!("Smart Freeze: {e}"));
        }
    }
}
