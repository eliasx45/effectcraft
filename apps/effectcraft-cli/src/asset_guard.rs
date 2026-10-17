//! The frame server refuses frames it would render wrong (CR7): footage that isn't on this
//! machine renders colour bars and a missing font renders Inter, both silently, so while the
//! replica's project needs anything this machine lacks, every frame fails with what is missing.
//! An item previewed through its proxy needs only the proxy (the request's Proxy Use decides).
//! The Mac then shows its own frame (CR5 falls back to This Mac). `EFFECTCRAFT_ALLOW_MISSING=1`
//! turns the guard off (debugging only).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use effectcraft_automation::frame_server::{FrameRender, LogFn, RenderJob, Rendered};
use effectcraft_engine::cloud_assets;
use effectcraft_engine::project::Project;
use effectcraft_engine::project::render_queue::ProxyUse;

/// How long a check holds for the same project (files can appear on disk meanwhile).
const RECHECK: Duration = Duration::from_secs(2);

struct Checked {
    key: (usize, usize, u64, ProxyUse),
    at: Instant,
    blocked: Option<String>,
}

pub struct AssetGuard {
    inner: Arc<dyn FrameRender>,
    log: LogFn,
    last: Mutex<(Option<Checked>, cloud_assets::BlockerCache)>,
}

/// `render` behind the guard, unless `EFFECTCRAFT_ALLOW_MISSING` is set.
pub fn wrap(render: Arc<dyn FrameRender>, log: LogFn) -> Arc<dyn FrameRender> {
    if std::env::var("EFFECTCRAFT_ALLOW_MISSING").is_ok_and(|v| !v.is_empty() && v != "0") {
        log("EFFECTCRAFT_ALLOW_MISSING: frames render even with missing footage or fonts");
        return render;
    }
    Arc::new(AssetGuard { inner: render, log, last: Mutex::new((None, Default::default())) })
}

impl AssetGuard {
    fn blocked(&self, project: &Project, proxy_use: ProxyUse) -> Option<String> {
        // The replica's project: a new one per revision (identity + a cheap fingerprint).
        let key = (std::ptr::from_ref(project) as usize, project.items.len(), project.next_id, proxy_use);
        let mut guard = self.last.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (last, cache) = &mut *guard;
        if let Some(c) = last.as_ref().filter(|c| c.key == key && c.at.elapsed() < RECHECK) {
            return c.blocked.clone();
        }
        let t0 = Instant::now();
        let missing = cloud_assets::blockers_cached(project, proxy_use, cache);
        let blocked = (!missing.is_empty()).then(|| format!("missing on the cloud machine: {}", cloud_assets::summary(&missing)));
        let was = last.as_ref().map(|c| c.blocked.clone());
        if was.as_ref() != Some(&blocked) {
            match &blocked {
                Some(b) => (self.log)(&format!("refusing frames, {b} (checked in {:.1} ms)", t0.elapsed().as_secs_f64() * 1e3)),
                None => (self.log)(&format!("footage and fonts all present (checked in {:.1} ms)", t0.elapsed().as_secs_f64() * 1e3)),
            }
        }
        *last = Some(Checked { key, at: Instant::now(), blocked: blocked.clone() });
        blocked
    }
}

impl FrameRender for AssetGuard {
    fn render(&self, job: &RenderJob<'_>) -> Result<(Rendered, bool), String> {
        if let Some(why) = self.blocked(job.project, job.opts.proxy) {
            return Err(why);
        }
        self.inner.render(job)
    }

    fn gpu_name(&self) -> Option<String> {
        self.inner.gpu_name()
    }

    fn warm(&self, project: &effectcraft_engine::project::Project, comp: effectcraft_engine::project::ItemId, time: effectcraft_engine::time::Tick) {
        self.inner.warm(project, comp, time);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use effectcraft_engine::render::{LayerCache, RenderOpts};
    use effectcraft_engine::time::Tick;
    use serde_json::json;

    struct Fine;
    impl FrameRender for Fine {
        fn render(&self, _: &RenderJob<'_>) -> Result<(Rendered, bool), String> {
            Ok((Rendered::Rgba8 { width: 1, height: 1, bytes: vec![0; 4] }, false))
        }
    }

    fn render(guard: &Arc<dyn FrameRender>, s: &effectcraft_engine::Session) -> Result<(Rendered, bool), String> {
        let cache = LayerCache::new(1 << 20);
        let comp = s.active_comp_id().ok_or("no comp")?;
        guard.render(&RenderJob { project: &s.project, comp, time: Tick::ZERO, opts: RenderOpts::default(), cache: &cache, times: None })
    }

    #[test]
    fn frames_fail_while_a_font_is_missing_and_render_once_it_is_not() {
        let log: LogFn = Arc::new(|_| {});
        let guard = wrap(Arc::new(Fine), log);
        let mut s = effectcraft_engine::Session::default();
        s.execute("comp.new", json!({"name": "C"})).ok();
        s.execute("layer.newText", json!({"text": "Hi", "font": "EffectCraft Test Font That Nobody Has"})).ok();
        let err = render(&guard, &s).err().unwrap_or_default();
        assert!(err.contains("EffectCraft Test Font That Nobody Has"), "{err}");
        // A project with fonts this machine has renders.
        let mut ok = effectcraft_engine::Session::default();
        ok.execute("comp.new", json!({"name": "C"})).ok();
        ok.execute("layer.newText", json!({"text": "Hi", "font": "Inter"})).ok();
        assert!(render(&guard, &ok).is_ok());
    }
}
