//! The bundled Gradient Generator ScriptUI panel (`extensions/scriptui-panels/Gradient
//! Generator.jsx`): every template builds a composition in one undo step, driven by a Gradient
//! Controls layer whose Palette dropdown switches the colours live, and lands in the active comp as
//! its bottom layer when asked.

use std::sync::Arc;

use effectcraft_engine::Session;
use effectcraft_engine::config::MemoryConfig;
use serde_json::{Value, json};

use crate::run_code;

const PANEL: &str = "Gradient Generator.jsx";
const TEMPLATES: [&str; 7] = ["Ribbons", "S-Waves", "Liquid", "Bands", "Swirl", "Soft Mesh", "Sweep"];

fn session() -> Session {
    let mut s = Session { expr: Some(Arc::new(effectcraft_expr::Expressions)), ..Default::default() };
    crate::install(&mut s);
    s.config = Some(Arc::new(MemoryConfig::default()));
    s.load_settings();
    s
}

fn ok(s: &mut Session, code: &str) -> Value {
    let out = run_code(s, code, "test.jsx");
    assert!(out.error.is_none(), "{:?}", out.error);
    out.result
}

fn status(s: &mut Session, win: u64) -> String {
    fn find(w: &Value) -> Option<String> {
        if w["name"] == "status" {
            return w["text"].as_str().map(str::to_string);
        }
        w["children"].as_array()?.iter().find_map(find)
    }
    let w = s.execute("scriptui.get", json!({"window": win})).unwrap();
    find(&w["root"]).unwrap_or_default()
}

/// The comp named `name`'s layers (top to bottom), each with its effects and every expression that
/// fails at 1 s (the expression error, which is empty when it evaluates).
const INSPECT: &str = r#"
    function inspect(name) {
      var c; for (var i = 1; i <= app.project.numItems; i++) if (app.project.item(i).name === name) c = app.project.item(i);
      var out = { size: [c.width, c.height, c.duration], layers: [], errors: [] };
      function walk(g) {
        for (var j = 1; j <= g.numProperties; j++) {
          var p = g.property(j);
          if (p.propertyType === PropertyType.PROPERTY) {
            if (p.expression) { try { p.valueAtTime(1, false); } catch (e) { out.errors.push(p.name + ": " + e.message); } if (p.expressionError) out.errors.push(p.name + ": " + p.expressionError); }
          } else walk(p);
        }
      }
      for (var l = 1; l <= c.numLayers; l++) {
        var fx = c.layer(l).property("ADBE Effect Parade"), names = [];
        for (var e = 1; e <= fx.numProperties; e++) names.push(fx.property(e).name);
        out.layers.push([c.layer(l).name, names]);
        walk(c.layer(l));
      }
      return out;
    }
"#;

#[test]
fn every_template_builds_an_editable_gradient_comp_in_one_undo_step() {
    let mut s = session();
    let win = s.execute("window.scriptPanel", json!({"name": PANEL})).unwrap()["window"].as_u64().unwrap();
    let list = s.execute("file.scripts.list", json!({})).unwrap();
    assert!(list.as_array().unwrap().iter().any(|e| e["name"] == PANEL && e["panel"] == true && e["source"] == "extension"));
    s.execute("scriptui.set", json!({"window": win, "widget": "format", "value": "1:1  1080×1080"})).unwrap();
    s.execute("scriptui.set", json!({"window": win, "widget": "duration", "value": "5 s"})).unwrap();
    for (i, t) in TEMPLATES.iter().enumerate() {
        s.execute("scriptui.set", json!({"window": win, "widget": "template", "value": t})).unwrap();
        let steps = s.history.undo.len();
        s.execute("scriptui.click", json!({"window": win, "widget": "generate"})).unwrap();
        let name = format!("Gradient {t} (Deep Blue)");
        assert_eq!(status(&mut s, win), format!("Made \u{201c}{name}\u{201d}"), "template {i}");
        assert_eq!(s.history.undo.len(), steps + 1, "{t}: one undo step");
        let r = ok(&mut s, &format!("{INSPECT} inspect({name:?})"));
        assert_eq!(r["size"], json!([1080, 1080, 5]), "{t}");
        assert_eq!(r["errors"], json!([]), "{t}: expressions evaluate");
        let layers = r["layers"].as_array().unwrap();
        assert_eq!(layers.len(), 3, "{t}: {layers:?}");
        assert_eq!(layers[0], json!(["Gradient Controls", ["Palette", "Color 1", "Color 2", "Color 3", "Color 4", "Speed", "Blur", "Scale", "Seed", "Angle"]]));
        assert_eq!(layers[1][0], json!(t));
        assert_eq!(layers[2], json!(["Background", ["Fill"]]));
    }
    // Ribbons folds a blurred bar with Wave Warp and Warp.
    let r = ok(&mut s, &format!("{INSPECT} inspect(\"Gradient Ribbons (Deep Blue)\").layers[1][1]"));
    assert_eq!(r, json!(["Gradient Ramp", "Fast Box Blur", "Wave Warp", "Warp"]));
}

