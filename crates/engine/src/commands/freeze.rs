//! Native Smart Freeze commands. Rendering happens on a snapshot in a background task.

use effectcraft_project::LayerSource;
use std::sync::Arc;

use effectcraft_project::freeze::{MAX_FRAMES, MAX_SECTIONS, Section, Target, Version};
use effectcraft_render::{RenderOpts, Renderer};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::{CommandSpec, b_p, bad, comp_id, has_comp, str_p};
use crate::{EngineError, Result, Session, cmd, query};

fn section_id(p: &Value) -> Result<u64> {
    p.get("section").and_then(Value::as_u64).ok_or_else(|| bad("freeze", "section must be an existing section ID"))
}

fn selected_target(s: &Session, p: &Value) -> Result<Target> {
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let mut selected = if p.get("layers").is_some() {
        super::layout_cmds::named_layers(comp, p, "freeze.freeze")?
    } else if s.active_comp_id() == Some(cid) {
        s.state.selected_layers.clone()
    } else {
        vec![]
    };
    if selected.iter().any(|id| comp.layer(*id).is_none()) {
        return Err(bad("freeze.freeze", "selected layer is not in the requested composition"));
    }
    selected.sort_by_key(|id| comp.layers.iter().position(|l| l.id == *id));
    selected.dedup();
    match str_p(p, "mode").unwrap_or("auto") {
        "comp" => Ok(Target::Comp { comp: cid }),
        "precomp" => {
            if selected.len() == 1
                && let Some(layer) = selected.first().and_then(|id| comp.layer(*id))
                && let LayerSource::Comp { item } = layer.source
            {
                return Ok(Target::Precomp { comp: item });
            }
            Err(bad("freeze.freeze", "select one precomposition"))
        }
        "selection" => Ok(Target::Selection { comp: cid, layers: selected }),
        "auto" => {
            if selected.is_empty() || selected.len() == comp.layers.len() {
                Ok(Target::Comp { comp: cid })
            } else if selected.len() == 1
                && let Some(layer) = selected.first().and_then(|id| comp.layer(*id))
                && let LayerSource::Comp { item } = layer.source
            {
                Ok(Target::Precomp { comp: item })
            } else {
                Ok(Target::Selection { comp: cid, layers: selected })
            }
        }
        _ => Err(bad("freeze.freeze", "mode: auto|comp|precomp|selection")),
    }
}

