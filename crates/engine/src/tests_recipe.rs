//! `comp.recipe` / `comp.fromRecipe`: recipes are small, rebuild comps that render the same, and
//! the recipe of a rebuilt comp is the original recipe.

use effectcraft_keyframe::{Keyframe, Value as KV};
use effectcraft_project::{ItemId, LayerId};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use crate::Session;

/// A 320×180 comp at 29.97 fps, 2 s (frames that aren't whole ticks of a second).
fn session() -> (Session, u64) {
    let mut s = Session::default();
    let c = s.execute("comp.new", json!({"name": "Main", "width": 320, "height": 180, "frameRate": 29.97, "duration": 2})).unwrap();
    (s, c["comp"].as_u64().unwrap())
}

fn layer(r: &Value) -> u64 {
    r["layer"].as_u64().unwrap_or_else(|| panic!("no layer in {r}"))
}

fn set(s: &mut Session, l: u64, path: &str, value: Value) {
    s.execute("prop.set", json!({"layer": l, "path": path, "value": value})).unwrap_or_else(|e| panic!("{path}: {e}"));
}

/// Keyframes with Easy Ease on both sides at `(seconds, value)`.
fn eased_keys(s: &mut Session, comp: u64, l: u64, path: &str, keys: &[(f64, KV)]) {
    let keys: Vec<Keyframe> = keys.iter().map(|(t, v)| Keyframe::new(Tick::from_seconds_f64(*t), v.clone()).eased()).collect();
    let fr = s.project.comp(ItemId(comp)).unwrap().frame_rate;
    s.edit("keys", None, |proj, _| {
        let p = proj.comp_mut(ItemId(comp)).unwrap().layer_mut(LayerId(l)).unwrap().props.prop_mut(path).unwrap();
        p.keys = keys
            .into_iter()
            .map(|mut k| {
                k.time = fr.snap_nearest(k.time);
                k
            })
            .collect();
        Ok(())
    })
    .unwrap();
}

fn recipe(s: &mut Session, comp: u64) -> Value {
    s.execute("comp.recipe", json!({"comp": comp})).unwrap()
}

/// Rebuild comp `comp` from its recipe and check the round trip: in the same project the
/// rebuilt comp renders like the original at `times` and has the same recipe (but for its
/// name); in a new project the recipe comes back exactly.
fn round_trip(s: &mut Session, comp: u64, times: &[f64]) -> Value {
    let r1 = recipe(s, comp);
    let b = s.execute("comp.fromRecipe", json!({"recipe": r1, "name": "Rebuilt"})).unwrap();
    let rebuilt = b["comp"].as_u64().unwrap();
    for t in times {
        let c = s.execute("render.compare", json!({"a": {"comp": comp, "time": t}, "b": {"comp": rebuilt, "time": t}})).unwrap();
        assert!(c["max_abs"].as_f64().unwrap() <= 2.0 && c["changed_pixels"].as_u64().unwrap() == 0, "frame at {t} s differs: {c}\nrecipe: {r1}");
    }
    if r1.get("precomps").is_none() {
        let mut r2 = recipe(s, rebuilt);
        r2["comp"]["name"] = r1["comp"]["name"].clone();
        for u in r2.get_mut("unsupported").and_then(Value::as_array_mut).into_iter().flatten() {
            u["comp"] = r1["comp"]["name"].clone();
        }
        assert_eq!(r1, r2, "recipe of the rebuilt comp");
    }
    let mut fresh = Session::default();
    let id = fresh.execute("comp.fromRecipe", json!({"recipe": r1})).unwrap()["comp"].as_u64().unwrap();
    assert_eq!(recipe(&mut fresh, id), r1, "recipe rebuilt in a new project");
    // Undo removes the whole comp in one step.
    let n = fresh.project.items.len();
    assert!(n > 0);
    fresh.execute("edit.undo", json!({})).unwrap();
    assert!(fresh.project.comp(ItemId(id)).is_none());
    r1
}

