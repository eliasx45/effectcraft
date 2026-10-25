//! Keys by evaluated values: the hashed structure of a layer without its keyframes.
//!
//! A key that hashes every animated property's *value at the frame time* doesn't need the
//! keyframes themselves: two layers whose structure matches once the keyframes are left out,
//! and whose animated values match at the frame time, draw the same pixels there, whatever
//! their keyframes do elsewhere. So editing a keyframe changes the keys only of the frames
//! whose values it changes (between its neighbouring keyframes), not of every frame.
//!
//! That holds only while the layer is drawn from its values at the frame time alone. A layer
//! that reads other times ([`pointwise`] is false: time effects, simulations, paint spans,
//! Auto-Orient along its path, frame blending, time remapping, forced mask motion blur; or it
//! is motion blurred, which the keys ask where they know the render) keeps its keyframes in
//! the key, as before.

use std::hash::Hash;

use effectcraft_keyframe::Value;
use effectcraft_project::{AutoOrient, FrameBlend, GroupKind, Layer, LayerSource, MaskMotionBlur, Node, PropGroup};
use effectcraft_time::Tick;

use super::{KeyHasher, hash_debug};

/// Does a property of `g` have keyframes?
pub(super) fn has_keys(g: &PropGroup) -> bool {
    g.children.iter().any(|c| match c {
        Node::Prop(p) => !p.keys.is_empty(),
        Node::Group(sub) => has_keys(sub),
    })
}

/// Leave out every keyframed property's keyframes and its static value (unused while it has
/// keyframes): the key hashes the value at the frame time instead.
fn strip(g: &mut PropGroup) {
    for c in &mut g.children {
        strip_node(c);
    }
}

fn strip_node(n: &mut Node) {
    match n {
        Node::Prop(p) if !p.keys.is_empty() => {
            p.keys = Vec::new();
            p.value = Value::Scalar(0.0);
        }
        Node::Prop(_) => {}
        Node::Group(g) => strip(g),
    }
}

/// Hash `n` as [`hash_debug`] does, without its keyframes (see [`strip`]).
pub(super) fn hash_node(h: &mut KeyHasher, n: &Node) {
    let keyed = match n {
        Node::Prop(p) => !p.keys.is_empty(),
        Node::Group(g) => has_keys(g),
    };
    if keyed {
        let mut n = n.clone();
        strip_node(&mut n);
        hash_debug(h, &n);
    } else {
        hash_debug(h, n);
    }
}

/// Hash every field of `layer`, every property included, without its keyframes.
pub(super) fn hash_layer(h: &mut KeyHasher, layer: &Layer) {
    if has_keys(&layer.props) {
        let mut l = layer.clone();
        strip(&mut l.props);
        hash_debug(h, &l);
    } else {
        hash_debug(h, layer);
    }
}

/// Is `layer` drawn from its property values at the frame time alone (see the module docs)?
/// `time_dependent`, `reads_other_times` and `paint` are its [`super::LayerInfo`] flags.
pub(super) fn pointwise(layer: &Layer, time_dependent: bool, reads_other_times: bool, paint: bool) -> bool {
    let forced_mask_blur = layer
        .masks()
        .is_some_and(|m| m.children.iter().any(|n| matches!(n, Node::Group(g) if matches!(g.kind, GroupKind::Mask { motion_blur: MaskMotionBlur::On, .. }))));
    !(time_dependent
        || reads_other_times
        || paint
        || forced_mask_blur
        || layer.auto_orient == AutoOrient::AlongPath
        || layer.switches.frame_blend != FrameBlend::Off
        // (A constant Time Remap, like a literal `0.5` or a single keyframe — what imported
        // stills and icons carry — shows one nested time.)
        || layer.props.get("timeRemap").is_some_and(|p| p.keys.len() > 1 || super::live_expression(p)))
}

/// Hash, for every keyframed property of `g`, the keyframes its values between layer times
/// `lt0` and `lt1` depend on: before the first keyframe the first, after the last the last
/// (held); else from the last key at or before `lt0` to the first at or after `lt1`, and two
/// more on each side (a segment's auto-Bézier tangents and eases read its keys' neighbours). For
/// a layer drawn at several times around the frame's (motion blur's shutter), with
/// [`super::LayerInfo::free`]: a keyframe edit outside the window keeps the key.
pub(crate) fn hash_window(h: &mut KeyHasher, g: &PropGroup, lt0: Tick, lt1: Tick) {
    for c in &g.children {
        match c {
            Node::Prop(p) => {
                let (Some(first), Some(last)) = (p.keys.first(), p.keys.last()) else { continue };
                p.uid.hash(h);
                let end = p.keys.len() - 1;
                let (a, b) = if lt1 <= first.time {
                    (0, 0)
                } else if lt0 >= last.time {
                    (end, end)
                } else {
                    let lo = p.keys.partition_point(|k| k.time <= lt0).saturating_sub(1);
                    let hi = p.keys.partition_point(|k| k.time < lt1).min(end);
                    (lo.saturating_sub(2), hi.saturating_add(2).min(end))
                };
                (a, b, end).hash(h);
                if let Some(keys) = p.keys.get(a..=b) {
                    hash_debug(h, &keys);
                }
            }
            Node::Group(sub) => hash_window(h, sub, lt0, lt1),
        }
    }
}