fn create(s: &mut Session, p: &Value) -> Result<Value> {
    if cfg!(target_arch = "wasm32") {
        return Err(bad("freeze.freeze", "Smart Freeze rendering requires the native desktop host; originals remain editable"));
    }
    if s.project.freeze.sections.len() >= MAX_SECTIONS {
        return Err(bad("freeze.freeze", "too many frozen sections"));
    }
    let target = selected_target(s, p)?;
    if let Some(id) = s
        .project
        .freeze
        .sections
        .iter()
        .find(|section| {
            section.target == target
                || section.target.comp() == target.comp() && !matches!(section.target, Target::Selection { .. }) && !matches!(target, Target::Selection { .. })
        })
        .map(|s| s.id)
    {
        let mut config = json!({"section":id});
        for key in ["quickDraft", "name", "start", "end", "followCompositionRange"] {
            if let Some(value) = p.get(key) {
                config[key] = value.clone();
            }
        }
        if config.as_object().is_some_and(|m| m.len() > 1) {
            s.execute("freeze.configure", config)?;
        }
        if b_p(p, "auto").unwrap_or(false) {
            s.execute("freeze.auto", json!({"section": id, "enabled": true}))?;
        }
        return refresh(s, &json!({"section": id, "wait": b_p(p, "wait").unwrap_or(false)}));
    }
    if let Target::Selection { comp, layers } = &target
        && s.project
            .freeze
            .sections
            .iter()
            .any(|s| matches!(&s.target,Target::Selection {comp:other,layers:used} if other==comp && layers.iter().any(|id|used.contains(id))))
    {
        return Err(bad("freeze.freeze", "these layers already belong to a frozen section; refresh or remove that section first"));
    }
    crate::freeze::validate_target(&s.project, &target).map_err(EngineError::Other)?;
    let comp = s.project.comp(target.comp()).ok_or(EngineError::NoComp)?;
    let start = crate::render_view::seconds_p(p, "start", "freeze.freeze")?.map(Tick::from_seconds_f64).unwrap_or(Tick::ZERO);
    let end = crate::render_view::seconds_p(p, "end", "freeze.freeze")?.map(Tick::from_seconds_f64).unwrap_or(comp.duration);
    if start < Tick::ZERO || end > comp.duration || end <= start {
        return Err(bad("freeze.freeze", "range must lie inside the composition"));
    }
    let name = str_p(p, "name").map(str::to_string).unwrap_or_else(|| {
        let comp_name = s.project.item(target.comp()).map(|i| i.name.as_str()).unwrap_or("Composition");
        if let Target::Selection { layers, .. } = &target {
            let layer = layers.first().and_then(|id| comp.layer(*id)).map(|l| l.name.as_str()).unwrap_or("Selection");
            let suffix = if layers.len() > 1 { format!(" + {}", layers.len() - 1) } else { String::new() };
            format!("{comp_name} · {layer}{suffix}")
        } else {
            comp_name.to_owned()
        }
    });
    let draft = b_p(p, "quickDraft").unwrap_or(true);
    let auto = b_p(p, "auto").unwrap_or(false);
    let explicit_range = p.get("start").is_some_and(|v| !v.is_null()) || p.get("end").is_some_and(|v| !v.is_null());
    let follow_comp_range = match p.get("followCompositionRange") {
        Some(v) => v.as_bool().ok_or_else(|| bad("freeze.freeze", "followCompositionRange must be a boolean"))?,
        None => !explicit_range,
    };
    if follow_comp_range && explicit_range {
        return Err(bad("freeze.freeze", "choose composition duration or an explicit range, not both"));
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| bad("freeze.freeze", e.to_string()))?.as_nanos();
    let id = s.edit("Smart Freeze Section", None, |project, _| {
        if project.freeze.namespace.is_empty() {
            project.freeze.namespace = format!("project-{}-{stamp}", std::process::id());
        }
        let id = project.next_id;
        project.next_id = id.checked_add(1).ok_or_else(|| bad("freeze.freeze", "project IDs exhausted"))?;
        let section = Section { id, name, target, start, end, follow_comp_range, enabled: true, auto, quick_draft: draft, cache: None, previous: vec![] };
        section.validate().map_err(EngineError::Other)?;
        project.freeze.sections.push(section);
        Ok(id)
    })?;
    refresh(s, &json!({"section": id, "wait": b_p(p, "wait").unwrap_or(false)}))
}

fn refresh(s: &mut Session, p: &Value) -> Result<Value> {
    if cfg!(target_arch = "wasm32") {
        return Err(bad("freeze.refresh", "Smart Freeze rendering requires the native desktop host"));
    }
    if s.tasks.iter().any(|t| matches!(t.kind, "smartFreeze" | "freezeStorage")) {
        return Err(bad("freeze.refresh", "a Smart Freeze job is already running; cancel it first"));
    }
    let id = section_id(p)?;
    let section = s.project.freeze.sections.iter().find(|x| x.id == id).cloned().ok_or_else(|| bad("freeze.refresh", "section not found"))?;
    let automatic = b_p(p, "automatic").unwrap_or(false);
    if automatic && s.claims.get(section.target.comp()).is_some() {
        return Err(bad("freeze.refresh", "Auto waits for the composition claim to be released"));
    }
    let original = s.project.clone();
    *s.freeze_runtime.latest_source.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = (s.epoch, original.clone());
    let epoch = s.epoch;
    let client = s.client.clone();
    let claim_at_start = s.claims.get(section.target.comp()).map(|c| c.owner.clone());
    let (footage, expr) = (s.footage.clone(), s.expr.clone());
    let opts = RenderOpts {
        backend: if s.accel.is_some() { effectcraft_render::Backend::Auto } else { effectcraft_render::Backend::Cpu },
        guides: true,
        nested_switches: s.prefs.general.switches_affect_nested_comps,
        draft_shadows: s.prefs.three_d.realtime_shadows,
        ..Default::default()
    };
    let opts = if section.quick_draft { RenderOpts { scale: 0.5, draft: true, ..opts } } else { opts };
    let prior = section
        .cache
        .as_ref()
        .zip(s.freeze_runtime.proofs.get(&id))
        .filter(|(version, proof)| !version.draft && version.identity == proof.capture.identity)
        .map(|(version, proof)| (version.clone(), proof.capture.clone()));
    let job = FreezeJob {
        original,
        capture: None,
        section,
        epoch,
        client,
        footage,
        expr,
        opts,
        pause: s.freeze_runtime.pause_until.clone(),
        prior,
        slot: s.freeze_runtime.render_slot.clone(),
        accel: s.accel.clone(),
        latest: s.freeze_runtime.latest_source.clone(),
        claim_at_start,
        automatic,
    };
    start_stage(s, job, b_p(p, "wait").unwrap_or(false))
}