#[test]
fn shape_with_gradient_stroke_style_and_keys() {
    let (mut s, c) = session();
    let l = layer(
        &s.execute(
            "layer.newShape",
            json!({"kind": "ellipse", "name": "Circle", "size": [120, 120], "fill": "#3a7bff", "stroke": "#ffffff", "strokeWidth": 0.7}),
        )
        .unwrap(),
    );
    // A radial gradient fill next to the stroke.
    let gf = s.execute("layer.addShapeItem", json!({"layer": l, "kind": "gfill", "group": "contents/#1"})).unwrap();
    let gpath = gf["path"].as_str().unwrap().to_string();
    set(&mut s, l, &format!("{gpath}/type"), json!(1));
    set(&mut s, l, &format!("{gpath}/end"), json!([60, 0]));
    set(&mut s, l, &format!("{gpath}/colors"), json!({"colors": [[0, [0.62, 0.91, 1.0]], [0.45, [0.23, 0.48, 1.0]], [1, [0.1, 0.12, 0.42]]]}));
    s.execute("layer.style.innerShadow", json!({"layers": [l]})).unwrap();
    set(&mut s, l, "layerStyles/innerShadow/size", json!(12));
    set(&mut s, l, "layerStyles/innerShadow/color", json!([0.03, 0.04, 0.2, 1]));
    set(&mut s, l, "transform/position", json!([110, 90]));
    eased_keys(
        &mut s,
        c,
        l,
        "transform/scale",
        &[(0.0, KV::Vec3([0.0, 0.0, 100.0])), (0.2669, KV::Vec3([115.0, 115.0, 100.0])), (0.5339, KV::Vec3([100.0, 100.0, 100.0]))],
    );
    eased_keys(&mut s, c, l, "transform/opacity", &[(0.0, KV::Scalar(0.0)), (0.1335, KV::Scalar(100.0))]);
    let r = round_trip(&mut s, c, &[0.0, 0.1, 0.3, 1.0]);
    // Compact, readable, and only what was authored.
    let text = r.to_string();
    assert!(text.len() < 2500, "{} bytes: {text}", text.len());
    let props = &r["layers"][0]["props"];
    assert_eq!(props["transform/position"], json!([110, 90]), "{r}");
    assert_eq!(props[&format!("{gpath}/type")], json!("Radial"), "{r}");
    assert!(props.get("transform/anchor").is_none() && props.get("transform/rotation").is_none(), "{r}");
    let scale = &props["transform/scale"]["keys"];
    assert_eq!(scale[1], json!({"time": 0.2669, "value": [115, 115], "in": "easy", "out": "easy"}), "{r}");
    assert_eq!(r["layers"][0]["styles"], json!(["innerShadow"]));
    assert!(r.get("unsupported").is_none(), "{r}");
}

#[test]
fn solid_with_effect_and_mask() {
    let (mut s, c) = session();
    let l = layer(&s.execute("layer.newSolid", json!({"name": "Card", "width": 160, "height": 90, "color": "#ff8020"})).unwrap());
    s.execute("effect.apply", json!({"effect": "Gaussian Blur", "layers": [l]})).unwrap();
    set(&mut s, l, "effects/#1/blurriness", json!(6));
    s.execute("layer.addMask", json!({"layer": l, "shape": "ellipse", "rect": [10, 10, 100, 60]})).unwrap();
    s.execute("layer.mask.mode", json!({"layer": l, "mask": "#1", "mode": "Subtract"})).unwrap();
    set(&mut s, l, "transform/rotation", json!(12));
    let r = round_trip(&mut s, c, &[0.0, 1.0]);
    let lr = &r["layers"][0];
    assert_eq!(lr["type"], "solid");
    assert_eq!(lr["size"], json!([160, 90]));
    assert_eq!(lr["effects"], json!([{"effect": "ec.blur.gaussian"}]), "{r}");
    assert_eq!(lr["masks"][0]["mode"], "Subtract", "{r}");
    assert_eq!(lr["props"]["effects/#1/blurriness"], json!(6), "{r}");
}