/// Does every live expression of `g` read nothing but its own property's value and the time
/// (see [`self_contained`])? Then its values at other times follow from its keyframes.
pub(crate) fn contained(g: &PropGroup) -> bool {
    g.children.iter().all(contained_node)
}

/// [`contained`] for the properties motion blur reads at its shutter's times: the transform but
/// Opacity (each sub-sample's matrix; opacity applies once) and the masks; for shape and text
/// layers every other property too (their content is drawn at each sub-sample, and whether it
/// moves is asked of the layer's content key there). Other layers' effects and Layer Styles are
/// evaluated at the frame time (an effect reading its own parameters at other times reads other
/// times: not `pointwise`).
pub(crate) fn sampled_contained(layer: &Layer) -> bool {
    let content = matches!(layer.source, LayerSource::Shape | LayerSource::Text);
    layer.props.children.iter().all(|c| match c {
        Node::Group(g) if g.match_id == "transform" => g.children.iter().all(|c| matches!(c, Node::Prop(p) if p.match_id == "opacity") || contained_node(c)),
        Node::Group(g) if g.match_id == "masks" => contained(g),
        Node::Group(_) if !content => true,
        _ => contained_node(c),
    })
}

/// [`contained`] for what a layer that reads its own other times reads there: its source, masks
/// and effects (`EffectHost::self_at`, frame blending, time remapping), not its transform, which
/// is read at the frame time (Auto-Orient along its path aside, which reads its position).
pub(crate) fn inner_contained(layer: &Layer) -> bool {
    if layer.auto_orient == AutoOrient::AlongPath {
        return contained(&layer.props);
    }
    layer.props.children.iter().all(|c| matches!(c, Node::Group(g) if g.match_id == "transform") || contained_node(c))
}

fn contained_node(n: &Node) -> bool {
    match n {
        Node::Prop(p) => !super::live_expression(p) || p.expr.as_ref().is_some_and(|e| self_contained(&e.text)),
        Node::Group(sub) => contained(sub),
    }
}

/// Names an expression may use and still be self-contained: the language, `value`, `time` and
/// functions of their arguments alone.
const PURE: &[&str] = &[
    "value",
    "time",
    "Math",
    "Number",
    "String",
    "Boolean",
    "Array",
    "parseInt",
    "parseFloat",
    "isNaN",
    "isFinite",
    "true",
    "false",
    "null",
    "undefined",
    "NaN",
    "Infinity",
    "var",
    "let",
    "const",
    "if",
    "else",
    "return",
    "for",
    "while",
    "do",
    "break",
    "continue",
    "typeof",
    "new",
    "of",
    "in",
    "linear",
    "ease",
    "easeIn",
    "easeOut",
    "clamp",
    "add",
    "sub",
    "mul",
    "div",
    "length",
    "normalize",
    "dot",
    "cross",
    "degreesToRadians",
    "radiansToDegrees",
    "posterizeTime",
    "framesToTime",
    "timeToFrames",
    "noise",
];

/// Does expression `text` read nothing but its property's own value and the time? Then its value
/// at any time follows from its own property (whose keyframes the key hashes) and that time.
/// Conservative: any name that isn't the language, `value`, `time`, a pure function or a variable
/// it declares (`var`/`let`/`const`/`function`) fails, as do names after `.` that aren't method
/// calls on those (`Math.floor`, `x.toString`), checked by being after a dot at all.
pub(crate) fn self_contained(text: &str) -> bool {
    let b = text.as_bytes();
    let mut names: Vec<&str> = vec![];
    let mut declared: Vec<&str> = vec![];
    let mut i = 0;
    let mut after_dot = false;
    let mut declare_next = false;
    while let Some(&c) = b.get(i) {
        match c {
            b'"' | b'\'' | b'`' => {
                // A string: skip to its end (escapes included). Template literals may hold code.
                if c == b'`' {
                    return false;
                }
                i += 1;
                while let Some(&d) = b.get(i) {
                    if d == b'\\' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    if d == c {
                        break;
                    }
                }
                after_dot = false;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while b.get(i).is_some_and(|d| *d != b'\n') {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b.get(i) == Some(&b'*') && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
            }
            b'0'..=b'9' => {
                while b.get(i).is_some_and(|d| d.is_ascii_alphanumeric() || *d == b'.') {
                    i += 1;
                }
                after_dot = false;
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                let start = i;
                while b.get(i).is_some_and(|d| d.is_ascii_alphanumeric() || *d == b'_' || *d == b'$') {
                    i += 1;
                }
                let Some(name) = text.get(start..i) else { return false };
                if declare_next {
                    declared.push(name);
                } else if !after_dot {
                    names.push(name);
                }
                declare_next = matches!(name, "var" | "let" | "const" | "function");
                after_dot = false;
            }
            b'.' => {
                after_dot = true;
                i += 1;
            }
            c if c.is_ascii_whitespace() => i += 1,
            c if c.is_ascii() => {
                after_dot = false;
                i += 1;
            }
            // Non-ASCII outside strings: not a name this check knows.
            _ => return false,
        }
    }
    names.iter().all(|n| PURE.contains(n) || declared.contains(n))
}
