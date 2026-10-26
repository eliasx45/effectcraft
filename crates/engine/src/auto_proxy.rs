//! Automatic proxy suggestions (PROXY-AUTO1): footage too heavy to preview smoothly is offered a
//! proxy ("Smoke is 6K and costs ~46 ms/frame here. Make a 1080p proxy?"), shown where the
//! Timeline's Smart Freeze hint appears. A movie is suggested, once, when any of:
//!
//! - it is at least [`SIZE_FACTOR`]× the largest composition using it, in either dimension;
//! - the hardware decoder refuses it or cannot take a stream like it ([`crate::Importer::hardware_refuses`]:
//!   the 6K Level 6.0 H.264 "Smoke" clip decodes in software);
//! - its layer's measured time (the viewer's Render Time pane, [`crate::freeze_cost`]) stays above
//!   the frame budget.
//!
//! Yes (`proxy.autoCreate`) makes the proxy in the background: the largest using comp's size,
//! aspect kept, as H.264 (hardware-friendly) in `Proxies/` next to the project file; the
//! original is never touched. When it is done it becomes the item's proxy, marked
//! [`Proxy::auto`]. Automatic proxies are **preview-only**: final renders (Render Queue, exports,
//! Save Frame As) use the original under Proxy Use ▸ Current Settings, and only Use All Proxies
//! renders with them (`Renderer::proxy_for`). No (`proxy.dismissSuggestion`) never asks about
//! that item again; the answer is saved in the project.

use effectcraft_project::{AlphaMode, Footage, FootageKind, ItemId, ItemKind, LayerSource, Proxy};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, b_p, bad};
use crate::{EngineError, Result, Session, cmd, query};

/// Footage this many times the largest using comp (either dimension) is suggested.
pub const SIZE_FACTOR: f64 = 1.5;
/// Frames measured before a footage layer's time counts.
const MIN_SAMPLES: u32 = crate::freeze_cost::MIN_SAMPLES;
/// A proxy must have at most this share of the original's pixels to be worth making.
const MAX_AREA_SHARE: f64 = 0.75;
/// The background task's kind (Jobs panel, the Timeline hint's progress).
pub const TASK_KIND: &str = "autoProxy";

/// Why a footage item is suggested a proxy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Much larger than the largest comp using it.
    Size,
    /// The hardware decoder refuses it.
    Hardware,
    /// Its layer's measured time is above the frame budget.
    Slow,
}

impl Reason {
    fn id(self) -> &'static str {
        match self {
            Reason::Size => "size",
            Reason::Hardware => "hardware",
            Reason::Slow => "slow",
        }
    }
}

/// One footage item worth a proxy.
#[derive(Clone, Debug, PartialEq)]
pub struct ProxySuggestion {
    pub item: ItemId,
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// The proxy's size (the largest using comp's, aspect kept; even).
    pub proxy_width: u32,
    pub proxy_height: u32,
    /// The largest composition using it.
    pub comp: ItemId,
    pub reasons: Vec<Reason>,
    /// The slowest measured layer time of this footage (ms/frame) and its frame budget.
    pub ms_per_frame: Option<f64>,
    pub budget_ms: Option<f64>,
}

/// "6K" for 6144 wide, "4K" for 3840, "1080p" below 2.5K.
fn size_label(w: u32, h: u32) -> String {
    if w.max(h) >= 2500 { format!("{}K", (f64::from(w.max(h)) / 1000.0).round() as u32) } else { format!("{}p", w.min(h)) }
}

impl ProxySuggestion {
    /// "Smoke is 6K and costs ~46 ms/frame here. Make a 1080p proxy?"
    pub fn question(&self) -> String {
        let what = format!("{} is {}", self.name, size_label(self.width, self.height));
        let why = match self.ms_per_frame {
            Some(ms) => format!(" and costs ~{ms:.0} ms/frame here"),
            None if self.reasons.contains(&Reason::Hardware) => " and the hardware decoder can't take it".into(),
            None => String::new(),
        };
        format!("{what}{why}. Make a {} proxy?", size_label(self.proxy_width, self.proxy_height))
    }