#[test]
fn dropdown_with_named_items_round_trips() {
    // 27 Oct (Speaker Card scene): `effects/#1/menu: "Words"` came back as an error, the
    // recipe naming an item the fresh control's default menu (Item 1-3) doesn't have.
    let (mut s, c) = session();
    let l = layer(&s.execute("layer.newSolid", json!({"name": "Ctrl", "width": 16, "height": 16, "color": "#000000"})).unwrap());
    s.execute("effect.apply", json!({"effect": "Dropdown Menu Control", "layers": [l]})).unwrap();
    s.execute("effect.editDropdown", json!({"layer": l, "items": ["Lines", "Words", "Letters"]})).unwrap();
    set(&mut s, l, "effects/#1/menu", json!(1));
    let r = round_trip(&mut s, c, &[0.0]);
    assert_eq!(r["layers"][0]["props"]["effects/#1/menu"], json!({"value": "Words", "items": ["Lines", "Words", "Letters"]}), "{r}");
    // A renamed menu left at its first item still carries its items. (The round trip opened the
    // rebuilt comp, so name the original one; night run 29 Oct.)
    s.execute("prop.set", json!({"comp": c, "layer": l, "path": "effects/#1/menu", "value": 0})).unwrap();
    let r = round_trip(&mut s, c, &[0.0]);
    assert_eq!(r["layers"][0]["props"]["effects/#1/menu"]["items"], json!(["Lines", "Words", "Letters"]), "{r}");
    // "Item N" reads as the Nth item of any menu; a name the menu doesn't have stays an error.
    let ctl = |v: Value| {
        json!({"recipe": {"comp": {"name": "D", "width": 10, "height": 10, "duration": 1},
        "layers": [{"type": "null", "effects": [{"effect": "ec.control.dropdown"}], "props": {"effects/#1/menu": v}}]}})
    };
    for (v, want) in [(json!("Item 2"), 1), (json!({"value": "item 3", "items": ["A", "B", "C"]}), 2), (json!({"value": "b", "items": ["A", "B"]}), 1)] {
        let id = s.execute("comp.fromRecipe", ctl(v.clone())).unwrap_or_else(|e| panic!("{v}: {e}"))["comp"].as_u64().unwrap();
        let comp = s.project.comp(ItemId(id)).unwrap();
        let menu = comp.layers.iter().find_map(|l| l.props.prop("effects/#1/menu")).unwrap();
        assert_eq!(menu.value, KV::Enum(want), "{v}");
    }
    for v in [json!("Words"), json!("Item 4"), json!({"value": "Z", "items": ["A"]}), json!({"value": "A", "items": []}), json!({"value": 0, "items": "A"})] {
        assert!(s.execute("comp.fromRecipe", ctl(v.clone())).is_err(), "{v}");
    }
}

#[test]
fn text_layer_with_animator() {
    let (mut s, c) = session();
    let l = layer(&s.execute("layer.newText", json!({"text": "Hello", "size": 40, "fill": [1, 0.2, 0.2], "position": [160, 100]})).unwrap());
    s.execute("layer.addTextAnimator", json!({"layer": l, "properties": ["opacity"]})).unwrap();
    set(&mut s, l, "text/animators/#1/properties/opacity", json!(0));
    set(&mut s, l, "text/animators/#1/selectors/#1/end", json!(40));
    let r = round_trip(&mut s, c, &[0.0, 1.0]);
    let lr = &r["layers"][0];
    assert_eq!(lr["type"], "text");
    assert_eq!(lr["text"]["text"], "Hello", "{r}");
    assert_eq!(lr["animators"], json!([{"kind": "animator", "props": ["opacity"]}]), "{r}");
}

#[test]
fn parented_null_and_track_matte() {
    let (mut s, c) = session();
    let rig = layer(&s.execute("layer.newNull", json!({"name": "Rig"})).unwrap());
    let card = layer(&s.execute("layer.newSolid", json!({"name": "Card", "width": 80, "height": 80, "color": "#20c060"})).unwrap());
    s.execute("layer.setParent", json!({"layers": [card], "parent": rig})).unwrap();
    eased_keys(&mut s, c, rig, "transform/rotation", &[(0.0, KV::Scalar(0.0)), (1.0, KV::Scalar(90.0))]);
    let matte = layer(&s.execute("layer.newShape", json!({"kind": "rect", "name": "Matte", "size": [100, 60], "fill": "#ffffff"})).unwrap());
    let fill = layer(&s.execute("layer.newSolid", json!({"name": "Fill", "color": "#2040ff"})).unwrap());
    s.execute("layer.setTrackMatte", json!({"layer": fill, "matte": matte, "kind": "alpha"})).unwrap();
    s.execute("prop.setExpression", json!({"layer": matte, "path": "transform/position", "expression": "[160 + time * 40, 90]"})).unwrap();
    let r = round_trip(&mut s, c, &[0.0, 0.5, 1.5]);
    let by_name = |n: &str| r["layers"].as_array().unwrap().iter().find(|l| l["name"] == n).unwrap().clone();
    assert_eq!(by_name("Card")["parent"], "Rig", "{r}");
    assert_eq!(by_name("Fill")["trackMatte"], json!({"layer": "Matte", "kind": "Alpha"}), "{r}");
    assert_eq!(by_name("Matte")["props"]["transform/position"]["expr"], "[160 + time * 40, 90]", "{r}");
    let _ = card;
}

