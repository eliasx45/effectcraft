//! Instance-aware projections of real nested layers; no document copies or render changes.
use super::*;
use effectcraft_engine::commands::inline_comp::{self, Context};

const MAX_ROWS: usize = 20_000;

#[derive(Clone, Debug)]
pub(super) struct InlineRow {
    root: ItemId,
    path: Vec<u64>,
    context: Context,
    row: Row,
}

fn instance_key(root: ItemId, path: &[u64]) -> Vec<u64> {
    std::iter::once(root.0).chain(path.iter().copied()).collect()
}

pub(super) fn expander(app: &mut dyn TimelineHost, ui: &mut egui::Ui, root: ItemId, path: &[u64], layer: &Layer, rect: Rect) {
    if !matches!(layer.source, LayerSource::Comp { .. }) {
        return;
    }
    let mut child_path = path.to_vec();
    child_path.push(layer.id.0);
    let key = instance_key(root, &child_path);
    let open = app.ui().timeline.inline_precomps.contains(&key);
    let resp = widgets::twirl(ui, rect, open, tl_id(("inline-precomp", &key)), &app.tokens())
        .on_hover_text(tr("Expand this composition's layers inline (edits affect every instance)"));
    app.auto_mut().add(&format!("timeline.inline.{}.expand", key.iter().map(u64::to_string).collect::<Vec<_>>().join(".")), rect, "Expand nested composition");
    if resp.clicked() {
        if open {
            app.ui_mut().timeline.inline_precomps.remove(&key);
        } else if let Err(e) = inline_comp::context(app.session(), root, &child_path, app.session().time_of(root)) {
            app.ui_mut().status = e.to_string();
        } else {
            app.ui_mut().timeline.inline_precomps.insert(key);
        }
    }
}

pub(super) fn expand(app: &dyn TimelineHost, root: ItemId, rows: Vec<Row>) -> Vec<Row> {
    let mut out = Vec::with_capacity(rows.len());
    // Repeated instances share row structure within this pass, but keep their own time/path.
    let mut sources = std::collections::BTreeMap::new();
    let mut i = 0;
    while let Some(row) = rows.get(i) {
        let layer = row.layer;
        while let Some(row) = rows.get(i).filter(|r| r.layer == layer) {
            out.push(row.clone());
            i += 1;
        }
        append(app, root, &[layer.0], &mut out, &mut sources);
    }
    out
}

fn append(app: &dyn TimelineHost, root: ItemId, path: &[u64], out: &mut Vec<Row>, sources: &mut std::collections::BTreeMap<ItemId, std::sync::Arc<Vec<Row>>>) {
    if out.len() >= MAX_ROWS || !app.ui().timeline.inline_precomps.contains(&instance_key(root, path)) {
        return;
    }
    let Ok(context) = inline_comp::context(app.session(), root, path, app.session().time_of(root)) else { return };
    let Some(comp) = app.session().project.comp(context.comp) else { return };
    let rows = sources.entry(context.comp).or_insert_with(|| std::sync::Arc::new(build_rows(app, comp))).clone();
    let mut i = 0;
    while let Some(row) = rows.get(i) {
        if out.len() >= MAX_ROWS {
            return;
        }
        let layer = row.layer;
        while let Some(row) = rows.get(i).filter(|r| r.layer == layer) {
            if out.len() >= MAX_ROWS {
                return;
            }
            // Waveforms use a separate source cache; keep them in the source comp timeline.
            if matches!(row.kind, RowKind::Layer | RowKind::Group { .. } | RowKind::Prop { .. }) {
                out.push(Row {
                    layer,
                    depth: path.len(),
                    kind: RowKind::Inline(Box::new(InlineRow { root, path: path.to_vec(), context: context.clone(), row: row.clone() })),
                });
            }
            i += 1;
        }
        if path.len() < inline_comp::MAX_DEPTH {
            let mut next = path.to_vec();
            next.push(layer.0);
            append(app, root, &next, out, sources);
        }
    }
}

fn command(n: &InlineRow, id: &str, params: serde_json::Value) -> (String, serde_json::Value) {
    ("comp.inlineEdit".into(), json!({"root": n.root.0, "path": n.path, "command": id, "params": params}))
}