#[derive(Clone)]
struct FreezeJob {
    automatic: bool,
    claim_at_start: Option<String>,
    latest: Arc<std::sync::Mutex<(u64, Arc<effectcraft_project::Project>)>>,
    accel: Option<Arc<dyn effectcraft_render::Accelerator>>,
    original: Arc<effectcraft_project::Project>,
    capture: Option<Arc<crate::freeze::Capture>>,
    section: Section,
    epoch: u64,
    client: Option<String>,
    footage: Arc<dyn effectcraft_render::FootageSource>,
    expr: Option<Arc<dyn effectcraft_render::ExprHost>>,
    opts: RenderOpts,
    pause: Arc<std::sync::atomic::AtomicU64>,
    prior: Option<(Version, Arc<crate::freeze::Capture>)>,
    slot: Arc<std::sync::Mutex<()>>,
}

fn start_stage(s: &mut Session, job: FreezeJob, wait: bool) -> Result<Value> {
    let root = crate::freeze_storage::root(&job.original.freeze.namespace).map_err(EngineError::Other)?;
    let label = format!("{} {}", if job.opts.draft { "Draft" } else { "Freeze" }, job.section.name);
    s.spawn_task("smartFreeze", label, wait, move |ctl| {
        let FreezeJob { original, capture, section, epoch, client, footage, expr, opts, pause, prior, slot, accel, latest, claim_at_start, automatic } = job;
        let slot_for_next = slot.clone();
        let id = section.id;
        let fresh_capture = capture.is_none();
        let capture = match capture {
            Some(c) => c,
            None => Arc::new(crate::freeze::Capture::new(original.clone(), &section)?),
        };
        capture.verify_media()?;
        if fresh_capture {
            // A verified file may differ from a decoder/frame already kept by the editor.
            // Refresh must render the current bytes, not rebake the old media cache.
            for stamp in &capture.media {
                footage.forget(&stamp.path);
            }
        }
        if ctl.cancelled() {
            return Err("Smart Freeze cancelled".into());
        }
        let _permit = loop {
            match slot.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(e)) => break e.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if ctl.cancelled() {
                        return Err("Smart Freeze cancelled".into());
                    }
                    ctl.message("Waiting for the cancelled freeze worker to finish");
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        };

        let comp = capture.render.comp(section.target.comp()).ok_or("Smart Freeze composition disappeared")?;
        let rate = comp.frame_rate;
        let (range_start, range_end) = section.range(comp.duration)?;
        let first = rate.frame_at(range_start);
        let end = rate.frame_at(range_end - Tick(1)).checked_add(1).ok_or("Smart Freeze frame range overflow")?;
        let count = end
            .checked_sub(first)
            .and_then(|n| u64::try_from(n).ok())
            .filter(|n| *n > 0 && *n <= MAX_FRAMES)
            .ok_or("Smart Freeze frame range exceeds its budget")?;
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let created = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?;
        let directory = root.join(format!("{}-{}-{}", std::process::id(), section.id, created.as_nanos()));
        std::fs::create_dir(&directory).map_err(|e| e.to_string())?;
        std::fs::write(directory.join("owner.json"), b"{\"owner\":\"effectcraft-smart-freeze\",\"version\":1}").map_err(|e| e.to_string())?;
        let _job_folder = crate::freeze_storage::JobFolder(directory.clone());
        let mut renderer = Renderer::new(&capture.render, footage.as_ref(), opts);
        renderer.expr = expr.as_deref();
        let mut bytes = 0u64;
        let began = web_time::Instant::now();
        let mut reused_frames = 0u64;
        let changed_spans = prior.as_ref().and_then(|(_, old)| capture.changed_spans(old));
        let pool = rayon::ThreadPoolBuilder::new().num_threads(2).thread_name(|i| format!("smart-freeze-{i}")).build().map_err(|e| e.to_string())?;
        let mut checked = original.clone();
        for frame in first..end {
            let done = u64::try_from(frame.saturating_sub(first)).map_err(|e| e.to_string())?;
            if !ctl.progress(done, count) {
                return Err("Smart Freeze cancelled".into());
            }
            while !wait && crate::freeze_runtime::now_millis() < pause.load(std::sync::atomic::Ordering::Relaxed) {
                if ctl.cancelled() {
                    return Err("Smart Freeze cancelled".into());
                }
                ctl.message("Yielding while editing or playing");
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            ctl.message(if opts.draft { "Rendering draft" } else { "Rendering full quality" });
            let (current_epoch, current) = latest.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
            if current_epoch != epoch || !capture.fonts_current() || (!Arc::ptr_eq(&current, &checked) && !capture.still_matches(&current)) {
                return Err("Smart Freeze source changed while rendering; originals remain live".into());
            }
            checked = current;
            if !opts.draft
                && let Some((old, _)) = &prior
                && old.frame_rate == rate
                && old.render_options == serde_json::to_string(&opts).map_err(|e| e.to_string())?
                && changed_spans.as_ref().is_some_and(|spans| !spans.iter().any(|(start, end)| rate.tick_of(frame) >= *start && rate.tick_of(frame) < *end))
            {
                let previous_frame = std::path::Path::new(&old.directory).join(format!("{frame}.ecfreeze"));
                if let Ok(image) = effectcraft_render::freeze::read_frame(&previous_frame) {
                    let copied = effectcraft_render::freeze::write_frame(&directory.join(format!("{frame}.ecfreeze")), &image).map_err(|e| e.to_string())?;
                    bytes = bytes.saturating_add(copied);
                    reused_frames = reused_frames.saturating_add(1);
                    continue;
                }
            }
            let selected = match &section.target {
                Target::Selection { layers, .. } => Some(layers.as_slice()),
                _ => None,
            };
            let image = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pool.install(|| {
                    effectcraft_render::passes::block_on(crate::offload::in_passes(accel.as_deref(), |a| {
                        renderer.with_accel(a).freeze_selected_frame(section.target.comp(), rate.tick_of(frame), selected)
                    }))
                })
            }))
            .map_err(|_| "Smart Freeze render failed; originals are intact")??;
            bytes =
                bytes.saturating_add(effectcraft_render::freeze::write_frame(&directory.join(format!("{frame}.ecfreeze")), &image).map_err(|e| e.to_string())?);
        }
        capture.verify_media()?;
        if !ctl.progress(count, count) {
            return Err("Smart Freeze cancelled".into());
        }
        let mut version = Version {
            directory: directory.to_string_lossy().into_owned(),
            identity: capture.identity.clone(),
            draft: opts.draft,
            width: (f64::from(comp.width) * opts.scale).round().max(1.0) as u32,
            height: (f64::from(comp.height) * opts.scale).round().max(1.0) as u32,
            frames: count,
            bytes,
            created_unix: created.as_secs(),
            render_options: serde_json::to_string(&opts).map_err(|e| e.to_string())?,
            frame_rate: rate,
            reused_frames,
            sample: None,
        };
        if !opts.draft {
            ctl.message("Measuring live and cached preview");
            version.sample = pool.install(|| {
                crate::freeze_measure::measure(
                    &capture.original,
                    &section,
                    &version,
                    footage.as_ref(),
                    expr.as_deref(),
                    accel.as_deref(),
                    opts,
                    began.elapsed().as_secs_f64(),
                    ctl,
                )
            });
        }
        capture.verify_media()?;
        if ctl.cancelled() {
            return Err("Smart Freeze cancelled".into());
        }
        std::fs::write(directory.join("version.json"), serde_json::to_vec(&version).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        Ok(Box::new(move |session| {
            if session
                .claims
                .get(section.target.comp())
                .is_some_and(|claim| client.as_deref().map_or_else(|| claim_at_start.as_deref() != Some(claim.owner.as_str()), |owner| owner != claim.owner))
            {
                return Err(EngineError::Other("Smart Freeze composition was claimed by another client while rendering".into()));
            }
            if session.epoch != epoch || !capture.still_matches(&session.project) || !session.project.freeze.sections.iter().any(|s| s == &section) {
                return Err(EngineError::Other("Smart Freeze changed while rendering; originals remain available; refresh again".into()));
            }
            if !capture.media.is_empty() {
                session.layer_cache.clear();
                session.events.push(crate::Event::PurgeCaches);
            }
            if automatic {
                session.attach_automatic_freeze(id, version.clone())?;
            } else {
                let previous_client = std::mem::replace(&mut session.client, client.clone());
                let attached = session.edit("Attach Smart Freeze", None, |project, _| {
                    let current = project.freeze.sections.iter_mut().find(|s| s.id == id).ok_or_else(|| bad("freeze.refresh", "section removed"))?;
                    current.enabled = true;
                    current.start = range_start;
                    current.end = range_end;
                    if let Some(old) = current.cache.take() {
                        current.previous.push(old);
                    }
                    if current.previous.len() > 32 {
                        current.previous.remove(0);
                    }
                    current.cache = Some(version.clone());
                    Ok(())
                });
                session.client = previous_client;
                attached?;
            }
            session.freeze_runtime.accept(id, session.revision, capture.clone());
            if opts.draft {
                let section = session.project.freeze.sections.iter().find(|s| s.id == id).cloned().ok_or_else(|| bad("freeze.refresh", "section removed"))?;
                let next = FreezeJob {
                    original,
                    capture: Some(capture),
                    section,
                    epoch,
                    client,
                    footage,
                    expr,
                    pause,
                    prior,
                    accel,
                    latest,
                    claim_at_start,
                    automatic,
                    slot: slot_for_next,
                    opts: RenderOpts { scale: 1.0, draft: false, ..opts },
                };
                let full = start_stage(session, next, wait)?;
                if wait {
                    return Ok(full);
                }
                return Ok(json!({"section":id, "state":"draft", "version":version, "nextJob":full["job"], "full":full}));
            }
            let project = session.project.clone();
            let saved_version = version.clone();
            if let Err(e) = session.spawn_task("freezeStorage", "Maintain Smart Freeze storage", wait, move |ctl| {
                let root = crate::freeze_storage::root(&project.freeze.namespace)?;
                let now = crate::freeze_runtime::now_millis() / 1000;
                if let Some(section) = project.freeze.sections.iter().find(|section| section.id == id) {
                    crate::freeze_storage::adopt_previous(&root, id, &section.previous, now)?;
                }
                crate::freeze_storage::record(&root, id, &saved_version, now)?;
                let protected = crate::freeze_storage::protected(&project);
                let report = crate::freeze_storage::maintain(&root, &protected, now, false, project.freeze.budget_bytes, project.freeze.retire_old, |path| {
                    if ctl.cancelled() {
                        return Err("Smart Freeze cancelled".into());
                    }
                    crate::freeze_storage::move_to_trash(path)
                })?;
                Ok(Box::new(move |_| Ok(json!({"namespace":project.freeze.namespace, "storage":report}))))
            }) {
                session.toast(format!("Freeze attached; storage maintenance needs attention: {e}"));
            }
            Ok(json!({"section": id, "state": "frozenManual", "version": version}))
        }))
    })
}