#[test]
fn precomp_layer_carries_its_comp() {
    let (mut s, c) = session();
    let a = layer(&s.execute("layer.newShape", json!({"kind": "star", "name": "Star", "size": [80, 80], "fill": "#ffd020"})).unwrap());
    eased_keys(&mut s, c, a, "transform/rotation", &[(0.0, KV::Scalar(0.0)), (2.0, KV::Scalar(180.0))]);
    s.execute("layer.precompose", json!({"layers": [a], "name": "Inner", "mode": "move", "open": false})).unwrap();
    let pre = s.project.comp(ItemId(c)).unwrap().layers.first().unwrap().id.0;
    set(&mut s, pre, "transform/opacity", json!(60));
    set(&mut s, pre, "transform/position", json!([200, 80]));
    let r = round_trip(&mut s, c, &[0.0, 0.7]);
    assert_eq!(r["layers"][0]["type"], "precomp");
    assert_eq!(r["layers"][0]["comp"], "Inner");
    assert_eq!(r["precomps"][0]["comp"]["name"], "Inner", "{r}");
    assert_eq!(r["precomps"][0]["layers"][0]["name"], "Star", "{r}");
}

#[test]
fn layers_subset_and_file() {
    let (mut s, c) = session();
    let a = layer(&s.execute("layer.newSolid", json!({"name": "A", "color": "#ff0000"})).unwrap());
    let b = layer(&s.execute("layer.newSolid", json!({"name": "B", "color": "#00ff00"})).unwrap());
    s.execute("layer.setParent", json!({"layers": [b], "parent": a})).unwrap();
    let r = s.execute("comp.recipe", json!({"comp": c, "layers": ["B"]})).unwrap();
    assert_eq!(r["layers"].as_array().unwrap().len(), 1);
    // The parent isn't in the recipe: said so, not silently dropped.
    assert!(r["unsupported"][0]["what"].as_str().unwrap().contains("layer `A`"), "{r}");
    let dir = std::env::temp_dir().join(format!("ec-recipe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("r.json").to_string_lossy().to_string();
    let w = s.execute("comp.recipe", json!({"comp": c, "path": path})).unwrap();
    assert_eq!(w["layers"], 2);
    let made = s.execute("comp.fromRecipe", json!({"path": path, "name": "From File"})).unwrap();
    assert_eq!(made["layers"], 2);
    assert_eq!(made["name"], "From File");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn hand_written_recipe_builds() {
    // What an agent writes after reading a recipe: lenient values, defaults left out.
    let mut s = Session::default();
    let r = json!({
        "comp": {"name": "Lower Third", "width": 640, "height": 360, "frameRate": 25, "duration": 3, "background": [0.1, 0.1, 0.1]},
        "layers": [
            {"name": "Title", "type": "text", "text": "Ada Lovelace", "props": {"transform/position": [60, 300]}},
            {"name": "Bar", "type": "shape",
             "contents": [{"kind": "group", "name": "Bar", "contents": [{"kind": "rect"}, {"kind": "fill"}]}],
             "props": {
                "contents/group/contents/rect/size": [400, 8],
                "contents/group/contents/fill/color": "#ffcc00",
                "transform/position": {"keys": [{"time": 0, "value": [-200, 320], "out": "easy"}, {"time": 0.5, "value": [260, 320], "in": "easy"}]}
             }},
            {"name": "Plate", "type": "solid", "color": [0, 0, 0], "size": [640, 80], "props": {"transform/opacity": 50}}
        ]
    });
    let made = s.execute("comp.fromRecipe", json!({"recipe": r})).unwrap();
    assert_eq!(made["layers"], 3);
    let back = recipe(&mut s, made["comp"].as_u64().unwrap());
    assert_eq!(back["layers"][1]["props"]["contents/group/contents/fill/color"], json!([1, 0.8, 0]), "{back}");
    assert_eq!(back["layers"][1]["props"]["transform/position"]["keys"][1]["time"], json!(0.5));
    assert_eq!(back["comp"]["background"], json!([0.1, 0.1, 0.1]));
    // Mistakes are errors that name the layer and the path, and change nothing.
    let before = s.project.items.len();
    let bad =
        json!({"comp": {"name": "X", "width": 100, "height": 100, "duration": 1}, "layers": [{"name": "L", "type": "shape", "props": {"transform/nope": 1}}]});
    let e = s.execute("comp.fromRecipe", json!({"recipe": bad})).unwrap_err().to_string();
    assert!(e.contains("`L`") && e.contains("transform/nope"), "{e}");
    let e = s
        .execute(
            "comp.fromRecipe",
            json!({"recipe": {"comp": {"name": "X", "width": 100, "height": 100, "duration": 1}, "layers": [{"name": "L", "type": "blob"}]}}),
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("unknown type"), "{e}");
    assert_eq!(s.project.items.len(), before);
}

#[test]
fn junk_recipes_are_errors_not_crashes() {
    let mut s = Session::default();
    for junk in [
        json!(null),
        json!(1),
        json!("not json"),
        json!({}),
        json!({"comp": 5}),
        json!({"comp": {"width": -1, "height": 1e300, "duration": "x"}}),
        json!({"comp": {"name": "C", "width": 10, "height": 10, "duration": 1, "frameRate": 0}}),
        json!({"comp": {"name": "C", "width": 10, "height": 10, "duration": 1}, "layers": [1, 2]}),
        json!({"comp": {"name": "C", "width": 10, "height": 10, "duration": 1}, "layers": [{"type": "shape", "contents": [{"kind": "group", "contents": "x"}]}]}),
        json!({"comp": {"name": "C", "width": 10, "height": 10, "duration": 1}, "layers": [{"type": "shape", "contents": [{"raw": {"node": "Group"}}]}]}),
        json!({"comp": {"name": "C", "width": 10, "height": 10, "duration": 1}, "layers": [{"type": "solid", "props": {"transform/scale": {"keys": [{"time": "a"}]}}}]}),
        json!({"comp": {"name": "C", "width": 10, "height": 10, "duration": 1}, "layers": [{"type": "solid", "parent": "#99"}]}),
        json!({"comp": {"name": "C", "width": 10, "height": 10, "duration": 1}, "layers": [{"type": "precomp", "comp": "nope"}]}),
        json!({"comp": {"name": "C", "width": 10, "height": 10, "duration": 1}, "layers": [{"type": "solid", "effects": [{"effect": "No Such Effect"}]}]}),
        json!({"comp": {"name": "C", "width": 10, "height": 10, "duration": 1}, "layers": [{"type": "null", "contents": []}]}),
        json!({"comp": {"name": "C", "width": 10, "height": 10, "duration": 1}, "layers": [{"type": "solid", "props": {"transform/position": {"keys": [{"time": 0, "value": [1, 2], "in": "wobbly"}]}}}]}),
        json!({"format": "something-else"}),
    ] {
        let r = s.execute("comp.fromRecipe", json!({"recipe": junk}));
        assert!(r.is_err(), "{junk} → {r:?}");
    }
    let mut deep = json!(1);
    for _ in 0..400 {
        deep = json!([deep]);
    }
    assert!(s.execute("comp.fromRecipe", json!({"recipe": {"comp": deep}})).is_err());
    assert!(s.execute("comp.fromRecipe", json!({})).is_err());
    assert!(s.execute("comp.recipe", json!({"comp": 12345})).is_err());
}

#[test]
fn what_the_factories_cant_build_is_kept_raw() {
    let (mut s, c) = session();
    let l = layer(
        &s.execute("layer.newShape", json!({"kind": "rect", "name": "Dashed", "size": [200, 100], "stroke": "#ffffff", "strokeWidth": 6, "fillType": "none"}))
            .unwrap(),
    );
    set(&mut s, l, "contents/#1/contents/stroke/dashes/dash", json!(20));
    // The Dashes "+" button adds a second Dash/Gap pair the stroke factory doesn't make.
    s.edit("dash", None, |proj, _| {
        let mut ids = effectcraft_project::build::Ids(&mut proj.next_id);
        let (d, g) = (ids.prop("dash2", "Dash 2", KV::Scalar(5.0)), ids.prop("gap2", "Gap 2", KV::Scalar(9.0)));
        let dashes = proj.comp_mut(ItemId(c)).unwrap().layer_mut(LayerId(l)).unwrap().props.group_mut("contents/#1/contents/stroke/dashes").unwrap();
        dashes.children.push(d.into());
        dashes.children.push(g.into());
        Ok(())
    })
    .unwrap();
    let r = round_trip(&mut s, c, &[0.0]);
    let items = &r["layers"][0]["contents"][0]["contents"];
    let stroke = items.as_array().unwrap().iter().find(|i| i["kind"] == "stroke").unwrap();
    assert!(stroke.get("raw").is_some(), "{r}");
    // The rest is still described item by item.
    assert!(items.as_array().unwrap().iter().any(|i| i == &json!({"kind": "rect"})), "{r}");
}

#[test]
fn three_d_camera_light_and_include_defaults() {
    let (mut s, c) = session();
    let card = layer(&s.execute("layer.newSolid", json!({"name": "Card", "width": 120, "height": 80, "color": "#e0e0e0"})).unwrap());
    s.edit("3d", None, |proj, _| {
        proj.comp_mut(ItemId(c)).unwrap().layer_mut(LayerId(card)).unwrap().switches.three_d = true;
        Ok(())
    })
    .unwrap();
    set(&mut s, card, "transform/rotationY", json!(35));
    s.execute("layer.newLight", json!({"kind": "Spot"})).unwrap();
    s.execute("layer.newCamera", json!({"name": "Cam"})).unwrap();
    let r = round_trip(&mut s, c, &[0.0]);
    let types: Vec<&str> = r["layers"].as_array().unwrap().iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["camera", "light", "solid"], "{r}");
    assert_eq!(r["layers"][1]["light"], "Spot");
    assert_eq!(r["layers"][2]["switches"], json!({"three_d": true}));
    // includeDefaults lists every property.
    let all = s.execute("comp.recipe", json!({"comp": c, "includeDefaults": true})).unwrap();
    let n = |r: &Value| r["layers"][2]["props"].as_object().map_or(0, |p| p.len());
    assert!(n(&all) > n(&r) + 5, "{} vs {}", n(&all), n(&r));
    assert_eq!(all["layers"][2]["props"]["transform/opacity"], json!(100));
}

#[test]
fn footage_layer_keeps_its_file_and_says_when_it_is_missing() {
    let (mut s, c) = session();
    s.edit("footage", None, |proj, st| {
        let f = effectcraft_project::Footage {
            path: "/no/such/dir/clip.mov".into(),
            kind: effectcraft_project::FootageKind::Video,
            width: 320,
            height: 180,
            frame_rate: effectcraft_time::FrameRate::FPS_30,
            duration: Tick::from_seconds_f64(2.0),
            has_video: true,
            ..Default::default()
        };
        let item = proj.add_item("clip.mov", effectcraft_color::Label::Aqua, None, effectcraft_project::ItemKind::Footage(f));
        let comp = proj.comp(ItemId(c)).unwrap().clone();
        let l = effectcraft_project::build::layer(proj, &comp, "clip.mov", effectcraft_project::LayerSource::Footage { item }, (320, 180), None);
        crate::commands::layer::insert_layer(proj, st, ItemId(c), l)?;
        Ok(())
    })
    .unwrap();
    let l = s.project.comp(ItemId(c)).unwrap().layers.first().unwrap().id.0;
    set(&mut s, l, "transform/scale", json!([50, 50, 100]));
    let r = round_trip(&mut s, c, &[0.0]);
    let lr = &r["layers"][0];
    assert_eq!(lr["type"], "footage");
    assert_eq!(lr["file"], "/no/such/dir/clip.mov");
    assert_eq!(lr["props"]["transform/scale"], json!([50, 50]), "{r}");
    assert!(r["unsupported"][0]["why"].as_str().unwrap().contains("offline"), "{r}");
}

#[test]
fn round_trip_harness_sees_differences() {
    // The render comparison the round trips rely on notices a change.
    let (mut s, c) = session();
    let l = layer(&s.execute("layer.newSolid", json!({"name": "A", "width": 40, "height": 40, "color": "#ff0000"})).unwrap());
    let r = recipe(&mut s, c);
    let b = s.execute("comp.fromRecipe", json!({"recipe": r})).unwrap()["comp"].as_u64().unwrap();
    s.execute("prop.set", json!({"comp": c, "layer": l, "path": "transform/position", "value": [30, 30]})).unwrap();
    let cmp = s.execute("render.compare", json!({"a": {"comp": c, "time": 0}, "b": {"comp": b, "time": 0}})).unwrap();
    assert!(cmp["changed_pixels"].as_u64().unwrap() > 0, "{cmp}");
}