    /// The label of the background task that makes it.
    pub fn task_label(&self) -> String {
        task_label(self.item, &self.name, self.proxy_width, self.proxy_height)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "item": self.item.0,
            "name": self.name,
            "width": self.width,
            "height": self.height,
            "proxyWidth": self.proxy_width,
            "proxyHeight": self.proxy_height,
            "comp": self.comp.0,
            "reasons": self.reasons.iter().map(|r| r.id()).collect::<Vec<_>>(),
            "msPerFrame": self.ms_per_frame,
            "frameBudgetMs": self.budget_ms,
            "question": self.question(),
        })
    }
}

fn task_label(item: ItemId, name: &str, w: u32, h: u32) -> String {
    format!("{} proxy of {name} (#{})", size_label(w, h), item.0)
}

/// The largest even size of `w`×`h`'s aspect that fits `max_w`×`max_h`, never larger than the
/// source (at least 16×16).
pub fn fit_size(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w == 0 || h == 0 || max_w == 0 || max_h == 0 {
        return (16, 16);
    }
    let s = (f64::from(max_w) / f64::from(w)).min(f64::from(max_h) / f64::from(h)).min(1.0);
    let even = |v: f64| ((v / 2.0).round() as u32 * 2).max(16);
    (even(f64::from(w) * s), even(f64::from(h) * s))
}

/// Footage the proxy builder takes: an online movie with pixels and without an alpha channel
/// (proxies are opaque H.264).
fn candidate(f: &Footage) -> bool {
    let codec = f.codec.to_ascii_lowercase();
    let nal = codec.is_empty() || ["h.264", "h264", "avc", "hevc", "h.265", "h265"].iter().any(|k| codec.contains(k));
    f.kind == FootageKind::Video && f.has_video && !f.missing && !f.path.is_empty() && f.width > 0 && f.height > 0 && (f.alpha == AlphaMode::Ignore || nal)
}

/// The proxy file for footage `f` at `w`×`h`: `Proxies/` next to the project file.
fn proxy_path(project_path: &str, f: &Footage, w: u32, h: u32) -> Option<std::path::PathBuf> {
    let dir = std::path::Path::new(project_path).parent()?.join("Proxies");
    let stem = std::path::Path::new(&f.path).file_stem()?.to_string_lossy().into_owned();
    Some(dir.join(format!("{stem}_{w}x{h}_proxy.mp4")))
}

impl Session {
    /// Footage items currently getting an automatic proxy.
    fn auto_proxy_building(&self, item: ItemId) -> bool {
        let tag = format!("(#{})", item.0);
        self.tasks.iter().any(|t| t.kind == TASK_KIND && t.label.ends_with(&tag))
    }