fn cancel_freeze(s: &mut Session) {
    let ids: Vec<_> = s.tasks.iter().filter(|t| matches!(t.kind, "smartFreeze" | "freezeCheck")).map(|t| format!("task:{}", t.id)).collect();
    for id in ids {
        s.cancel_job(&id);
    }
}

fn edit(s: &mut Session, p: &Value) -> Result<Value> {
    let id = section_id(p)?;
    cancel_freeze(s);
    s.edit("Edit Frozen Originals", None, |project, _| {
        let section = project.freeze.sections.iter_mut().find(|x| x.id == id).ok_or_else(|| bad("freeze.edit", "section not found"))?;
        section.enabled = false;
        Ok(())
    })?;
    Ok(json!({"section": id, "state": "live"}))
}

fn originals(s: &mut Session, _: &Value) -> Result<Value> {
    cancel_freeze(s);
    s.edit("Smart Freeze: Use Originals", None, |project, _| {
        for section in &mut project.freeze.sections {
            section.enabled = false;
            section.auto = false;
        }
        Ok(())
    })?;
    Ok(json!({"state": "live", "auto": false}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "freeze.freeze",
            "Freeze",
            ["Composition", "Smart Freeze"],
            None,
            "{comp?, layers?, mode?: auto|comp|precomp|selection, name?, start?, end?, followCompositionRange?: bool, quickDraft?, auto?: bool (keep it fresh, as freeze.auto), wait?}",
            has_comp,
            create
        ),
        cmd!("freeze.refresh", "Refresh", [], None, "{section, wait?, automatic?: bool (scheduler; no authoring undo step)}", super::always, refresh),
        cmd!("freeze.edit", "Edit Originals", [], None, "{section}", super::always, edit),
        cmd!(
            "freeze.configure",
            "Configure Freeze Section",
            [],
            None,
            "{section, quickDraft?: bool, name?: string, start?, end?, followCompositionRange?: bool}",
            super::always,
            |s, p| {
                let id = section_id(p)?;
                let start = crate::render_view::seconds_p(p, "start", "freeze.configure")?.map(Tick::from_seconds_f64);
                let end = crate::render_view::seconds_p(p, "end", "freeze.configure")?.map(Tick::from_seconds_f64);
                let follow = p
                    .get("followCompositionRange")
                    .map(|v| v.as_bool().ok_or_else(|| bad("freeze.configure", "followCompositionRange must be a boolean")))
                    .transpose()?;
                if follow == Some(true) && (start.is_some() || end.is_some()) {
                    return Err(bad("freeze.configure", "choose composition duration or an explicit range, not both"));
                }
                let range = if start.is_some() || end.is_some() || follow.is_some() {
                    let section = s.project.freeze.sections.iter().find(|s| s.id == id).ok_or_else(|| bad("freeze.configure", "section not found"))?;
                    let duration = s.project.comp(section.target.comp()).ok_or(EngineError::NoComp)?.duration;
                    let (old_start, old_end) = if section.follow_comp_range { (Tick::ZERO, duration) } else { (section.start, section.end) };
                    let follow = follow.unwrap_or(false);
                    let (start, end) = if follow { (Tick::ZERO, duration) } else { (start.unwrap_or(old_start), end.unwrap_or(old_end)) };
                    if start < Tick::ZERO || end <= start || end > duration {
                        return Err(bad("freeze.configure", "range must lie inside the composition"));
                    }
                    Some((start, end, follow))
                } else {
                    None
                };
                let draft = match p.get("quickDraft") {
                    Some(v) => Some(v.as_bool().ok_or_else(|| bad("freeze.configure", "quickDraft must be a boolean"))?),
                    None => None,
                };
                let name = match p.get("name") {
                    Some(v) => Some(
                        v.as_str()
                            .filter(|v| !v.trim().is_empty() && v.len() <= 512)
                            .ok_or_else(|| bad("freeze.configure", "name must be 1–512 bytes"))?
                            .to_owned(),
                    ),
                    None => None,
                };
                s.edit("Configure Smart Freeze", None, |project, _| {
                    let section = project.freeze.sections.iter_mut().find(|s| s.id == id).ok_or_else(|| bad("freeze.configure", "section not found"))?;
                    if let Some(draft) = draft {
                        section.quick_draft = draft;
                    }
                    if let Some(name) = name {
                        section.name = name;
                    }
                    if let Some((start, end, follow)) = range {
                        if let Some(old) = section.cache.take() {
                            section.previous.push(old);
                            if section.previous.len() > 32 {
                                section.previous.remove(0);
                            }
                        }
                        section.start = start;
                        section.end = end;
                        section.follow_comp_range = follow;
                        section.enabled = false;
                    }
                    Ok(())
                })?;
                Ok(json!({"section":id}))
            }
        ),
        cmd!("freeze.remove", "Remove Freeze Section", [], None, "{section}", super::always, |s, p| {
            let id = section_id(p)?;
            cancel_freeze(s);
            s.edit("Remove Smart Freeze Section", None, |project, _| {
                if !project.freeze.sections.iter().any(|v| v.id == id) {
                    return Err(bad("freeze.remove", "section not found"));
                }
                project.freeze.sections.retain(|v| v.id != id);
                Ok(())
            })?;
            s.freeze_runtime.proofs.remove(&id);
            s.freeze_runtime.pixel_epochs.remove(&id);
            s.freeze_runtime.reasons.remove(&id);
            Ok(json!({"removed":id,"cacheRetained":true}))
        }),
        cmd!("freeze.auto", "Auto-freeze Section", [], None, "{section, enabled: bool}", super::always, |s, p| {
            let id = section_id(p)?;
            let enabled = b_p(p, "enabled").ok_or_else(|| bad("freeze.auto", "enabled must be true or false"))?;
            let verified = s.freeze_runtime.proofs.get(&id).is_some_and(|proof| proof.revision == s.revision && proof.capture.fonts_current())
                && s.project
                    .freeze
                    .sections
                    .iter()
                    .find(|v| v.id == id)
                    .and_then(|v| v.cache.as_ref())
                    .is_some_and(|v| !s.freeze_runtime.bad_versions.contains(&v.directory));
            s.edit("Smart Freeze Auto", None, |project, _| {
                let section = project.freeze.sections.iter_mut().find(|s| s.id == id).ok_or_else(|| bad("freeze.auto", "section not found"))?;
                if enabled {
                    section.enabled = true;
                } else if section.auto && !verified {
                    section.enabled = false;
                }
                section.auto = enabled;
                Ok(())
            })?;
            Ok(json!({"section":id, "auto":enabled}))
        }),
        cmd!("freeze.originals", "Use Originals / Stop Auto", ["Composition", "Smart Freeze"], None, "{}", super::always, originals),
        cmd!("freeze.cancel", "Cancel Freeze", [], None, "{}", super::always, |s, _| {
            cancel_freeze(s);
            Ok(json!({"cancelled": true}))
        }),
        query!(
            "freeze.suggest",
            "Suggest Layers to Freeze",
            "{comp?: id (default: the active comp), all?: bool (every comp), dismiss?: {comp, layer}} -> {candidates: [{comp, layer, name, msPerFrame, frameBudgetMs, ratio, samples, target: precomp|selection|comp, question, freeze: freeze.freeze params}], autoHeavy}",
            |s, p| {
                if let Some(d) = p.get("dismiss").filter(|d| !d.is_null()) {
                    let id = |key: &str| d.get(key).and_then(Value::as_u64).ok_or_else(|| bad("freeze.suggest", format!("dismiss.{key} must be an id")));
                    s.dismiss_freeze_suggestion(effectcraft_project::ItemId(id("comp")?), effectcraft_project::LayerId(id("layer")?));
                }
                let scope = match p.get("comp") {
                    _ if b_p(p, "all").unwrap_or(false) => None,
                    Some(v) if !v.is_null() => Some(comp_id(s, p).map_err(|_| bad("freeze.suggest", format!("no composition {v}")))?),
                    _ => s.active_comp_id(),
                };
                let candidates: Vec<_> = s.freeze_suggestions_in(scope).iter().map(|c| c.to_json()).collect();
                Ok(json!({"candidates": candidates, "autoHeavy": s.prefs.previews.freeze_auto_heavy}))
            }
        ),
        query!("freeze.list", "Frozen Sections", "{}", |s, _| Ok(json!({"sections": s.project.freeze.sections}))),
    ]
}