fn toggle(set: &mut std::collections::BTreeSet<u64>, id: u64) {
    if !set.remove(&id) {
        set.insert(id);
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn show(
    app: &mut dyn TimelineHost,
    ui: &mut egui::Ui,
    n: &InlineRow,
    r: Rect,
    cw: &Cols,
    tm: TMap,
    graph_on: bool,
    left_clip: Rect,
    right_clip: Rect,
    actions: &mut Vec<(String, serde_json::Value)>,
) {
    let Some(comp) = app.session().project.comp_arc(n.context.comp) else { return };
    let Some(layer) = super::super::layer_arc(&comp, n.row.layer) else { return };
    if !crate::widgets::any_menu_open(ui.ctx())
        && !egui::Popup::is_any_open(ui.ctx())
        && ui.input(|i| {
            i.pointer.any_pressed()
                && i.pointer.interact_pos().is_some_and(|p| r.intersect(left_clip.union(right_clip)).contains(p) && (!graph_on || p.x < right_clip.min.x))
        })
    {
        focus(app, n.root, &n.path);
    }
    let t = app.tokens();
    let sel = app.session().state.comp_selections.get(&n.context.comp).cloned().unwrap_or_default();
    let selected = sel.layers.contains(&layer.id);
    let locked = layer.switches.locked || n.context.locked;
    let cy = r.center().y;
    let indent = cw.name + 8.0 + 14.0 * (n.path.len() + n.row.depth) as f32;
    let prefix = format!("timeline.inline.{}.layer.{}", instance_key(n.root, &n.path).iter().map(u64::to_string).collect::<Vec<_>>().join("."), layer.id.0);
    let lp = ui.painter().with_clip_rect(left_clip);
    let gp = ui.painter().with_clip_rect(right_clip);
    lp.rect_filled(r, 0.0, if selected { t.row_selected } else { t.row_alt });
    gp.rect_filled(r, 0.0, t.tl_bg);
    lp.line_segment(
        [pos2(cw.name + 14.0 * n.path.len() as f32, r.min.y), pos2(cw.name + 14.0 * n.path.len() as f32, r.max.y)],
        Stroke::new(1.0, t.accent.gamma_multiply(0.5)),
    );
    let id = tl_id((
        &prefix,
        match n.row.kind {
            RowKind::Prop { uid } | RowKind::Group { uid, .. } => uid,
            _ => 0,
        },
    ));
    let mut local = Vec::new();
    let row_auto = match n.row.kind {
        RowKind::Prop { uid } => format!("{prefix}.prop.{uid}.row"),
        RowKind::Group { uid, .. } => format!("{prefix}.group.{uid}.row"),
        _ => format!("{prefix}.row"),
    };
    app.auto_mut().add(&row_auto, r.intersect(left_clip), &layer.name);
    match &n.row.kind {
        RowKind::Layer => {
            for (i, (icon, name, on)) in [
                (Icon::Eye, "video", layer.switches.video),
                (Icon::Speaker, "audio", layer.switches.audio),
                (Icon::Solo, "solo", layer.switches.solo),
                (Icon::Lock, "lock", layer.switches.locked),
            ]
            .into_iter()
            .enumerate()
            {
                let br = Rect::from_center_size(pos2(cw.av + 10.0 + i as f32 * 18.0, cy), vec2(15.0, 15.0));
                if !Vis::of(&app.ui().timeline).av {
                    continue;
                }
                icons::paint(&lp, br, icon, if on { t.text } else { t.text_faint });
                app.auto_mut().add(&format!("{prefix}.{name}"), br, name);
                if ui.interact(br, id.with(name), Sense::click()).clicked() && (!locked || name == "lock") && !n.context.locked {
                    local.push(("layer.setSwitch".into(), json!({"layers":[layer.id.0],"switch":name,"value":!on})));
                }
            }
            let tw = Rect::from_center_size(pos2(indent, cy), vec2(14.0, 14.0));
            if widgets::twirl(ui, tw, app.ui().timeline.open_layers.contains(&layer.id.0), id.with("props"), &t).clicked() {
                toggle(&mut app.ui_mut().timeline.open_layers, layer.id.0);
            }
            app.auto_mut().add(&format!("{prefix}.twirl"), tw, "Layer properties");
            expander(app, ui, n.root, &n.path, &layer, Rect::from_center_size(pos2(indent + 16.0, cy), vec2(14.0, 14.0)));
            let nr = Rect::from_min_max(pos2(indent + 28.0, r.min.y), pos2(cw.name_end - 4.0, r.max.y));
            let response = ui.interact(nr, id.with("name"), Sense::click()).on_hover_text(format!(
                "{} • shared source layers; edits affect every instance",
                app.session().project.item(n.context.comp).map(|item| item.name.as_str()).unwrap_or("Source")
            ));
            widgets::text_fit(&lp, pos2(nr.min.x, cy), Align2::LEFT_CENTER, &layer.name, Tokens::ui(12.0), nr.width(), t.text);
            app.auto_mut().add(&format!("{prefix}.name"), nr, &layer.name);
            if response.clicked() && !locked {
                local.push(("layer.select".into(), json!({"layers":[layer.id.0],"toggle":ui.input(|i|i.modifiers.command || i.modifiers.shift)})));
            }
            response.context_menu(|ui| {
                ui.label(tr("Shared source layer"));
                if ui.button(tr("Open source composition")).clicked() {
                    local.push(("comp.open".into(), json!({"comp":n.context.comp.0})));
                    ui.close();
                }
                let mut name = layer.name.clone();
                if ui.add_enabled(!locked, egui::TextEdit::singleline(&mut name)).changed() {
                    local.push(("layer.rename".into(), json!({"layer":layer.id.0,"name":name})));
                }
                for (label, direction) in [("Move up", "forward"), ("Move down", "backward")] {
                    if ui.add_enabled(!locked, egui::Button::new(label)).clicked() {
                        local.push(("layer.arrange".into(), json!({"layers":[layer.id.0],"to":direction})));
                        ui.close();
                    }
                }
                for (label, switch, on) in [
                    ("Effects", "fx", layer.switches.effects),
                    ("Motion blur", "motionBlur", layer.switches.motion_blur),
                    ("3D", "threeD", layer.is_3d()),
                    ("Shy", "shy", layer.switches.shy),
                    ("Collapse transformations", "collapse", layer.switches.collapse),
                ] {
                    let mut value = on;
                    if ui.add_enabled(!locked, egui::Checkbox::new(&mut value, label)).changed() {
                        local.push(("layer.setSwitch".into(), json!({"layers":[layer.id.0],"switch":switch,"value":value})));
                    }
                }
            });
            if !graph_on {
                if let Some((offset, scale)) = n.context.affine {
                    let a = tm.x(offset + scale * layer.in_point.seconds());
                    let b = tm.x(offset + scale * layer.out_point.seconds());
                    let bar = Rect::from_min_max(pos2(a.min(b), r.min.y + 3.0), pos2(a.max(b), r.max.y - 3.0)).intersect(right_clip);
                    gp.rect_filled(bar, 2.0, bar_color(t.label(layer.label), selected));
                    ui.set_clip_rect(right_clip);
                    let resp = ui.interact(bar, id.with("bar"), Sense::click_and_drag());
                    app.auto_mut().add(&format!("{prefix}.bar"), bar, "Move nested layer");
                    if resp.drag_started() {
                        ui.ctx().data_mut(|d| d.insert_temp(id.with("bar-origin"), layer.start_time.seconds()));
                    }
                    if resp.dragged() && !locked {
                        let origin = ui.ctx().data(|d| d.get_temp::<f64>(id.with("bar-origin"))).unwrap_or(layer.start_time.seconds());
                        let start = origin + resp.drag_delta().x as f64 / tm.pps / scale;
                        if start.is_finite() {
                            local.push(("layer.timing".into(), json!({"layers":[layer.id.0],"start":start,"merge":format!("bar-{}",layer.id.0)})));
                        }
                    }
                    if resp.drag_stopped() {
                        actions.push(("__endMerge".into(), json!({})));
                    }
                    ui.set_clip_rect(left_clip);
                } else {
                    gp.text(
                        pos2(tm.x0 + 8.0, cy),
                        Align2::LEFT_CENTER,
                        format!("Source {:.3}s • remapped timing", n.context.time.seconds()),
                        Tokens::ui(11.0),
                        t.text_dim,
                    );
                }
            }
        }
        RowKind::Group { uid, name, open, has_children, .. } => {
            let tw = Rect::from_center_size(pos2(indent, cy), vec2(14.0, 14.0));
            if *has_children && widgets::twirl(ui, tw, *open, id.with("group"), &t).clicked() {
                toggle(&mut app.ui_mut().timeline.open_groups, *uid);
            }
            app.auto_mut().add(&format!("{prefix}.group.{uid}.twirl"), tw, "Expand property group");
            lp.text(pos2(indent + 12.0, cy), Align2::LEFT_CENTER, name, Tokens::ui(12.0), t.text);
            if let Some(g) = layer.props.find_group(*uid).filter(|g| g.is_instance()) {
                let eye = Rect::from_center_size(pos2(cw.av + 10.0, cy), vec2(15.0, 15.0));
                icons::paint(&lp, eye, Icon::Eye, if g.enabled { t.text } else { t.text_faint });
                app.auto_mut().add(&format!("{prefix}.group.{uid}.eye"), eye, "Enable group");
                if ui.interact(eye, id.with("eye"), Sense::click()).clicked() && !locked {
                    local.push(("prop.setGroupEnabled".into(), json!({"layer":layer.id.0,"prop":uid,"value":!g.enabled})));
                }
            }
        }
        RowKind::Prop { uid } => {
            let Some(prop) = layer.props.find(*uid) else { return };
            let project = app.session().project.clone();
            let expr = app.session().expr.clone();
            let mut eval = EvalCtx::new(&project, n.context.comp, &comp, n.context.time);
            eval.expr = expr.as_deref();
            let value = eval.value(&layer, prop);
            let raw = prop.value_at(layer.layer_time(eval.time));
            let sw = Rect::from_center_size(pos2(indent, cy), vec2(15.0, 15.0));
            if !prop.static_only {
                icons::paint(&lp, sw, Icon::Stopwatch, if prop.is_animated() { t.hot_text } else { t.text_dim });
                app.auto_mut().add(&format!("{prefix}.prop.{uid}.stopwatch"), sw, "Toggle animation at nested source time");
                if ui.interact(sw, id.with("stopwatch"), Sense::click()).clicked() && !locked {
                    local.push(("prop.toggleAnimation".into(), json!({"layer":layer.id.0,"prop":uid})));
                }
                let key = Rect::from_center_size(pos2(cw.av + 30.0, cy), vec2(14.0, 14.0));
                let at_key = effectcraft_engine::keyframe::key_at(&prop.keys, layer.layer_time(n.context.time)).is_some();
                icons::paint(&lp, key, Icon::Keyframe, if at_key { t.hot_text } else { t.text_dim });
                app.auto_mut().add(&format!("{prefix}.prop.{uid}.addKey"), key, "Add/remove source keyframe");
                if ui.interact(key, id.with("add-key"), Sense::click()).clicked() && !locked {
                    local.push(("prop.toggleKey".into(), json!({"layer":layer.id.0,"prop":uid})));
                }
            }
            let nr = Rect::from_min_max(pos2(indent + 12.0, r.min.y), pos2((cw.vx - 4.0).max(indent + 110.0), r.max.y));
            widgets::text_fit(
                &lp,
                pos2(nr.min.x, cy),
                Align2::LEFT_CENTER,
                &prop.name,
                Tokens::ui(12.0),
                nr.width(),
                if sel.props.contains(&(layer.id, *uid)) { t.hot_text } else { t.text },
            );
            let name_response = ui.interact(nr, id.with("name"), Sense::click());
            app.auto_mut().add(&format!("{prefix}.prop.{uid}.name"), nr, &prop.name);
            if name_response.clicked() && !locked {
                local.push(("prop.select".into(), json!({"layer":layer.id.0,"prop":uid,"add":ui.input(|i|i.modifiers.shift)})));
            }
            name_response.context_menu(|ui| {
                if ui.button(tr("Open source composition")).clicked() {
                    local.push(("comp.open".into(), json!({"comp":n.context.comp.0})));
                    ui.close();
                }
                if !prop.static_only {
                    ui.label(tr("Expression"));
                    let buffer = id.with("expression-buffer");
                    let mut text =
                        ui.ctx().data(|d| d.get_temp::<String>(buffer)).unwrap_or_else(|| prop.expr.as_ref().map(|e| e.text.clone()).unwrap_or_default());
                    let response = ui.add_enabled(!locked, egui::TextEdit::multiline(&mut text).desired_width(320.0).desired_rows(3));
                    app.auto_mut().add(&format!("{prefix}.prop.{uid}.expression"), response.rect, "Expression");
                    if response.changed() {
                        ui.ctx().data_mut(|d| d.insert_temp(buffer, text.clone()));
                    }
                    let apply = ui.add_enabled(!locked, egui::Button::new(tr("Apply expression")));
                    app.auto_mut().add(&format!("{prefix}.prop.{uid}.applyExpression"), apply.rect, "Apply expression");
                    if apply.clicked() {
                        local.push(("prop.setExpression".into(), json!({"layer":layer.id.0,"prop":uid,"expression":text})));
                        ui.ctx().data_mut(|d| d.remove::<String>(buffer));
                        ui.close();
                    }
                }
            });
            let at = pos2((cw.vx + 4.0).max(nr.max.x + 5.0), cy);
            ui.add_enabled_ui(!locked, |ui| {
                if matches!(value, Value::Text(_) | Value::Str(_)) {
                    let mut text = value.to_json().as_str().unwrap_or_default().to_string();
                    let rr = Rect::from_min_size(pos2(at.x, at.y - 9.0), vec2(160.0, 18.0));
                    let response = ui.put(rr, egui::TextEdit::singleline(&mut text).id(id.with("text")));
                    app.auto_mut().add(&format!("{prefix}.prop.{uid}.value"), response.rect, "Edit source text");
                    if response.changed() {
                        local.push(("prop.set".into(), json!({"layer":layer.id.0,"prop":uid,"value":text,"merge":format!("text-{uid}")})));
                    }
                    if response.lost_focus() {
                        app.session_mut().history.merge_key = None;
                    }
                } else {
                    value_editor(app, ui, &lp, &layer, prop, &value, &raw, at, Some(&instance_key(n.root, &n.path)), &mut local);
                }
            });
            if !graph_on {
                draw_keys(app, ui, n, &layer, prop, r, tm, right_clip, &mut local);
            }
        }
        _ => {}
    }
    ui.set_clip_rect(left_clip);
    for (id, mut params) in local {
        if id == "prop.set" {
            params["selected"] = json!(false);
        }
        if id == "comp.open" {
            actions.push((id, params));
            continue;
        }
        if id == "prop.editGradient" {
            params["comp"] = json!(n.context.comp.0);
            let scope = crate::state::InlineScope { root: n.root.0, path: n.path.clone(), time: Some(n.context.time) };
            if let Err(error) = with_scope(app, &scope, |app| app.request_dialog(DialogRequest::Gradient(params.clone()))).and_then(|result| result) {
                app.ui_mut().status = error;
            }
            continue;
        }
        actions.push(command(n, &id, params));
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_keys(
    app: &mut dyn TimelineHost,
    ui: &mut egui::Ui,
    n: &InlineRow,
    layer: &Layer,
    prop: &Property,
    r: Rect,
    tm: TMap,
    clip: Rect,
    actions: &mut Vec<(String, serde_json::Value)>,
) {
    let Some((offset, scale)) = n.context.affine else { return };
    ui.set_clip_rect(clip);
    let p = ui.painter().with_clip_rect(clip);
    let t = app.tokens();
    let selected = app.session().state.comp_selections.get(&n.context.comp).map(|s| s.keys.clone()).unwrap_or_default();
    for (index, k) in prop.keys.iter().enumerate() {
        let root_time = offset + scale * layer.comp_time(k.time).seconds();
        let center = pos2(tm.x(root_time), r.center().y);
        if !clip.expand(8.0).contains(center) {
            continue;
        }
        let kr = effectcraft_engine::KeyRef { layer: layer.id, prop: prop.uid, time: k.time };
        let rect = Rect::from_center_size(center, vec2(12.0, 14.0));
        icons::paint(&p, rect, Icon::Keyframe, if selected.contains(&kr) { t.keyframe_selected } else { t.text_dim });
        let id = tl_id(("inline-key", n.root.0, &n.path, prop.uid, index));
        let resp =
            ui.interact(rect, id, Sense::click_and_drag()).on_hover_text(format!("Source {:.3}s; parent {:.3}s", layer.comp_time(k.time).seconds(), root_time));
        let auto = format!(
            "timeline.inline.{}.layer.{}.prop.{}.key.{index}",
            instance_key(n.root, &n.path).iter().map(u64::to_string).collect::<Vec<_>>().join("."),
            layer.id.0,
            prop.uid
        );
        app.auto_mut().add(&auto, rect, "Source keyframe");
        let select = json!({"keys":[{"layer":layer.id.0,"prop":prop.uid,"time":k.time.seconds()}],"selectProperties":true});
        if layer.switches.locked || n.context.locked {
            continue;
        }
        if resp.clicked() || (resp.drag_started() && !selected.contains(&kr)) {
            let mut args = select.clone();
            args["toggle"] = json!(resp.clicked() && ui.input(|i| i.modifiers.shift));
            actions.push(("keys.select".into(), args));
        }
        if resp.double_clicked() {
            app.session_mut().set_time(Tick::from_seconds_f64(root_time));
        }
        if resp.drag_started() {
            ui.ctx().data_mut(|d| d.insert_temp(id.with("key-applied"), 0.0_f64));
        }
        if resp.dragged()
            && let Some(total) = app.session().project.comp(n.context.comp).and_then(|c| snapped_drag(resp.drag_delta().x, tm.pps, scale, c.frame_rate))
        {
            let applied = ui.ctx().data(|d| d.get_temp::<f64>(id.with("key-applied"))).unwrap_or(0.0);
            if total != applied {
                actions.push(("keys.move".into(), json!({"delta":total-applied,"merge":format!("key-{}",prop.uid)})));
                ui.ctx().data_mut(|d| d.insert_temp(id.with("key-applied"), total));
            }
        }
        if resp.drag_stopped() {
            app.session_mut().history.merge_key = None;
        }
        resp.context_menu(|ui| {
            for (label, command) in [("Easy ease", "keys.easyEase"), ("Hold", "keys.toggleHold"), ("Delete keyframe", "keys.delete")] {
                if ui.button(label).clicked() {
                    actions.push(("keys.select".into(), select.clone()));
                    actions.push((command.into(), json!({})));
                    ui.close();
                }
            }
        });
    }
}

fn snapped_drag(pixels: f32, pps: f64, scale: f64, rate: effectcraft_engine::time::FrameRate) -> Option<f64> {
    if !pps.is_finite() || pps <= 0.0 || !scale.is_finite() || scale.abs() < 1e-9 {
        return None;
    }
    let seconds = pixels as f64 / pps / scale;
    seconds.is_finite().then(|| rate.snap_nearest(Tick::from_seconds_f64(seconds)).seconds())
}

fn focus(app: &mut dyn TimelineHost, root: ItemId, path: &[u64]) {
    if app.ui().timeline.inline_parent_selection.is_none() {
        let parts = app.parts();
        let state = &mut parts.session.state;
        parts.ui.timeline.inline_parent_text_edit = state.text_edit.take();
        parts.ui.timeline.inline_parent_selection = Some(effectcraft_engine::CompSelection {
            layers: std::mem::take(&mut state.selected_layers),
            props: std::mem::take(&mut state.selected_props),
            keys: std::mem::take(&mut state.selected_keys),
            vertices: std::mem::take(&mut state.selected_vertices),
        });
    }
    app.ui_mut().timeline.inline_focus = Some(instance_key(root, path));
}

pub(super) fn clear_focus(app: &mut dyn TimelineHost) {
    let text_edit = app.ui_mut().timeline.inline_parent_text_edit.take();
    let root = app.ui_mut().timeline.inline_focus.take().and_then(|p| p.first().copied()).map(ItemId);
    let Some(mut selection) = app.ui_mut().timeline.inline_parent_selection.take() else { return };
    let Some(root) = root else { return };
    let Some(comp) = app.session().project.comp(root) else { return };
    let text_edit = text_edit.filter(|edit| comp.layer(edit.layer).is_some_and(|layer| matches!(layer.source, LayerSource::Text)));
    selection.layers.retain(|id| comp.layer(*id).is_some());
    selection.props.retain(|(id, uid)| comp.layer(*id).is_some_and(|l| l.props.find(*uid).is_some() || l.props.find_group(*uid).is_some()));
    selection.keys.retain(|k| comp.layer(k.layer).and_then(|l| l.props.find(k.prop)).is_some_and(|p| p.keys.iter().any(|key| key.time == k.time)));
    selection.vertices.retain(|v| comp.layer(v.layer).and_then(|l| l.props.find_group(v.mask)).is_some());
    if app.session().active_comp_id() == Some(root) {
        let state = &mut app.session_mut().state;
        if state.text_edit.is_none() {
            state.text_edit = text_edit;
        }
        if state.selected_layers.is_empty() && state.selected_props.is_empty() && state.selected_keys.is_empty() && state.selected_vertices.is_empty() {
            state.selected_layers = selection.layers;
            state.selected_props = selection.props;
            state.selected_keys = selection.keys;
            state.selected_vertices = selection.vertices;
        }
    } else {
        app.session_mut().state.comp_selections.insert(root, selection);
    }
}

/// The focused instance; explicit outer-comp targets must not be captured.
fn current_scope(app: &dyn TimelineHost) -> Option<crate::state::InlineScope> {
    if app.ui().focused != crate::dock::PanelKind::Timeline {
        return None;
    }
    let focus = app.ui().timeline.inline_focus.as_ref()?;
    let (&root, path) = focus.split_first()?;
    (Some(root) == app.session().active_comp_id().map(|id| id.0)).then(|| crate::state::InlineScope { root, path: path.to_vec(), time: None })
}

fn scoped_params(scope: &crate::state::InlineScope, id: &str, params: serde_json::Value) -> serde_json::Value {
    let mut args = json!({"root":scope.root,"path":scope.path,"command":id,"params":params});
    if let Some(time) = scope.time {
        args["sourceTime"] = json!(time.seconds());
    }
    args
}

/// Keep UI dialogs, rich-clipboard dispatch and graph gestures in the same source cursor.
pub(super) fn with_scope<H: TimelineHost + ?Sized, R>(app: &mut H, scope: &crate::state::InlineScope, run: impl FnOnce(&mut H) -> R) -> Result<R, String> {
    let mut context =
        inline_comp::context(app.session(), ItemId(scope.root), &scope.path, app.session().time_of(ItemId(scope.root))).map_err(|e| e.to_string())?;
    if let Some(time) = scope.time {
        context.time = time;
    }
    let previous = app.session().state.active_comp;
    let old_time = app.session().state.times.get(&context.comp).copied();
    let track = app.session().state.current_track;
    let text = app.session_mut().state.text_edit.take();
    let dispatch = app.ui_mut().timeline.inline_dispatch.replace(scope.clone());
    let old_dialog = app.dialog();
    app.session_mut().activate_comp(context.comp);
    app.session_mut().state.times.insert(context.comp, context.time);
    let result = effectcraft_engine::guard::guarded("inline source UI", || Ok(run(app))).map_err(|error| error.to_string());
    if old_dialog.is_none() && app.dialog().is_some() {
        app.ui_mut().timeline.inline_dialog = Some(crate::state::InlineScope { time: Some(context.time), ..scope.clone() });
    }
    match previous {
        Some(id) => app.session_mut().activate_comp(id),
        None => app.session_mut().activate_comp_none(),
    }
    match old_time {
        Some(time) => {
            app.session_mut().state.times.insert(context.comp, time);
        }
        None => {
            app.session_mut().state.times.remove(&context.comp);
        }
    }
    app.session_mut().state.current_track = track;
    app.session_mut().state.text_edit = text;
    app.ui_mut().timeline.inline_dispatch = dispatch;
    result
}

pub(super) fn invoke(app: &mut EffectcraftApp, ctx: &egui::Context, id: &str, params: &serde_json::Value) -> Option<Result<serde_json::Value, String>> {
    if app.ui().timeline.inline_dispatch.is_some() || id == "comp.inlineEdit" {
        return None;
    }
    let scope = current_scope(app)?;
    if params.get("comp").is_some() {
        return None;
    }
    if matches!(id, "time.nextKey" | "time.previousKey") {
        let navigation = (|| {
            let context = inline_comp::context(app.session(), ItemId(scope.root), &scope.path, app.session().time_of(ItemId(scope.root)))
                .map_err(|error| error.to_string())?;
            let Some((offset, scale)) = context.affine else {
                return Err(tr("Use the parent ruler to scrub a time-remapped instance; source keys remain editable.").into());
            };
            let source_time = with_scope(app, &scope, |app| {
                let mut params = params.clone();
                if app.ui().timeline.graph_editor
                    && let Some(comp) = app.session().active_comp_arc()
                {
                    params["visible"] = json!(
                        super::super::graph::shown_props(app, &comp).iter().map(|(layer, prop)| json!({"layer":layer.0,"prop":prop})).collect::<Vec<_>>()
                    );
                }
                app.invoke(ctx, id, params).map(|_| app.session().time())
            })??;
            let target = offset + scale * source_time.seconds();
            if !target.is_finite() {
                return Err(tr("Invalid nested keyframe time").into());
            }
            app.run_engine(ctx, "time.set", json!({"time":target}))
        })();
        return Some(navigation);
    }
    let source_ui = id.starts_with("timeline.reveal") || id.starts_with("timeline.keyAt") || id == "anim.reveal";
    let source_graph_zoom =
        app.ui().timeline.graph_editor && matches!(id, "timeline.zoomIn" | "timeline.zoomOut" | "timeline.zoomFit" | "timeline.zoomFrameToggle");
    if inline_comp::supported(id) || source_ui || source_graph_zoom {
        return Some(
            with_scope(app, &scope, |app| {
                let view = source_graph_zoom.then(|| take_graph_view(app));
                if source_graph_zoom {
                    let comp = app.session().active_comp_id().map(|c| c.0).unwrap_or(0);
                    load_graph_view(app, comp);
                }
                let result = app.invoke(ctx, id, params.clone());
                if let Some(view) = view {
                    let comp = app.session().active_comp_id().map(|c| c.0).unwrap_or(0);
                    store_graph_view(app, comp);
                    put_graph_view(app, view);
                }
                result
            })
            .and_then(|result| result),
        );
    }
    // Graph mode is a panel preference; opening it leaves the root viewer active.
    if id == "timeline.graphEditor" {
        return None;
    }
    if ["layer.", "prop.", "keys.", "mask.", "shape.", "anim.", "timeline.trim", "timeline.moveIn", "timeline.moveOut", "viewer.nudge"]
        .iter()
        .any(|prefix| id.starts_with(prefix))
        && ["layer", "layers", "prop", "keys"].iter().all(|key| params.get(key).is_none())
    {
        return Some(Err(tr("This action needs the source composition open; the parent was not changed.").into()));
    }
    None
}

pub(super) fn engine_target(app: &dyn TimelineHost, id: &str, params: &serde_json::Value) -> Option<serde_json::Value> {
    let scope = app.ui().timeline.inline_dispatch.as_ref()?;
    inline_comp::supported(id).then(|| scoped_params(scope, id, params.clone()))
}

pub(super) fn enabled(app: &EffectcraftApp, id: &str) -> Option<bool> {
    if app.ui().timeline.inline_dispatch.is_some() {
        return None;
    }
    let scope = current_scope(app)?;
    let context = inline_comp::context(app.session(), ItemId(scope.root), &scope.path, app.session().time_of(ItemId(scope.root))).ok()?;
    let selection = app.session().state.comp_selections.get(&context.comp).cloned().unwrap_or_default();
    let st = &app.session().state;
    let selected = match id {
        "edit.clear" | "edit.copy" | "edit.cut" | "edit.copyWithPropertyLinks" | "edit.copyWithRelativePropertyLinks" | "edit.copyExpressionOnly" => {
            !selection.layers.is_empty() || !selection.props.is_empty() || !selection.keys.is_empty()
        }
        "edit.duplicate" | "layer.setSwitch" => !selection.layers.is_empty(),
        "keys.copy" | "keys.delete" | "keys.easyEase" | "keys.easyEaseIn" | "keys.easyEaseOut" | "keys.toggleHold" | "keys.interpolation" | "keys.velocity" => {
            !selection.keys.is_empty()
        }
        "edit.paste" => {
            !st.clipboard.is_empty()
                || !st.key_clipboard.is_empty()
                || !st.value_clipboard.is_empty()
                || !st.effect_clipboard.is_empty()
                || !st.contents_clipboard.is_empty()
                || st.link_clipboard.is_some()
                || app
                    .hooks
                    .clipboard_text
                    .as_ref()
                    .and_then(|read| read())
                    .is_some_and(|text| effectcraft_engine::figma::clipboard::is_clipboard_text(&text) || effectcraft_engine::vector::is_svg_text(&text))
        }
        "keys.paste" => !st.key_clipboard.is_empty(),
        "edit.selectAll" => app.session().project.comp(context.comp).is_some_and(|c| !c.layers.is_empty()),
        "edit.deselectAll" => !selection.layers.is_empty() || !selection.props.is_empty() || !selection.keys.is_empty(),
        "prop.reset" => !selection.props.is_empty(),
        _ => return None,
    };
    Some(selected && !context.locked)
}

fn take_graph_view(app: &dyn TimelineHost) -> crate::state::InlineGraphView {
    let tl = &app.ui().timeline;
    crate::state::InlineGraphView { start: tl.start, pps: tl.pps, range: tl.graph_range, before_fit: tl.before_fit, auto_zoom: tl.graph_auto_zoom }
}
fn put_graph_view(app: &mut dyn TimelineHost, view: crate::state::InlineGraphView) {
    let tl = &mut app.ui_mut().timeline;
    tl.start = view.start;
    tl.pps = view.pps;
    tl.graph_range = view.range;
    tl.before_fit = view.before_fit;
    tl.graph_auto_zoom = view.auto_zoom;
}
fn load_graph_view(app: &mut dyn TimelineHost, comp: u64) {
    let view = app.ui().timeline.inline_graph_views.get(&comp).cloned().unwrap_or(crate::state::InlineGraphView { auto_zoom: true, ..Default::default() });
    put_graph_view(app, view);
}
fn store_graph_view(app: &mut dyn TimelineHost, comp: u64) {
    let view = take_graph_view(app);
    app.ui_mut().timeline.inline_graph_views.insert(comp, view);
}

pub(super) struct GraphLayout {
    pub comp: std::sync::Arc<Comp>,
    pub time: Tick,
    pub tm: TMap,
    restore: crate::state::InlineGraphView,
    source: u64,
}

pub(super) fn finish_graph(app: &mut dyn TimelineHost, layout: GraphLayout) {
    store_graph_view(app, layout.source);
    put_graph_view(app, layout.restore);
}

/// The parent ruler continues to scrub the parent. Source curves have their own labelled
/// source-time ruler, so reverse stretch/remapping never inverts key edits or mislabels speed.
pub(super) fn show_graph(app: &mut dyn TimelineHost, ui: &mut egui::Ui, p: &egui::Painter, area: Rect) -> Option<GraphLayout> {
    let scope = current_scope(app)?;
    let context = match inline_comp::context(app.session(), ItemId(scope.root), &scope.path, app.session().time_of(ItemId(scope.root))) {
        Ok(context) => context,
        Err(error) => {
            app.ui_mut().status = error.to_string();
            return None;
        }
    };
    let comp = app.session().project.comp_arc(context.comp)?;
    let restore = take_graph_view(app);
    load_graph_view(app, context.comp.0);
    if app.ui().timeline.inline_graph_active != Some(context.comp.0) {
        super::super::graph::reset_context(ui.ctx());
        app.session_mut().history.merge_key = None;
        app.ui_mut().timeline.inline_graph_active = Some(context.comp.0);
    }
    let fit = fit_pps(area.width(), &comp);
    let tm = TMap { x0: area.min.x, start: app.ui().timeline.start, pps: app.ui().timeline.pps.unwrap_or(fit) };
    let caption = Rect::from_min_max(area.min, pos2(area.max.x, area.min.y + 22.0));
    let ruler = Rect::from_min_max(pos2(area.min.x, caption.max.y), pos2(area.max.x, caption.max.y + 20.0));
    let graph = Rect::from_min_max(pos2(area.min.x, ruler.max.y), area.max);
    let label = app.session().project.item(context.comp).map(|item| item.name.as_str()).unwrap_or("Source");
    let title = crate::i18n::tr_args("Source graph: {} · {} s", &[&label, &format!("{:.3}", context.time.seconds())]);
    p.rect_filled(caption, 0.0, app.tokens().panel_bg);
    p.text(caption.left_center() + vec2(8.0, 0.0), Align2::LEFT_CENTER, &title, Tokens::ui(11.5), app.tokens().accent);
    app.auto_mut().add("timeline.inlineGraph.source", caption, &title);
    p.rect_filled(ruler, 0.0, app.tokens().tl_ruler_bg);
    for i in 0..=8 {
        let x = ruler.min.x + ruler.width() * i as f32 / 8.0;
        let seconds = tm.t(x);
        p.line_segment([pos2(x, ruler.max.y - 5.0), pos2(x, ruler.max.y)], Stroke::new(1.0, app.tokens().text_faint));
        p.text(pos2(x + 3.0, ruler.min.y + 8.0), Align2::LEFT_CENTER, format!("{seconds:.2}s"), Tokens::ui(10.0), app.tokens().text_dim);
    }
    let response = ui.interact(ruler, tl_id("inline-graph-ruler"), Sense::click_and_drag());
    app.auto_mut().add("timeline.inlineGraph.ruler", ruler, &format!("{},{}", tm.start, tm.pps));
    if (response.clicked() || response.dragged())
        && let Some(pointer) = response.interact_pointer_pos()
    {
        if let Some((offset, scale)) = context.affine {
            let source = comp.frame_rate.snap_nearest(Tick::from_seconds_f64(tm.t(pointer.x)));
            app.session_mut().set_time(Tick::from_seconds_f64(offset + scale * source.seconds()));
        } else {
            app.ui_mut().status = tr("Use the parent ruler to scrub a time-remapped instance; source keys remain editable.").into();
        }
    }
    let result = with_scope(app, &scope, |app| {
        let project = app.session().project.clone();
        let expr = app.session().expr.clone();
        let mut eval = EvalCtx::new(&project, context.comp, &comp, context.time);
        eval.expr = expr.as_deref();
        let mut actions = vec![];
        super::super::graph::show(app, ui, p, &comp, &eval, tm, graph, &mut actions);
        for (id, params) in actions {
            if id == "__endMerge" {
                app.session_mut().history.merge_key = None;
                continue;
            }
            if let Err(error) = app.invoke(ui.ctx(), &id, params) {
                app.ui_mut().status = error;
            }
        }
    });
    if let Err(error) = result {
        app.ui_mut().status = error;
    }
    let x = tm.x(context.time.seconds());
    if x.is_finite() && x >= area.min.x && x <= area.max.x {
        p.line_segment([pos2(x, ruler.min.y), pos2(x, graph.max.y - 28.0)], Stroke::new(1.0, app.tokens().cti));
    }
    Some(GraphLayout { comp, time: context.time, tm, restore, source: context.comp.0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use effectcraft_engine::Session;

    fn fixture() -> (EffectcraftApp, ItemId, ItemId, u64, u64, u64) {
        let mut s = Session::default();
        let child = ItemId(s.execute("comp.new", json!({"name":"Source"})).unwrap()["comp"].as_u64().unwrap());
        let leaf = s.execute("layer.newSolid", json!({"name":"Shared layer"})).unwrap()["layer"].as_u64().unwrap();
        let root = ItemId(s.execute("comp.new", json!({"name":"Parent"})).unwrap()["comp"].as_u64().unwrap());
        let a = s.execute("layer.addItem", json!({"item":child.0})).unwrap()["layer"].as_u64().unwrap();
        let b = s.execute("layer.addItem", json!({"item":child.0})).unwrap()["layer"].as_u64().unwrap();
        (EffectcraftApp::new(s), root, child, leaf, a, b)
    }

    #[test]
    fn repeated_sources_keep_instance_times_and_refresh_expansion_on_each_pass() {
        let (mut app, root, child, leaf, a, b) = fixture();
        app.ui.timeline.inline_precomps.extend([instance_key(root, &[a]), instance_key(root, &[b])]);
        std::sync::Arc::make_mut(&mut app.session.project).comp_mut(root).unwrap().layer_mut(LayerId(b)).unwrap().start_time = Tick::from_seconds_f64(1.0);
        app.session.set_time(Tick::from_seconds_f64(3.0));
        let projected = |app: &EffectcraftApp| expand(app, root, build_rows(app, app.session.project.comp(root).unwrap()));
        let first = projected(&app);
        let nested: Vec<_> = first.iter().filter_map(|r| if let RowKind::Inline(n) = &r.kind { Some(n) } else { None }).collect();
        assert_eq!(nested.len(), 2);
        assert_eq!(nested[0].context.comp, child);
        assert_eq!(nested[1].context.comp, child);
        assert!((nested[0].context.time.seconds() - nested[1].context.time.seconds()).abs() > 0.9);
        assert_ne!(nested[0].path, nested[1].path);

        let transform = app.session.project.comp(child).unwrap().layer(LayerId(leaf)).unwrap().transform().unwrap().uid;
        app.ui.timeline.open_layers.insert(leaf);
        app.ui.timeline.open_groups.insert(transform);
        let open = projected(&app);
        assert!(open.iter().any(|r| matches!(&r.kind, RowKind::Inline(n) if matches!(n.row.kind, RowKind::Prop { .. }))));
        app.ui.timeline.open_groups.remove(&transform);
        let closed = projected(&app);
        assert!(!closed.iter().any(|r| matches!(&r.kind, RowKind::Inline(n) if matches!(n.row.kind, RowKind::Prop { .. }))));
        assert_eq!(app.session.active_comp_id(), Some(root));
    }

    fn select_source(app: &mut EffectcraftApp, root: ItemId, leaf: u64, instance: u64) {
        app.ui.focused = crate::dock::PanelKind::Timeline;
        focus(app, root, &[instance]);
        app.session.execute("comp.inlineEdit", json!({"root":root.0,"path":[instance],"command":"layer.select","params":{"layers":[leaf]}})).unwrap();
    }

    #[test]
    fn inline_layer_copy_cut_paste_updates_shared_source_once_and_preserves_parent() {
        let (mut app, root, child, leaf, a, b) = fixture();
        let ctx = egui::Context::default();
        select_source(&mut app, root, leaf, a);
        crate::menus::invoke(&mut app, &ctx, "edit.copy", json!({})).unwrap();
        assert_eq!(app.session.state.clipboard.len(), 1);
        assert_eq!(app.session.state.clipboard[0].id, LayerId(leaf));
        focus(&mut app, root, &[b]);
        let undo = app.session.history.undo.len();
        crate::menus::invoke(&mut app, &ctx, "edit.paste", json!({})).unwrap();
        assert_eq!(app.session.history.undo.len(), undo + 1);
        assert_eq!(app.session.project.comp(child).unwrap().layers.len(), 2);
        assert_eq!(app.session.project.comp(root).unwrap().layers.len(), 2);
        assert_eq!(app.session.active_comp_id(), Some(root));
        let open = app.session.state.open_comps.clone();
        crate::menus::invoke(&mut app, &ctx, "edit.cut", json!({})).unwrap();
        assert_eq!(app.session.project.comp(child).unwrap().layers.len(), 1);
        assert!(app.session.undo());
        assert_eq!(app.session.project.comp(child).unwrap().layers.len(), 2);
        assert_eq!(app.session.state.open_comps, open);
        assert_eq!(app.session.active_comp_id(), Some(root));
    }

    #[test]
    fn inline_key_clipboard_uses_instance_source_time_with_reverse_stretch() {
        let (mut app, root, child, leaf, a, _) = fixture();
        app.session.execute("prop.addKey", json!({"comp":child.0,"layer":leaf,"path":"transform/opacity","time":1,"value":40})).unwrap();
        let prop = app.session.project.comp(child).unwrap().layer(LayerId(leaf)).unwrap().props.prop("transform/opacity").unwrap().uid;
        let instance = std::sync::Arc::make_mut(&mut app.session.project).comp_mut(root).unwrap().layer_mut(LayerId(a)).unwrap();
        instance.start_time = Tick::from_seconds_f64(10.0);
        instance.stretch = -200.0;
        app.session.set_time(Tick::from_seconds_f64(4.0));
        select_source(&mut app, root, leaf, a);
        app.session
            .execute(
                "comp.inlineEdit",
                json!({"path":[a],"command":"keys.select","params":{"keys":[{"layer":leaf,"prop":prop,"time":1}],"selectProperties":true}}),
            )
            .unwrap();
        let ctx = egui::Context::default();
        crate::menus::invoke(&mut app, &ctx, "edit.copy", json!({})).unwrap();
        assert!(app.session.state.clip_is_keys);
        crate::menus::invoke(&mut app, &ctx, "edit.paste", json!({})).unwrap();
        let property = app.session.project.comp(child).unwrap().layer(LayerId(leaf)).unwrap().props.find(prop).unwrap();
        // Fixture uses 29.97 fps; assert the destination's actual frame-snapped source time.
        let source = inline_comp::context(&app.session, root, &[a], app.session.time()).unwrap().time;
        assert!(property.keys.iter().any(|key| (key.time.seconds() - source.seconds()).abs() < 0.02));
        assert_eq!(app.session.active_comp_id(), Some(root));
        assert_eq!(app.session.project.comp(root).unwrap().layers.len(), 2);
    }

    fn rich_clipboard() -> String {
        json!({"format":"figcraft.clipboard","version":1,"warnings":[],"nodes":[
            {"id":"frame","type":"FRAME","name":"Card","size":{"x":100,"y":100},"absoluteTransform":[[1,0,0],[0,1,0]],"children":[
            {"id":"rect","type":"RECTANGLE","name":"Imported","size":{"x":20,"y":20},"absoluteTransform":[[1,0,0],[0,1,0]],"fills":[{"type":"SOLID","color":{"r":1,"g":0,"b":0,"a":1}}]}]}]}).to_string()
    }

    #[test]
    fn inline_rich_paste_uses_fake_clipboard_and_never_opens_a_new_composition() {
        let (mut app, root, child, leaf, a, _) = fixture();
        select_source(&mut app, root, leaf, a);
        let ctx = egui::Context::default();
        let open = app.session.state.open_comps.clone();
        app.hooks.clipboard_text = Some(Box::new(|| Some(rich_clipboard())));
        assert_eq!(enabled(&app, "edit.paste"), Some(true));
        let undo = app.session.history.undo.len();
        crate::menus::invoke(&mut app, &ctx, "edit.paste", json!({})).unwrap();
        assert_eq!(app.session.history.undo.len(), undo + 1);
        // A pasted frame becomes a precomp layer holding the frame's children.
        let project = &app.session.project;
        let card = project.comp(child).unwrap().layers.iter().find(|l| l.name == "Card").expect("the pasted frame is a layer of the inline source");
        let effectcraft_project::LayerSource::Comp { item } = card.source else { panic!("a pasted frame is a precomp layer") };
        assert!(project.comp(item).unwrap().layers.iter().any(|l| l.name == "Imported"));
        assert_eq!(app.session.project.comp(root).unwrap().layers.len(), 2);
        assert_eq!(app.session.state.open_comps, open);
        assert_eq!(app.session.active_comp_id(), Some(root));
        crate::menus::invoke(&mut app, &ctx, "edit.copy", json!({})).unwrap();
        app.hooks.clipboard_text = Some(Box::new(|| Some("{\"format\":\"figcraft.clipboard\",broken".into())));
        let before = app.session.project.clone();
        assert!(crate::menus::invoke(&mut app, &ctx, "edit.paste", json!({})).is_err());
        assert!(app.session.project.same_content(&before));
    }

    #[test]
    fn inline_key_navigation_maps_source_keys_back_to_parent_and_refuses_ambiguous_remap() {
        let (mut app, root, child, leaf, a, _) = fixture();
        app.session.execute("prop.addKey", json!({"comp":child.0,"layer":leaf,"path":"transform/opacity","time":1,"value":40})).unwrap();
        app.session.execute("prop.addKey", json!({"comp":child.0,"layer":leaf,"path":"transform/opacity","time":3,"value":80})).unwrap();
        let instance = std::sync::Arc::make_mut(&mut app.session.project).comp_mut(root).unwrap().layer_mut(LayerId(a)).unwrap();
        instance.start_time = Tick::from_seconds_f64(1.0);
        instance.stretch = 200.0;
        app.session.set_time(Tick::from_seconds_f64(4.0));
        select_source(&mut app, root, leaf, a);
        app.ui.timeline.graph_editor = true;
        let ctx = egui::Context::default();
        crate::menus::invoke(&mut app, &ctx, "time.nextKey", json!({})).unwrap();
        assert!((app.session.time().seconds() - 7.0).abs() < 0.05);
        assert_eq!(app.session.active_comp_id(), Some(root));
        crate::menus::invoke(&mut app, &ctx, "time.previousKey", json!({})).unwrap();
        assert!((app.session.time().seconds() - 3.0).abs() < 0.05);
        app.session.execute("layer.enableTimeRemap", json!({"layers":[a]})).unwrap();
        let before = app.session.time();
        assert!(crate::menus::invoke(&mut app, &ctx, "time.nextKey", json!({})).is_err());
        assert_eq!(app.session.time(), before);
    }

    #[test]
    fn source_modal_commit_revalidates_locked_and_deleted_instances() {
        let (mut app, root, child, leaf, a, _) = fixture();
        select_source(&mut app, root, leaf, a);
        let scope = crate::state::InlineScope { root: root.0, path: vec![a], time: Some(Tick::ZERO) };
        let ctx = egui::Context::default();
        std::sync::Arc::make_mut(&mut app.session.project).comp_mut(root).unwrap().layer_mut(LayerId(a)).unwrap().switches.locked = true;
        let before = app.session.project.clone();
        let result = with_scope(&mut app, &scope, |app| {
            crate::menus::run_engine(app, &ctx, "prop.set", json!({"comp":child.0,"layer":leaf,"path":"transform/opacity","value":20}))
        });
        assert!(result.unwrap().is_err());
        assert!(app.session.project.same_content(&before));
        std::sync::Arc::make_mut(&mut app.session.project).comp_mut(root).unwrap().layers.retain(|layer| layer.id != LayerId(a));
        assert!(with_scope(&mut app, &scope, |_| ()).is_err());
        assert_eq!(app.session.active_comp_id(), Some(root));
    }

    #[test]
    fn inline_graph_dialogs_keep_source_keys_and_source_time_when_parent_scrubs() {
        let (mut app, root, child, leaf, a, _) = fixture();
        app.session.execute("prop.addKey", json!({"comp":child.0,"layer":leaf,"path":"transform/opacity","time":1,"value":40})).unwrap();
        let prop = app.session.project.comp(child).unwrap().layer(LayerId(leaf)).unwrap().props.prop("transform/opacity").unwrap().uid;
        select_source(&mut app, root, leaf, a);
        app.session
            .execute(
                "comp.inlineEdit",
                json!({"path":[a],"command":"keys.select","params":{"keys":[{"layer":leaf,"prop":prop,"time":1}],"selectProperties":true}}),
            )
            .unwrap();
        let ctx = egui::Context::default();
        crate::menus::invoke(&mut app, &ctx, "keys.velocity", json!({})).unwrap();
        assert_eq!(app.dialog, Some(crate::Dialog::KeyVelocity));
        let scope = app.ui.timeline.inline_dialog.clone().unwrap();
        app.session.set_time(Tick::from_seconds_f64(5.0));
        with_scope(&mut app, &scope, |app| crate::menus::run_engine(app, &ctx, "keys.interpolation", json!({"interpolation":"hold"}))).unwrap().unwrap();
        let key = &app.session.project.comp(child).unwrap().layer(LayerId(leaf)).unwrap().props.find(prop).unwrap().keys[0];
        assert_eq!(key.out_interp, Interp::Hold);
        assert_eq!(app.session.active_comp_id(), Some(root));
        app.dialog = None;
        app.ui.timeline.inline_dialog = None;
        crate::menus::invoke(&mut app, &ctx, "keys.set", json!({"layer":leaf,"prop":prop,"time":1})).unwrap();
        assert_eq!(app.dialog, Some(crate::Dialog::Form));
        assert_eq!(app.dialog_state.form.command, "keys.set");
        assert_eq!(app.session.active_comp_id(), Some(root));
    }

    #[test]
    fn graph_source_projection_and_zoom_leave_parent_view_and_cursor_intact() {
        let (mut app, root, child, leaf, a, _) = fixture();
        app.session.execute("prop.addKey", json!({"comp":child.0,"layer":leaf,"path":"transform/opacity","time":1,"value":40})).unwrap();
        select_source(&mut app, root, leaf, a);
        app.ui.timeline.graph_editor = true;
        app.ui.timeline.start = 2.0;
        app.ui.timeline.pps = Some(123.0);
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 400.0));
            let p = ui.painter().clone();
            let layout = show_graph(&mut app, ui, &p, rect).unwrap();
            assert_eq!(layout.comp.layers.len(), 1);
            assert!(app.auto.find("timeline.inlineGraph.source").is_some());
            assert!(app.auto.find("timeline.graph").is_some());
            finish_graph(&mut app, layout);
        });
        out.textures_delta.clear();
        assert_eq!(app.session.active_comp_id(), Some(root));
        assert_eq!(app.ui.timeline.start, 2.0);
        assert_eq!(app.ui.timeline.pps, Some(123.0));
        assert!(app.ui.timeline.inline_graph_views.contains_key(&child.0));
    }

    #[test]
    fn slow_key_drag_accumulates_before_snapping_including_reverse_stretch() {
        let rate = effectcraft_engine::time::FrameRate::FPS_30;
        for scale in [2.0, -2.0] {
            let mut applied = 0.0;
            let mut steps = 0.0;
            for i in 1..=40 {
                let total = snapped_drag(i as f32 * 0.1, 60.0, scale, rate).unwrap();
                steps += total - applied;
                applied = total;
            }
            assert!((steps - scale.signum() / 30.0).abs() < 1e-9);
        }
        assert_eq!(snapped_drag(1.0, 60.0, 0.0, rate), None);
    }

    #[test]
    fn leaving_inline_focus_keeps_a_new_viewer_selection_and_enables_source_menus() {
        let (mut app, root, child, leaf, a, b) = fixture();
        app.ui.focused = crate::dock::PanelKind::Timeline;
        app.session.execute("layer.select", json!({"layers":[a]})).unwrap();
        focus(&mut app, root, &[a]);
        app.session.execute("comp.inlineEdit", json!({"path":[a],"command":"layer.select","params":{"layers":[leaf]}})).unwrap();
        assert_eq!(enabled(&app, "edit.clear"), Some(true));
        assert_eq!(enabled(&app, "edit.duplicate"), Some(true));
        assert_eq!(enabled(&app, "keys.easyEase"), Some(false));
        assert_eq!(app.session.state.comp_selections[&child].layers, vec![LayerId(leaf)]);
        app.session.execute("layer.select", json!({"layers":[b]})).unwrap();
        clear_focus(&mut app);
        assert_eq!(app.session.state.selected_layers, vec![LayerId(b)]);
    }

    #[test]
    fn inline_focus_hides_parent_inspector_selection_and_restores_on_exit() {
        let (mut app, root, _, _, a, b) = fixture();
        app.session.execute("layer.select", json!({"layers":[b]})).unwrap();
        focus(&mut app, root, &[a]);
        assert!(app.session.state.selected_layers.is_empty());
        assert!(app.session.state.selected_props.is_empty());
        clear_focus(&mut app);
        assert_eq!(app.session.state.selected_layers, vec![LayerId(b)]);
        assert!(app.ui.timeline.inline_focus.is_none());
    }

    #[test]
    fn inline_delete_routes_to_source_and_unsupported_shortcuts_never_touch_parent() {
        let (mut app, root, child, leaf, a, _) = fixture();
        app.ui.focused = crate::dock::PanelKind::Timeline;
        app.ui.timeline.inline_focus = Some(instance_key(root, &[a]));
        app.session.execute("comp.inlineEdit", json!({"path":[a],"command":"layer.select","params":{"layers":[leaf]}})).unwrap();
        let ctx = egui::Context::default();
        assert!(crate::menus::invoke(&mut app, &ctx, "layer.precompose", json!({})).is_err());
        assert_eq!(app.session.project.comp(root).unwrap().layers.len(), 2);
        crate::menus::invoke(&mut app, &ctx, "edit.clear", json!({})).unwrap();
        assert_eq!(app.session.project.comp(root).unwrap().layers.len(), 2);
        assert!(app.session.project.comp(child).unwrap().layers.is_empty());
        assert_eq!(app.session.active_comp_id(), Some(root));
    }

    #[test]
    fn repeated_instances_have_separate_expansion_and_shared_layer_identity() {
        let (mut app, root, child, leaf, a, b) = fixture();
        app.ui.timeline.inline_precomps.insert(instance_key(root, &[a]));
        let rows = expand(&app, root, build_rows(&app, app.session.project.comp(root).unwrap()));
        let projected: Vec<_> = rows
            .iter()
            .filter_map(|r| match &r.kind {
                RowKind::Inline(n) => Some(n),
                _ => None,
            })
            .collect();
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].path, vec![a]);
        assert_eq!(projected[0].context.comp, child);
        assert_eq!(projected[0].row.layer, LayerId(leaf));
        app.ui.timeline.inline_precomps.insert(instance_key(root, &[b]));
        let rows = expand(&app, root, build_rows(&app, app.session.project.comp(root).unwrap()));
        let projected: Vec<_> = rows
            .iter()
            .filter_map(|r| match &r.kind {
                RowKind::Inline(n) => Some(n),
                _ => None,
            })
            .collect();
        assert_eq!(projected.len(), 2);
        assert_eq!(projected[0].row.layer, projected[1].row.layer);
        assert_ne!(projected[0].path, projected[1].path);
        assert_eq!(app.session.project.comp(child).unwrap().layers.len(), 1);
    }

    #[test]
    fn nested_property_rows_are_not_copied_or_bound_to_parent_comp() {
        let (mut app, root, child, leaf, a, _) = fixture();
        app.ui.timeline.inline_precomps.insert(instance_key(root, &[a]));
        app.ui.timeline.open_layers.insert(leaf);
        let group = app.session.project.comp(child).unwrap().layer(LayerId(leaf)).unwrap().transform().unwrap().uid;
        app.ui.timeline.open_groups.insert(group);
        let rows = expand(&app, root, build_rows(&app, app.session.project.comp(root).unwrap()));
        let nested = rows
            .iter()
            .find_map(|r| match &r.kind {
                RowKind::Inline(n) if matches!(n.row.kind, RowKind::Prop { .. }) => Some(n),
                _ => None,
            })
            .unwrap();
        let (id, args) = command(nested, "prop.set", json!({"layer":leaf,"path":"transform/opacity","value":25}));
        app.session.execute(&id, args).unwrap();
        assert_eq!(app.session.active_comp_id(), Some(root));
        assert_eq!(app.session.project.comp(child).unwrap().layer(LayerId(leaf)).unwrap().props.prop("transform/opacity").unwrap().value.as_f64(), 25.0);
    }
}