#[test]
fn the_palette_dropdown_switches_the_colours_and_custom_keeps_your_own() {
    let mut s = session();
    let win = s.execute("window.scriptPanel", json!({"name": PANEL})).unwrap()["window"].as_u64().unwrap();
    s.execute("scriptui.set", json!({"window": win, "widget": "template", "value": "Liquid"})).unwrap();
    s.execute("scriptui.click", json!({"window": win, "widget": "generate"})).unwrap();
    let r = ok(
        &mut s,
        r##"
        var c; for (var i = 1; i <= app.project.numItems; i++) if (app.project.item(i).name === "Gradient Liquid (Deep Blue)") c = app.project.item(i);
        var ctl = c.layer("Gradient Controls").property("ADBE Effect Parade"), menu = ctl.property("Palette").property(1);
        var col = c.layer("Liquid").property("ADBE Effect Parade").property("Colorama").property("APC Colorama-0002").property("palette");
        function hex(v) { var s = "#"; for (var k = 0; k < 3; k++) { var h = Math.round(v[k] * 255).toString(16); s += h.length < 2 ? "0" + h : h; } return s; }
        var first = hex(ctl.property("Color 2").property(1).valueAtTime(1, false));
        menu.setValue(5);
        var ember = hex(ctl.property("Color 2").property(1).valueAtTime(1, false));
        var cycle = col.valueAtTime(1, false);
        menu.setValue(11);
        ctl.property("Color 2").property(1).setValue([1, 0, 0, 1]);
        [first, ember, cycle.split(" ")[1], hex(ctl.property("Color 2").property(1).valueAtTime(1, false)), menu.isDropdownEffect]
    "##,
    );
    assert_eq!(r, json!(["#236197", "#8b1a00", "0.3:0.5451,0.1020,0.0000", "#ff0000", true]));
}

#[test]
fn it_adds_the_gradient_to_the_active_comp_as_its_background() {
    let mut s = session();
    s.execute("comp.new", json!({"name": "Edit", "width": 1280, "height": 720, "frameRate": 25, "duration": 8})).unwrap();
    s.execute("layer.newSolid", json!({"name": "Title", "color": "#ffffff", "width": 200, "height": 100})).unwrap();
    let win = s.execute("window.scriptPanel", json!({"name": PANEL})).unwrap()["window"].as_u64().unwrap();
    s.execute("scriptui.set", json!({"window": win, "widget": "addToComp", "value": true})).unwrap();
    s.execute("scriptui.click", json!({"window": win, "widget": "generate"})).unwrap();
    assert_eq!(status(&mut s, win), "Added \u{201c}Gradient Ribbons (Deep Blue)\u{201d} to \u{201c}Edit\u{201d}");
    let r = ok(
        &mut s,
        r#"
        var e, g; for (var i = 1; i <= app.project.numItems; i++) { var it = app.project.item(i); if (it.name === "Edit") e = it; if (it.name === "Gradient Ribbons (Deep Blue)") g = it; }
        [e.numLayers, e.layer(1).name, e.layer(2).name, [g.width, g.height, g.duration, g.frameRate]]
    "#,
    );
    assert_eq!(r, json!([2, "Title", "Gradient Ribbons (Deep Blue)", [1280, 720, 8, 25]]));
}
