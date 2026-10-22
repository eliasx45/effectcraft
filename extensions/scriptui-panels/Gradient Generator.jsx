// Gradient Generator: animated gradient backgrounds in one click.
//
// A ScriptUI panel that ships with EffectCraft as an optional extension (Window ▸ Gradient
// Generator.jsx). Pick a template, a palette, a format and a duration, then Generate: it builds a
// composition from built-in effects only (Gradient Ramp, 4-Color Gradient, Fractal Noise, Fast Box
// Blur, Wave Warp, Warp, Turbulent Displace, Twirl, Colorama) driven by expressions from one
// "Gradient Controls" layer, so everything stays editable in Effect Controls:
//   Palette   dropdown of the built-in palettes; "Custom" uses Color 1–4 as you set them
//   Color 1–4 background, two main colours, highlight
//   Speed, Blur (%), Scale (%), Angle, Seed (another variation of the same look)
// It uses only After Effects' documented scripting API (items.addComp, layers.addSolid/addNull,
// Effects.addProperty, Property.expression, setPropertyParameters, app.settings and ScriptUI).
// Original work by the EffectCraft contributors, MIT OR Apache-2.0.

(function gradientGenerator(thisObj) {
  var SECTION = "Gradient Generator";
  var CTRL = "Gradient Controls";
  var C = 'thisComp.layer("' + CTRL + '")';

  // Our own palettes: background, main A, main B, highlight.
  var PALETTES = [
    { name: "Deep Blue", colors: ["#0c1423", "#236197", "#4697f3", "#c5e4ff"] },
    { name: "Electric", colors: ["#05060f", "#1b2cff", "#8a2be2", "#ff4fd8"] },
    { name: "Soft Light", colors: ["#f1eff3", "#3d5afe", "#b04df0", "#ff5fd2"] },
    { name: "Sunset", colors: ["#140606", "#c2185b", "#ff6f3c", "#ffd27f"] },
    { name: "Ember", colors: ["#0d0200", "#8b1a00", "#ff5a00", "#ffd36b"] },
    { name: "Mint", colors: ["#04140f", "#0f766e", "#2dd4bf", "#d1fae5"] },
    { name: "Aurora", colors: ["#020a12", "#0b6e4f", "#3ddc97", "#7b5cff"] },
    { name: "Ocean", colors: ["#01080f", "#034078", "#1282a2", "#b8e3ff"] },
    { name: "Gold", colors: ["#0b0803", "#6b4a12", "#c99a2e", "#f7e3a3"] },
    { name: "Mono", colors: ["#0a0a0a", "#3a3a3a", "#9a9a9a", "#f2f2f2"] }
  ];

  var TEMPLATES = [
    { name: "Ribbons", build: ribbons, about: "Blurred gradient bar folded by Wave Warp and Warp" },
    { name: "S-Waves", build: sWaves, about: "Tall soft S-curves" },
    { name: "Liquid", build: liquid, about: "Fractal noise through Colorama" },
    { name: "Bands", build: bands, about: "Wavy colour bands through Colorama" },
    { name: "Swirl", build: swirl, about: "Four colours twirled around the centre" },
    { name: "Soft Mesh", build: mesh, about: "Four drifting colour points (simple)" },
    { name: "Sweep", build: sweep, about: "A slowly turning linear gradient (simple)" }
  ];

  var FORMATS = [
    { name: "Active comp size", w: 0, h: 0 },
    { name: "16:9  1920×1080", w: 1920, h: 1080 },
    { name: "9:16  1080×1920", w: 1080, h: 1920 },
    { name: "1:1  1080×1080", w: 1080, h: 1080 },
    { name: "4:5  1080×1350", w: 1080, h: 1350 },
    { name: "4K  3840×2160", w: 3840, h: 2160 }
  ];
  var DURATIONS = [
    { name: "Active comp length", s: 0 },
    { name: "5 s", s: 5 },
    { name: "10 s", s: 10 },
    { name: "15 s", s: 15 },
    { name: "30 s", s: 30 }
  ];

  // ------------------------------------------------------------------ helpers

  function rgb(hex) {
    var n = parseInt(hex.replace("#", ""), 16);
    return [((n >> 16) & 255) / 255, ((n >> 8) & 255) / 255, (n & 255) / 255];
  }

  // A property by a path of names or match ids ("positionsColors/point1").
  function prop(group, path) {
    var parts = path.split("/"), p = group;
    for (var i = 0; i < parts.length; i++) {
      p = p.property(parts[i]);
      if (!p) throw new Error("no property " + path);
    }
    return p;
  }

  function fx(layer, matchName, name) {
    var e = layer.property("ADBE Effect Parade").addProperty(matchName);
    if (name) e.name = name;
    return e;
  }

  function set(effect, path, value) {
    prop(effect, path).setValue(value);
  }

  function expr(effect, path, code) {
    prop(effect, path).expression = code;
  }

  // Expression text reading a control of the Gradient Controls layer.
  function ctl(name) {
    var kind = /^Color/.test(name) ? "Color" : name === "Angle" ? "Angle" : name === "Palette" ? "Menu" : "Slider";
    return C + '.effect("' + name + '")("' + kind + '")';
  }

  // Colorama's Output Cycle as a palette text from Color 1–4 (wraps back towards Color 1).
  var PALETTE_EXPR =
    "function f(c){return c[0].toFixed(4)+','+c[1].toFixed(4)+','+c[2].toFixed(4);}\n" +
    "var a=" + ctl("Color 1") + ",b=" + ctl("Color 2") + ",c=" + ctl("Color 3") + ",d=" + ctl("Color 4") + ";\n" +
    "'0:'+f(a)+' 0.3:'+f(b)+' 0.55:'+f(c)+' 0.75:'+f(d)+' 0.9:'+f(b)";

  function colorama(layer) {
    var e = fx(layer, "APC Colorama");
    set(e, "APC Colorama-0002/usePresetPalette", 9); // Custom
    expr(e, "APC Colorama-0002/palette", PALETTE_EXPR);
    return e;
  }

  function fullSolid(comp, name, margin) {
    var w = Math.round(comp.width * margin), h = Math.round(comp.height * margin);
    return comp.layers.addSolid([0, 0, 0], name, w, h, 1);
  }

  // ------------------------------------------------------------------ controls

  function controls(comp, palette) {
    var n = comp.layers.addNull(comp.duration);
    n.name = CTRL;
    n.enabled = false;
    var menu = fx(n, "ADBE Dropdown Control", "Palette");
    var names = [];
    for (var i = 0; i < PALETTES.length; i++) names.push(PALETTES[i].name);
    names.push("Custom");
    try {
      menu.property(1).setPropertyParameters(names);
    } catch (e) {}
    menu.property(1).setValue(palette + 1);
    var table = [];
    for (var p = 0; p < PALETTES.length; p++) {
      var cs = [];
      for (var k = 0; k < 4; k++) cs.push("[" + rgb(PALETTES[p].colors[k]).join(",") + ",1]");
      table.push("[" + cs.join(",") + "]");
    }
    for (var c = 0; c < 4; c++) {
      var e = fx(n, "ADBE Color Control", "Color " + (c + 1));
      var v = rgb(PALETTES[palette].colors[c]);
      e.property(1).setValue([v[0], v[1], v[2], 1]);
      // A palette from the menu, or the colour as set ("Custom").
      e.property(1).expression = "var P=[" + table.join(",") + "];\nvar m=" + ctl("Palette") + ".value;\nm>=1&&m<=P.length?P[m-1][" + c + "]:value";
    }
    var sliders = [["Speed", 1], ["Blur", 100], ["Scale", 100], ["Seed", 0]];
    for (var s = 0; s < sliders.length; s++) fx(n, "ADBE Slider Control", sliders[s][0]).property(1).setValue(sliders[s][1]);
    fx(n, "ADBE Angle Control", "Angle");
    return n;
  }

  function background(comp) {
    var bg = comp.layers.addSolid([0, 0, 0], "Background", comp.width, comp.height, 1);
    var f = fx(bg, "ADBE Fill");
    expr(f, "color", ctl("Color 1"));
    return bg;
  }

  // ------------------------------------------------------------------ templates
  // `k` scales pixel sizes from a 1920-wide frame to this one.

  // A long gradient bar, blurred first and then folded, so the soft edges become silky ribbons.
  function ribbonBar(comp, name, k, o) {
    var d = Math.max(comp.width, comp.height);
    var bar = comp.layers.addSolid([1, 1, 1], name, Math.round(d * o.length), Math.round(d * o.thickness), 1);
    var r = fx(bar, "ADBE Ramp");
    set(r, "start", [0, bar.height / 2]);
    set(r, "end", [bar.width, bar.height / 2]);
    expr(r, "startColor", ctl("Color 2"));
    expr(r, "endColor", ctl(o.endColor));
    // Not Repeat Edge Pixels: the bar's edges must fade out.
    var b = fx(bar, "ADBE Box Blur2");
    expr(b, "radius", o.blur * k + "*" + ctl("Blur") + "/100");
    set(b, "iterations", 3);
    var w = fx(bar, "ADBE Wave Warp");
    set(w, "waveType", o.waveType);
    set(w, "height", o.waveHeight * k);
    set(w, "width", o.waveWidth * k);
    set(w, "direction", 90);
    expr(w, "speed", o.speed + "*" + ctl("Speed"));
    expr(w, "phase", ctl("Seed") + "*37");
    var wp = fx(bar, "ADBE WRPMESH");
    set(wp, "warpStyle", o.warpStyle);
    set(wp, "warpAxis", 2);
    set(wp, "bend", o.bend);
    set(wp, "horizontalDistortion", 29);
    var t = bar.property("ADBE Transform Group");
    expr(t, "ADBE Scale", "var s=" + ctl("Scale") + "/100;[" + o.scale[0] + "*s," + o.scale[1] + "*s]");
    // Portrait frames turn the bar upright so the ribbons run along the long side.
    var turn = comp.height > comp.width ? 90 : 0;
    expr(t, "ADBE Rotate Z", o.rotation + turn + "+" + ctl("Angle") + "+time*" + ctl("Speed") + "*" + o.drift);
    return bar;
  }

  function ribbons(comp, k) {
    ribbonBar(comp, "Ribbons", k, {
      length: 0.8, thickness: 0.18, endColor: "Color 3", blur: 130, waveType: 7, waveHeight: 300, waveWidth: 300,
      speed: 0.3, warpStyle: 9, bend: 25, scale: [67, 96], rotation: 0, drift: 3
    });
  }

  function sWaves(comp, k) {
    ribbonBar(comp, "S-Waves", k, {
      length: 0.8, thickness: 0.2, endColor: "Color 4", blur: 150, waveType: 7, waveHeight: 380, waveWidth: 1100,
      speed: 0.25, warpStyle: 9, bend: 45, scale: [80, 110], rotation: -8, drift: 2
    });
  }

  function liquid(comp, k) {
    var l = fullSolid(comp, "Liquid", 1.2);
    var n = fx(l, "ADBE Fractal Noise");
    set(n, "contrast", 260);
    expr(n, "transform/scale", 220 * k + "*" + ctl("Scale") + "/100");
    expr(n, "evolution", "time*" + ctl("Speed") + "*40+" + ctl("Seed") + "*97");
    var t = fx(l, "ADBE Turbulent Displace");
    set(t, "amount", 120);
    set(t, "size", 250 * k);
    expr(t, "evolution", "time*" + ctl("Speed") + "*30+" + ctl("Seed") + "*61");
    var b = fx(l, "ADBE Box Blur2");
    expr(b, "radius", 25 * k + "*" + ctl("Blur") + "/100");
    set(b, "iterations", 3);
    set(b, "repeatEdge", true);
    colorama(l);
    expr(l.property("ADBE Transform Group"), "ADBE Rotate Z", ctl("Angle"));
  }

  function bands(comp, k) {
    var l = fullSolid(comp, "Bands", 1.3);
    var r = fx(l, "ADBE Ramp");
    set(r, "start", [l.width / 2, 0]);
    set(r, "end", [l.width / 2, l.height]);
    set(r, "startColor", [0, 0, 0, 1]);
    set(r, "endColor", [1, 1, 1, 1]);
    var w = fx(l, "ADBE Wave Warp");
    set(w, "height", 170 * k);
    expr(w, "width", 700 * k + "*" + ctl("Scale") + "/100");
    set(w, "direction", 90);
    expr(w, "speed", "0.4*" + ctl("Speed"));
    expr(w, "phase", ctl("Seed") + "*37");
    var t = fx(l, "ADBE Turbulent Displace");
    set(t, "amount", 70);
    set(t, "size", 420 * k);
    expr(t, "evolution", "time*" + ctl("Speed") + "*25+" + ctl("Seed") + "*61");
    var b = fx(l, "ADBE Box Blur2");
    expr(b, "radius", 30 * k + "*" + ctl("Blur") + "/100");
    set(b, "iterations", 3);
    set(b, "repeatEdge", true);
    var c = colorama(l);
    set(c, "APC Colorama-0002/cycles", 3);
    expr(l.property("ADBE Transform Group"), "ADBE Rotate Z", ctl("Angle"));
  }

  function fourColor(l, order) {
    var g = fx(l, "ADBE 4ColorGradient");
    for (var i = 0; i < 4; i++) expr(g, "positionsColors/color" + (i + 1), ctl("Color " + order[i]));
    return g;
  }

  function swirl(comp, k) {
    var l = fullSolid(comp, "Swirl", 1.0);
    var g = fourColor(l, [2, 1, 3, 4]);
    set(g, "blend", 140);
    var tw = fx(l, "ADBE Twirl");
    expr(tw, "angle", "360+Math.sin(time*" + ctl("Speed") + "*0.5+" + ctl("Seed") + ")*160+" + ctl("Angle"));
    expr(tw, "radius", "55*" + ctl("Scale") + "/100");
    var b = fx(l, "ADBE Box Blur2");
    expr(b, "radius", 20 * k + "*" + ctl("Blur") + "/100");
    set(b, "iterations", 3);
    set(b, "repeatEdge", true);
  }

  function mesh(comp, k) {
    var l = fullSolid(comp, "Soft Mesh", 1.0);
    var g = fourColor(l, [2, 1, 3, 4]);
    set(g, "blend", 160);
    var base = [[0.2, 0.2], [0.8, 0.2], [0.2, 0.8], [0.8, 0.8]];
    for (var i = 0; i < 4; i++) {
      expr(
        g,
        "positionsColors/point" + (i + 1),
        "var s=" + ctl("Speed") + ",e=" + ctl("Seed") + "+" + i * 1.7 + ",r=" + ctl("Scale") + "/100;\n" +
          "[thisLayer.width*(" + base[i][0] + "+0.22*r*Math.sin(time*s*0.45+e)), thisLayer.height*(" + base[i][1] + "+0.22*r*Math.cos(time*s*0.37+e*1.3))]"
      );
    }
    var b = fx(l, "ADBE Box Blur2");
    expr(b, "radius", 10 * k + "*" + ctl("Blur") + "/100");
    set(b, "iterations", 3);
    set(b, "repeatEdge", true);
  }

  function sweep(comp, k) {
    var l = fullSolid(comp, "Sweep", 1.0);
    var r = fx(l, "ADBE Ramp");
    var dir = "var a=(" + ctl("Angle") + "+time*" + ctl("Speed") + "*12+" + ctl("Seed") + "*29)*Math.PI/180,R=Math.max(thisLayer.width,thisLayer.height)*0.5*" + ctl("Scale") + "/100;\n";
    expr(r, "start", dir + "[thisLayer.width/2-Math.cos(a)*R, thisLayer.height/2-Math.sin(a)*R]");
    expr(r, "end", dir + "[thisLayer.width/2+Math.cos(a)*R, thisLayer.height/2+Math.sin(a)*R]");
    expr(r, "startColor", ctl("Color 2"));
    expr(r, "endColor", ctl("Color 3"));
    set(r, "scatter", 10);
  }

  // ------------------------------------------------------------------ generate

  function uniqueName(base) {
    var name = base, n = 2, taken = {};
    for (var i = 1; i <= app.project.numItems; i++) taken[app.project.item(i).name] = true;
    while (taken[name]) name = base + " " + n++;
    return name;
  }

  // Builds the composition; adds it to `into` (a comp) as its bottom layer when given.
  function generate(o) {
    var t = TEMPLATES[o.template], into = o.into;
    var w = FORMATS[o.format].w || (into ? into.width : 1920);
    var h = FORMATS[o.format].h || (into ? into.height : 1080);
    var dur = DURATIONS[o.duration].s || (into ? into.duration : 10);
    var fps = into ? into.frameRate : 30;
    var comp = app.project.items.addComp(uniqueName("Gradient " + t.name + " (" + PALETTES[o.palette].name + ")"), w, h, 1, dur, fps);
    var k = Math.max(w, h) / 1920;
    // Bottom to top: the background, the gradient, the controls.
    background(comp);
    t.build(comp, k);
    controls(comp, o.palette);
    if (into) {
      var layer = into.layers.add(comp);
      layer.moveToEnd();
    }
    return comp;
  }

  // ------------------------------------------------------------------ panel

  function load(key, count, fallback) {
    if (!app.settings.haveSetting(SECTION, key)) return fallback;
    var v = parseInt(app.settings.getSetting(SECTION, key), 10);
    return v >= 0 && v < count ? v : fallback;
  }

  var ui = thisObj instanceof Panel ? thisObj : new Window("palette", "Gradient Generator", undefined, { resizeable: true });
  ui.orientation = "column";
  ui.alignChildren = ["fill", "top"];
  ui.spacing = 6;
  ui.margins = 10;

  function row(label, items, key, fallback) {
    var g = ui.add("group");
    g.alignChildren = ["fill", "center"];
    var l = g.add("statictext", undefined, label);
    l.preferredSize.width = 64;
    var names = [];
    for (var i = 0; i < items.length; i++) names.push(items[i].name);
    var d = g.add("dropdownlist", undefined, names, { name: key });
    d.alignment = ["fill", "center"];
    d.selection = load(key, items.length, fallback);
    d.onChange = function () {
      if (d.selection) app.settings.saveSetting(SECTION, key, String(d.selection.index));
      if (key === "template") about.text = TEMPLATES[d.selection.index].about;
    };
    return d;
  }

  var template = row("Template", TEMPLATES, "template", 0);
  var about = ui.add("statictext", undefined, "", { name: "about" });
  about.alignment = ["fill", "top"];
  var palette = row("Palette", PALETTES, "palette", 0);
  var format = row("Format", FORMATS, "format", 0);
  var duration = row("Duration", DURATIONS, "duration", 0);
  var addTo = ui.add("checkbox", undefined, "Add to the active comp as its background", { name: "addToComp" });
  addTo.value = !app.settings.haveSetting(SECTION, "addToComp") || app.settings.getSetting(SECTION, "addToComp") === "1";
  addTo.onClick = function () {
    app.settings.saveSetting(SECTION, "addToComp", addTo.value ? "1" : "0");
  };
  var go = ui.add("button", undefined, "Generate", { name: "generate" });
  var status = ui.add("statictext", undefined, "", { name: "status" });
  status.alignment = ["fill", "top"];
  about.text = TEMPLATES[template.selection.index].about;
  status.text = "Pick a look, then Generate";

  go.onClick = function () {
    var active = app.project.activeItem instanceof CompItem ? app.project.activeItem : null;
    // A gradient made here (open after Generate) is never the comp to add the next one to.
    if (active && active.layer(CTRL)) active = null;
    app.beginUndoGroup("Generate Gradient");
    try {
      var comp = generate({
        template: template.selection.index,
        palette: palette.selection.index,
        format: format.selection.index,
        duration: duration.selection.index,
        into: addTo.value ? active : null
      });
      status.text = addTo.value && active ? "Added “" + comp.name + "” to “" + active.name + "”" : "Made “" + comp.name + "”";
      if (!(addTo.value && active)) comp.openInViewer();
    } catch (e) {
      status.text = "Could not generate: " + e.message;
    }
    app.endUndoGroup();
  };

  if (ui instanceof Window) {
    ui.show();
  } else {
    ui.layout.layout(true);
  }
})(this);