#[cfg(test)]
mod interaction_profile {
    use super::*;
    use std::{hint::black_box, time::Instant};

    #[test]
    #[ignore = "explicit CPU profiling probe; run without concurrent builds"]
    fn expanded_row_cost() {
        let repeats = std::env::var("EC_PROFILE_REPEATS").ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or(25).clamp(5, 200);
        for (layers, instances, properties) in [(50, 1, false), (500, 1, false), (500, 20, false), (100, 20, true)] {
            let mut session = effectcraft_engine::Session::default();
            let source = ItemId(session.execute("comp.new", json!({"name":"Synthetic source","duration":10})).unwrap()["comp"].as_u64().unwrap());
            let mut leaves = Vec::new();
            let mut groups = Vec::new();
            for i in 0..layers {
                let lid =
                    LayerId(session.execute("layer.newSolid", json!({"name":format!("Leaf {i}"),"width":10,"height":10})).unwrap()["layer"].as_u64().unwrap());
                leaves.push(lid.0);
                groups.push(session.active_comp().unwrap().layer(lid).unwrap().transform().unwrap().uid);
            }
            let root = ItemId(session.execute("comp.new", json!({"name":"Synthetic root","duration":10})).unwrap()["comp"].as_u64().unwrap());
            let mut paths = Vec::new();
            for _ in 0..instances {
                let lid = session.execute("layer.addItem", json!({"item":source.0})).unwrap()["layer"].as_u64().unwrap();
                paths.push(vec![lid]);
            }
            let mut app = EffectcraftApp::new(session);
            if properties {
                app.ui.timeline.open_layers.extend(leaves);
                app.ui.timeline.open_groups.extend(groups);
            }
            for expanded in [false, true] {
                app.ui.timeline.inline_precomps.clear();
                if expanded {
                    for path in &paths {
                        app.ui.timeline.inline_precomps.insert(instance_key(root, path));
                    }
                }
                let mut samples = Vec::new();
                let mut rows_count = 0;
                for i in 0..repeats + 3 {
                    let t = Instant::now();
                    let rows = build_rows(&app, app.session.project.comp(root).unwrap());
                    let rows = expand(&app, root, rows);
                    rows_count = rows.len();
                    black_box(&rows);
                    drop(rows);
                    let ms = t.elapsed().as_secs_f64() * 1000.0;
                    if i >= 3 {
                        samples.push(ms);
                    }
                }
                samples.sort_by(f64::total_cmp);
                println!(
                    "EC_PROFILE {}",
                    json!({"case":"inline_rows","source_layers":layers,"instances":instances,"properties":properties,
                    "expanded":expanded,"rows":rows_count,"samples":samples.len(),"p50_ms":samples[samples.len()/2],
                    "p95_ms":samples[(samples.len()*95/100).min(samples.len()-1)],"raw_ms":samples})
                );
            }
        }
    }
}
