//! Motion Tile with Output Width/Height over 100% leaves no seam where the tiles meet the layer's
//! original edge. A fractional growth (Output Height 133.44% of 1080 px) used to put the output's
//! pixel grid half a pixel off the source's, and sampling past the layer's last row mixed in the
//! transparency outside it: a line of part-transparent pixels along the original edge (Elias's
//! precomp tiled downwards with Mirror Edges, parented to a null at a fractional position and
//! 111.44% scale: a bright then a dark row in the comp).

use effectcraft_color::Label;
use effectcraft_keyframe::Value;
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{BitDepth, Comp, ItemId, ItemKind, Layer, LayerSource, Project, Solid};
use effectcraft_raster::Image;
use effectcraft_time::{FrameRate, Tick};

use crate::{NoFootage, RenderOpts, Renderer};

const W: f64 = 200.0;
const H: f64 = 100.0;

fn add_effect(p: &mut Project, l: &mut Layer, id: &str, vals: &[(&str, Value)]) {
    let spec = effectcraft_effects::find(id).unwrap();
    let mut next = p.next_id;
    let mut g = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), spec.name, [W, H]);
    p.next_id = next;
    for (k, v) in vals {
        g.prop_mut(k).unwrap().value = v.clone();
    }
    l.props.sub_mut("effects").unwrap().children.push(g.into());
}

/// A 300×300 comp holding a 200×100 precomp of an opaque gradient (dark to light along the axis
/// the tiles grow on), Motion Tile 133.44% along that axis, at `pos` and `scale` percent.
fn project(vertical: bool, mirror: bool, pos: [f64; 2], scale: f64) -> (Project, ItemId) {
    let mut p = Project::default();
    p.settings.bit_depth = BitDepth::Bpc32;
    let inner = Comp::new(W as u32, H as u32, FrameRate::FPS_30, Tick::from_seconds_f64(1.0));
    let iid = p.add_item("Inner", Label::Sandstone, None, ItemKind::Comp(inner.clone().into()));
    let sid = p.add_item("S", Label::Red, None, ItemKind::Solid(Solid { color: [1.0; 3], width: W as u32, height: H as u32, pixel_aspect: 1.0 }));
    let mut s = build::layer(&mut p, &inner, "S", LayerSource::Solid { item: sid }, (W as u32, H as u32), None);
    let (start, end) = if vertical { ([W / 2.0, 0.0], [W / 2.0, H]) } else { ([0.0, H / 2.0], [W, H / 2.0]) };
    let ramp = [
        ("start", Value::Vec2(start)),
        ("end", Value::Vec2(end)),
        ("startColor", Value::Color([0.1, 0.1, 0.1, 1.0])),
        ("endColor", Value::Color([0.3, 0.3, 0.3, 1.0])),
    ];
    add_effect(&mut p, &mut s, "ec.generate.gradientramp", &ramp);
    p.comp_mut(iid).unwrap().layers.push(s);

    let comp = Comp::new(300, 300, FrameRate::FPS_30, Tick::from_seconds_f64(1.0));
    let mut l = build::layer(&mut p, &comp, "Inner", LayerSource::Comp { item: iid }, (W as u32, H as u32), None);
    l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([pos[0], pos[1], 0.0]);
    l.props.prop_mut("transform/scale").unwrap().value = Value::Vec3([scale, scale, 100.0]);
    let (ow, oh) = if vertical { (100.0, 133.44) } else { (133.44, 100.0) };
    let tile = [
        ("tileCenter", Value::Vec2([W / 2.0, H / 2.0])),
        ("outputWidth", Value::Scalar(ow)),
        ("outputHeight", Value::Scalar(oh)),
        ("mirrorEdges", Value::Bool(mirror)),
    ];
    add_effect(&mut p, &mut l, "ec.stylize.motiontile", &tile);
    let cid = p.add_item("C", Label::Sandstone, None, ItemKind::Comp(comp.into()));
    p.comp_mut(cid).unwrap().layers.push(l);
    (p, cid)
}

/// The mean of channel `c` along comp row (`vertical`) or column `i`, over `span`.
fn line_mean(img: &Image, vertical: bool, i: i64, span: (i64, i64), c: usize) -> f32 {
    let n = (span.1 - span.0) as f32;
    (span.0..span.1).map(|j| if vertical { img.get(j, i)[c] } else { img.get(i, j)[c] }).sum::<f32>() / n
}

#[test]
fn motion_tile_leaves_no_seam_at_the_layers_original_edge() {
    // (position, scale %): fractional as in the repro, and whole.
    let placements = [([150.37, 130.71], 111.44), ([150.0, 150.0], 100.0)];
    for vertical in [true, false] {
        for mirror in [true, false] {
            for (pos, scale) in placements {
                let (p, cid) = project(vertical, mirror, pos, scale);
                let img = Renderer::new(&p, &NoFootage, RenderOpts::default()).comp_frame(cid, Tick::ZERO);
                let k = scale / 100.0;
                // Across the layer, clear of its other two edges; along the tiled axis, the
                // original edges.
                let (c_across, half_across, c_along, half_along) =
                    if vertical { (pos[0], W / 2.0 * k, pos[1], H / 2.0 * k) } else { (pos[1], H / 2.0 * k, pos[0], W / 2.0 * k) };
                let span = ((c_across - half_across).ceil() as i64 + 3, (c_across + half_across).floor() as i64 - 3);
                for edge in [c_along - half_along, c_along + half_along] {
                    let lines = (edge.floor() as i64 - 5)..=(edge.ceil() as i64 + 5);
                    let case = format!("vertical {vertical}, mirror {mirror}, position {pos:?}, scale {scale}, edge {edge:.2}");
                    for i in lines.clone() {
                        let a = line_mean(&img, vertical, i, span, 3);
                        assert!(a > 0.999, "{case}: line {i} part transparent (alpha {a})");
                    }
                    if !mirror {
                        // Repeated tiles jump from the light end to the dark one at the edge.
                        continue;
                    }
                    let v: Vec<f32> = lines.map(|i| line_mean(&img, vertical, i, span, 0)).collect();
                    for (n, w) in v.windows(3).enumerate() {
                        let dev = (w[1] - (w[0] + w[2]) * 0.5).abs();
                        assert!(dev < 0.004, "{case}: line {} deviates from its neighbours by {dev} ({w:?})", edge.floor() as i64 - 4 + n as i64);
                    }
                }
            }
        }
    }
}