    /// Footage worth a proxy (see the module docs), most pressing first: footage of the active
    /// comp, then the slowest measured, then the largest. Items with a proxy, turned down
    /// ([`effectcraft_project::ProjectSettings::proxy_hint_dismissed`]) or being made are left out.
    pub fn proxy_suggestions(&self) -> Vec<ProxySuggestion> {
        // Largest comp (by area) using each footage item, and the slowest measured time.
        let mut largest: std::collections::BTreeMap<ItemId, (ItemId, u32, u32)> = Default::default();
        let mut layer_item: std::collections::BTreeMap<(ItemId, effectcraft_project::LayerId), ItemId> = Default::default();
        for (&cid, comp) in self.project.comps() {
            for l in &comp.layers {
                if let LayerSource::Footage { item } = l.source {
                    layer_item.insert((cid, l.id), item);
                    let e = largest.entry(item).or_insert((cid, comp.width, comp.height));
                    if u64::from(comp.width) * u64::from(comp.height) > u64::from(e.1) * u64::from(e.2) {
                        *e = (cid, comp.width, comp.height);
                    }
                }
            }
        }
        let mut slow: std::collections::BTreeMap<ItemId, (f64, f64)> = Default::default();
        for (comp, layer, ms, budget, samples) in self.freeze_runtime.cost.measured() {
            if samples < MIN_SAMPLES {
                continue;
            }
            if let Some(&item) = layer_item.get(&(comp, layer)) {
                let e = slow.entry(item).or_insert((ms, budget));
                if ms > e.0 {
                    *e = (ms, budget);
                }
            }
        }
        let active: std::collections::BTreeSet<ItemId> = self
            .active_comp_id()
            .and_then(|c| self.project.comp(c))
            .map(|c| c.layers.iter().filter_map(|l| if let LayerSource::Footage { item } = l.source { Some(item) } else { None }).collect())
            .unwrap_or_default();
        let dismissed = &self.project.settings.proxy_hint_dismissed;
        let mut out = Vec::new();
        for (&item, &(comp, cw, ch)) in &largest {
            let Some(it) = self.project.item(item) else {
                continue;
            };
            let ItemKind::Footage(f) = &it.kind else {
                continue;
            };
            if it.proxy.is_some() || dismissed.contains(&item) || !candidate(f) || self.auto_proxy_building(item) {
                continue;
            }
            let (pw, ph) = fit_size(f.width, f.height, cw, ch);
            if f64::from(pw) * f64::from(ph) > MAX_AREA_SHARE * f64::from(f.width) * f64::from(f.height) {
                continue;
            }
            let mut reasons = Vec::new();
            if f64::from(f.width) >= SIZE_FACTOR * f64::from(cw) || f64::from(f.height) >= SIZE_FACTOR * f64::from(ch) {
                reasons.push(Reason::Size);
            }
            if self.importer.as_ref().is_some_and(|i| i.hardware_refuses(f)) {
                reasons.push(Reason::Hardware);
            }
            let measured = slow.get(&item).copied();
            if measured.is_some_and(|(ms, budget)| ms > budget) {
                reasons.push(Reason::Slow);
            }
            if reasons.is_empty() {
                continue;
            }
            out.push(ProxySuggestion {
                item,
                name: it.name.clone(),
                width: f.width,
                height: f.height,
                proxy_width: pw,
                proxy_height: ph,
                comp,
                reasons,
                ms_per_frame: measured.map(|m| m.0),
                budget_ms: measured.map(|m| m.1),
            });
        }
        out.sort_by(|a, b| {
            active
                .contains(&b.item)
                .cmp(&active.contains(&a.item))
                .then(b.ms_per_frame.unwrap_or(0.0).total_cmp(&a.ms_per_frame.unwrap_or(0.0)))
                .then((u64::from(b.width) * u64::from(b.height)).cmp(&(u64::from(a.width) * u64::from(a.height))))
        });
        out
    }

    /// "No" to `item`'s proxy suggestion: never asked again (saved in the project; undoable).
    pub fn dismiss_proxy_suggestion(&mut self, item: ItemId) -> Result<()> {
        if self.project.item(item).is_none() {
            return Err(bad("proxy.dismissSuggestion", format!("no item {}", item.0)));
        }
        self.edit("Don't Suggest Proxy", None, |proj, _| {
            proj.settings.proxy_hint_dismissed.insert(item);
            Ok(())
        })
    }

    /// "Yes": make `item`'s automatic proxy in the background (`wait`: before returning) and set
    /// it as the item's proxy when done.
    pub fn create_auto_proxy(&mut self, item: ItemId, wait: bool) -> Result<Value> {
        const C: &str = "proxy.autoCreate";
        let it = self.project.item(item).ok_or_else(|| bad(C, format!("no item {}", item.0)))?;
        let ItemKind::Footage(f) = &it.kind else {
            return Err(bad(C, format!("{} is not footage", it.name)));
        };
        if !candidate(f) {
            return Err(bad(C, format!("{} is not a movie a proxy can be made of", it.name)));
        }
        if self.auto_proxy_building(item) {
            return Err(bad(C, format!("a proxy of {} is already being made", it.name)));
        }
        let Some(project_path) = self.path.clone().filter(|p| !p.is_empty()) else {
            return Err(bad(C, "save the project first: proxies go in a Proxies folder next to it"));
        };
        let importer = self.importer.clone().ok_or_else(|| EngineError::Other("media import is not available in this build".into()))?;
        // The largest comp using it (else the footage's own size halved).
        let (cw, ch) = self
            .project
            .comps()
            .filter(|(_, c)| c.layers.iter().any(|l| matches!(l.source, LayerSource::Footage { item: i } if i == item)))
            .map(|(_, c)| (c.width, c.height))
            .max_by_key(|(w, h)| u64::from(*w) * u64::from(*h))
            .unwrap_or((f.width / 2, f.height / 2));
        let (w, h) = fit_size(f.width, f.height, cw, ch);
        let out = proxy_path(&project_path, f, w, h).ok_or_else(|| bad(C, "no folder for the proxy"))?;
        let out_str = out.to_string_lossy().into_owned();
        if out_str == f.path {
            return Err(bad(C, "the proxy would overwrite the original"));
        }
        let (src, name) = (f.path.clone(), it.name.clone());
        let label = task_label(item, &name, w, h);
        let proxy_label = size_label(w, h);
        self.spawn_task(TASK_KIND, label, wait, move |ctl| {
            // A finished proxy from before is used as it is.
            if !out.exists() {
                ctl.message(format!("Making a {proxy_label} proxy of {name}…"));
                importer.build_proxy(&src, &out_str, w, h, &mut |d, t| ctl.progress(d, t))?;
            }
            let apply: crate::jobs::Apply = Box::new(move |s: &mut Session| {
                let mut pf = s.probe_footage(&out_str).map_err(|e| EngineError::Other(format!("{out_str}: {e}")))?;
                if !pf.has_video {
                    return Err(EngineError::Other(format!("{out_str} has no video")));
                }
                let Some(it) = s.project.item(item) else {
                    return Err(EngineError::Other(format!("{name} was removed")));
                };
                if it.proxy.as_deref().is_some_and(|p| !p.auto) {
                    return Ok(json!({"item": item.0, "path": out_str, "toast": format!("{name} has a proxy already; {out_str} was left unused")}));
                }
                // The proxy plays like the original (conformed rate, loops, colour).
                if let ItemKind::Footage(f) = &it.kind {
                    pf.frame_rate = f.frame_rate;
                    pf.pixel_aspect = f.pixel_aspect;
                    pf.loop_count = f.loop_count;
                    pf.color_profile = f.color_profile;
                    pf.linear_light = f.linear_light;
                    pf.fields = f.fields;
                }
                let (pw, ph) = (pf.width, pf.height);
                s.edit("Create Proxy", None, |proj, _| {
                    if let Some(it) = proj.item_mut(item) {
                        it.proxy = Some(Box::new(Proxy { footage: pf, enabled: true, auto: true }));
                    }
                    Ok(())
                })?;
                Ok(json!({
                    "item": item.0,
                    "path": out_str,
                    "width": pw,
                    "height": ph,
                    "toast": format!("{name} now previews from a {} proxy (final renders use the original)", size_label(pw, ph)),
                }))
            });
            Ok(apply)
        })
    }
}

fn item_p(p: &Value, cmd: &str) -> Result<ItemId> {
    p.get("item").and_then(Value::as_u64).map(ItemId).ok_or_else(|| bad(cmd, "`item` must be a footage item id"))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        query!(
            "proxy.suggest",
            "Suggest Proxies",
            "{} -> {candidates: [{item, name, width, height, proxyWidth, proxyHeight, comp, reasons: [size|hardware|slow], msPerFrame?, frameBudgetMs?, question}]}",
            |s, _| Ok(json!({"candidates": s.proxy_suggestions().iter().map(ProxySuggestion::to_json).collect::<Vec<_>>()}))
        ),
        cmd!("proxy.dismissSuggestion", "Don't Suggest Proxy", [], None, "{item}", crate::commands::always, |s, p| {
            let item = item_p(p, "proxy.dismissSuggestion")?;
            s.dismiss_proxy_suggestion(item)?;
            Ok(json!({"item": item.0, "dismissed": true}))
        }),
        cmd!(
            "proxy.autoCreate",
            "Make Proxy",
            [],
            None,
            "{item, wait?} — a preview-only H.264 proxy at the largest using comp's size in Proxies/ next to the project; final renders keep the original unless Proxy Use is Use All Proxies",
            crate::commands::always,
            |s, p| {
                let item = item_p(p, "proxy.autoCreate")?;
                s.create_auto_proxy(item, b_p(p, "wait").unwrap_or(false))
            }
        ),
    ]
}
