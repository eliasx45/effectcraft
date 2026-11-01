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

fn contained_node(n: &Node) -> bool {
    match n {
        Node::Prop(p) => !super::live_expression(p) || p.expr.as_ref().is_some_and(|e| self_contained(&e.text)),
        Node::Group(sub) => contained(sub),
    }
}

/// For a layer that reads its own other times, keyed whole (every property, keyframe and
/// marker, its in and out points, and its comp's settings: `Mode::Whole` in `precomp_key`): do
/// the expressions of what it reads there (its source, masks and effects, `EffectHost::self_at`,
/// frame blending, time remapping; not its transform, read at the frame time, Auto-Orient along
/// its path aside, which reads its position) read nothing but their own value, the time, the
/// comp's settings and the layer itself (see [`own_contained`])?
pub(crate) fn inner_contained(layer: &Layer) -> bool {
    if layer.auto_orient == AutoOrient::AlongPath {
        return whole_contained(layer);
    }
    let inner = layer.props.children.iter().filter(|c| !matches!(c, Node::Group(g) if g.match_id == "transform")).map(node_reads).max();
    own_contained(layer, inner.unwrap_or(Reads::Pure))
}

/// Every expression of `layer` reads nothing but its own value, the time, its comp's settings
/// and its own layer (see [`own_contained`]): what a layer whose key hashes it whole may read
/// at any time.
pub(crate) fn whole_contained(layer: &Layer) -> bool {
    own_contained(layer, node_group_reads(&layer.props))
}

/// Whether expressions reading at most `reads` are keyed by the whole layer: an expression that
/// reads its own layer may read any of its properties (at any time, through `valueAtTime`), so
/// every expression of the layer must read nothing outside it, and no property may name a
/// layer (`effect("Layer Control")("Layer")` hands it another layer).
fn own_contained(layer: &Layer, reads: Reads) -> bool {
    match reads {
        Reads::Pure => true,
        Reads::Other => false,
        Reads::Own => node_group_reads(&layer.props) != Reads::Other && !names_a_layer(&layer.props),
    }
}

/// Does a property of `g` name a layer (a Layer Control or layer parameter set to one,
/// keyframed or driven by an expression)? One set to None hands an expression nothing.
fn names_a_layer(g: &PropGroup) -> bool {
    g.children.iter().any(|c| match c {
        Node::Prop(p) => matches!(p.value, Value::Layer(_)) && (matches!(p.value, Value::Layer(Some(_))) || !p.keys.is_empty() || p.has_expression()),
        Node::Group(sub) => names_a_layer(sub),
    })
}

fn node_group_reads(g: &PropGroup) -> Reads {
    g.children.iter().map(node_reads).max().unwrap_or(Reads::Pure)
}

fn node_reads(n: &Node) -> Reads {
    match n {
        Node::Prop(p) if super::live_expression(p) => p.expr.as_ref().map_or(Reads::Pure, |e| reads(&e.text)),
        Node::Prop(_) => Reads::Pure,
        Node::Group(sub) => node_group_reads(sub),
    }
}

/// What an expression may read, as far as its text shows, least first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Reads {
    /// Its property's own value, the time, and its comp's settings (`thisComp.frameDuration`,
    /// `.width`, `.height`, `.duration`, `.pixelAspect`, `.marker`): its value at any time
    /// follows from its property's keyframes and that time.
    Pure,
    /// Also its own layer: its properties at any time (`effect(…)`, `text.animator(…)`,
    /// `valueAtTime`, `key(…)`), markers, in and out points, index, text selector inputs.
    Own,
    /// Possibly another layer, comp, footage item or the project, or something the check doesn't
    /// know (an undeclared name, a computed member, a template literal).
    Other,
}

/// Names an expression may use and still read nothing but its value and the time: the
/// language, `value`, `time` and functions of their arguments alone.
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
    "function",
    "if",
    "else",
    "return",
    "for",
    "while",
    "do",
    "break",
    "continue",
    "switch",
    "case",
    "default",
    "try",
    "catch",
    "finally",
    "throw",
    "typeof",
    "instanceof",
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
    "lookAt",
    "degreesToRadians",
    "radiansToDegrees",
    "rgbToHsl",
    "hslToRgb",
    "hexToRgb",
    "createPath",
    "posterizeTime",
    "framesToTime",
    "timeToFrames",
    "noise",
];

/// Names that read the expression's own layer (see [`Reads::Own`]): the layer and property
/// objects, the layer attributes and property methods the expression language makes global,
/// and the text Expression Selector's inputs. (`random` is seeded by the layer's index and the
/// property.)
const OWN: &[&str] = &[
    "thisLayer",
    "thisProperty",
    "effect",
    "mask",
    "content",
    "text",
    "marker",
    "transform",
    "position",
    "scale",
    "rotation",
    "opacity",
    "anchorPoint",
    "name",
    "index",
    "inPoint",
    "outPoint",
    "startTime",
    "hasParent",
    "enabled",
    "active",
    "numKeys",
    "velocity",
    "speed",
    "key",
    "nearestKey",
    "valueAtTime",
    "velocityAtTime",
    "speedAtTime",
    "wiggle",
    "temporalWiggle",
    "smooth",
    "loopIn",
    "loopOut",
    "loopInDuration",
    "loopOutDuration",
    "textIndex",
    "textTotal",
    "selectorValue",
    "random",
    "gaussRandom",
    "seedRandom",
];

/// `thisComp` members fixed by the comp's settings (which every comp key hashes, the comp's
/// markers included).
const COMP_SETTINGS: &[&str] = &["frameDuration", "width", "height", "duration", "pixelAspect", "marker"];

/// Members that lead out of the layer (its parent, source, comp, other layers, its pixels, its
/// transform to the comp or world, which its parents and the camera move; a layer's `width`
/// and `height`, its source's size) or into the language's machinery (constructors, the
/// expression engine's internals).
const LEAVES: &[&str] = &[
    "parent",
    "source",
    "containingComp",
    "layer",
    "comp",
    "footage",
    "toComp",
    "fromComp",
    "toWorld",
    "fromWorld",
    "toCompVec",
    "fromCompVec",
    "toWorldVec",
    "fromWorldVec",
    "sourceRectAtTime",
    "sampleImage",
    "width",
    "height",
    "hasVideo",
    "constructor",
    "prototype",
];

/// Global names of the expression language's object model that a variable of the same name
/// doesn't hide until it is set (and functions it doesn't replace before its definition runs).
const HOST: &[&str] = &[
    "comp",
    "footage",
    "width",
    "height",
    "hasVideo",
    "Comp",
    "Footage",
    "Layer",
    "Prop",
    "Markers",
    "timeToTimecode",
    "timeToFeetAndFrames",
    "timeToCurrentFormat",
];

/// Words that fail an expression wherever they appear, in strings and comments too: a second
/// check that doesn't depend on reading the text right (a regular expression read as a
/// division could hide what follows it in a "string").
const NEVER: &[&str] = &[
    "thisProject",
    "parent",
    "footage",
    "toComp",
    "fromComp",
    "toWorld",
    "fromWorld",
    "toCompVec",
    "fromCompVec",
    "toWorldVec",
    "fromWorldVec",
    "sampleImage",
    "sourceRectAtTime",
    "containingComp",
    "constructor",
    "prototype",
    "eval",
    "Function",
    "globalThis",
];

/// Does expression `text` read nothing but its property's own value and the time? Then its value
/// at any time follows from its own property (whose keyframes the key hashes) and that time.
/// See [`reads`].
pub(crate) fn self_contained(text: &str) -> bool {
    reads(text) == Reads::Pure
}

/// A token of an expression's text.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Tok<'a> {
    Name(&'a str),
    Num,
    Str,
    /// A regular expression literal.
    Regex,
    Punct(u8),
}

/// The tokens of `text`, each with whether a line break comes before it; `None` for text this
/// reading doesn't follow (template literals, non-ASCII code, unterminated strings, comments or
/// regular expressions).
fn tokens(text: &str) -> Option<Vec<(Tok<'_>, bool)>> {
    let b = text.as_bytes();
    let mut out: Vec<(Tok, bool)> = vec![];
    let mut i = 0;
    let mut nl = false;
    while let Some(&c) = b.get(i) {
        let tok = match c {
            b'\n' | b'\r' => {
                nl = true;
                i += 1;
                continue;
            }
            c if c.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            b'`' => return None,
            b'"' | b'\'' => {
                i += 1;
                loop {
                    match b.get(i) {
                        None | Some(b'\n') => return None,
                        Some(b'\\') => i += 2,
                        Some(&d) => {
                            i += 1;
                            if d == c {
                                break;
                            }
                        }
                    }
                }
                Tok::Str
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while b.get(i).is_some_and(|d| *d != b'\n') {
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                loop {
                    match b.get(i) {
                        None => return None,
                        Some(b'*') if b.get(i + 1) == Some(&b'/') => break,
                        Some(b'\n') => {
                            nl = true;
                            i += 1;
                        }
                        Some(_) => i += 1,
                    }
                }
                i += 2;
                continue;
            }
            // A regular expression where a value starts (after an operator, a keyword, or
            // nothing); a division after a value.
            b'/' if regex_may_start(out.last().map(|t| t.0)) => {
                i += 1;
                let mut class = false;
                loop {
                    match b.get(i) {
                        None | Some(b'\n') => return None,
                        Some(b'\\') => i += 2,
                        Some(b'[') => {
                            class = true;
                            i += 1;
                        }
                        Some(b']') => {
                            class = false;
                            i += 1;
                        }
                        Some(b'/') if !class => {
                            i += 1;
                            break;
                        }
                        Some(_) => i += 1,
                    }
                }
                // Flags.
                while b.get(i).is_some_and(u8::is_ascii_alphabetic) {
                    i += 1;
                }
                Tok::Regex
            }
            b'0'..=b'9' => {
                while b.get(i).is_some_and(|d| d.is_ascii_alphanumeric() || *d == b'.') {
                    i += 1;
                }
                Tok::Num
            }
            b'.' if b.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                i += 1;
                while b.get(i).is_some_and(|d| d.is_ascii_alphanumeric() || *d == b'.') {
                    i += 1;
                }
                Tok::Num
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                let start = i;
                while b.get(i).is_some_and(|d| d.is_ascii_alphanumeric() || *d == b'_' || *d == b'$') {
                    i += 1;
                }
                Tok::Name(text.get(start..i)?)
            }
            c if c.is_ascii() => {
                i += 1;
                Tok::Punct(c)
            }
            // Non-ASCII outside strings and comments: not code this reading knows.
            _ => return None,
        };
        out.push((tok, nl));
        nl = false;
    }
    Some(out)
}

/// May a `/` after `prev` start a regular expression (rather than divide)?
fn regex_may_start(prev: Option<Tok>) -> bool {
    match prev {
        None => true,
        Some(Tok::Punct(c)) => !matches!(c, b')' | b']'),
        Some(Tok::Name(n)) => matches!(n, "return" | "typeof" | "instanceof" | "in" | "of" | "new" | "case" | "do" | "else" | "throw" | "void"),
        Some(Tok::Num | Tok::Str | Tok::Regex) => false,
    }
}

/// Is `word` (as a whole word) in `text` anywhere, strings and comments included?
fn has_word(text: &str, word: &str) -> bool {
    let is_name = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    let b = text.as_bytes();
    text.match_indices(word).any(|(at, _)| {
        let before = at.checked_sub(1).and_then(|j| b.get(j)).is_some_and(|c| is_name(*c));
        let after = b.get(at + word.len()).is_some_and(|c| is_name(*c));
        !before && !after
    })
}

/// What expression `text` may read (see [`Reads`]). Conservative: a name that isn't the
/// language, `value`, `time`, a pure function, an own-layer name ([`OWN`]), `thisComp.` with a
/// settings member or a variable the expression declares (`var`/`let`/`const` lists, function
/// names and parameters, `catch` bindings) reads something else, as does a member that leaves the
/// layer ([`LEAVES`]), a member computed from a string (`x["parent"]`), any subscript of a layer
/// or property object, and any word of [`NEVER`] anywhere in the text. A declared variable
/// named like a layer attribute counts as reading the layer, and one named like another global
/// of the object model ([`HOST`]) as reading anything: it may be read before it's set.
pub(crate) fn reads(text: &str) -> Reads {
    if NEVER.iter().any(|w| has_word(text, w)) || text.contains("__") {
        return Reads::Other;
    }
    let Some(toks) = tokens(text) else { return Reads::Other };
    // Declared names, first (`var` hoists: a declared name is a variable everywhere).
    let mut declared: Vec<&str> = vec![];
    // Bracket depth, and the depth of the `var` list being read.
    let mut depth = 0i32;
    let mut list: Option<i32> = None;
    let mut params: Option<i32> = None;
    for (i, (t, nl)) in toks.iter().enumerate() {
        let prev = i.checked_sub(1).and_then(|j| toks.get(j)).map(|t| t.0);
        if *nl && list == Some(depth) && prev != Some(Tok::Punct(b',')) {
            list = None;
        }
        match t {
            Tok::Name(n) => {
                let opens = matches!(prev, Some(Tok::Name("var" | "let" | "const" | "function")))
                    || (list == Some(depth) && prev == Some(Tok::Punct(b',')))
                    || (params == Some(depth) && matches!(prev, Some(Tok::Punct(b'(' | b','))));
                if opens {
                    declared.push(n);
                }
                if matches!(*n, "var" | "let" | "const") {
                    list = Some(depth);
                }
            }
            Tok::Punct(b'(' | b'[' | b'{') => {
                depth += 1;
                // `function name(…)`, `function (…)`, `catch (e)`: the parameters.
                let name_before = i.checked_sub(2).and_then(|j| toks.get(j)).is_some_and(|t| t.0 == Tok::Name("function"));
                if *t == Tok::Punct(b'(') && (matches!(prev, Some(Tok::Name("function" | "catch"))) || name_before) {
                    params = Some(depth);
                }
            }
            Tok::Punct(b')' | b']' | b'}') => {
                if params == Some(depth) {
                    params = None;
                }
                depth -= 1;
                if list.is_some_and(|d| d > depth) {
                    list = None;
                }
            }
            Tok::Punct(b';') if list == Some(depth) => list = None,
            _ => {}
        }
    }
    let mut most = Reads::Pure;
    let mut i = 0;
    while let Some(&(t, _)) = toks.get(i) {
        let prev = i.checked_sub(1).and_then(|j| toks.get(j)).map(|t| t.0);
        let next = toks.get(i + 1).map(|t| t.0);
        // (Not spread syntax, `...x`.)
        let after_dot = prev == Some(Tok::Punct(b'.')) && i.checked_sub(2).and_then(|j| toks.get(j)).map(|t| t.0) != Some(Tok::Punct(b'.'));
        match t {
            Tok::Name(n) if after_dot => {
                if LEAVES.contains(&n) {
                    return Reads::Other;
                }
            }
            // An object literal's key (`{top: 1, left: 2}`).
            Tok::Name(_) if matches!(prev, Some(Tok::Punct(b'{' | b','))) && next == Some(Tok::Punct(b':')) => {}
            Tok::Name("thisComp") => {
                let member = toks.get(i + 2).map(|t| t.0);
                match (next, member) {
                    (Some(Tok::Punct(b'.')), Some(Tok::Name(m))) if COMP_SETTINGS.contains(&m) => i += 2,
                    _ => return Reads::Other,
                }
            }
            Tok::Name(n) if OWN.contains(&n) => {
                // A layer or property object indexed by a computed name could reach anything.
                if next == Some(Tok::Punct(b'[')) {
                    return Reads::Other;
                }
                most = most.max(Reads::Own);
            }
            Tok::Name(n) if PURE.contains(&n) || (declared.contains(&n) && !HOST.contains(&n)) => {}
            Tok::Name(_) => return Reads::Other,
            // A member named by a string: `x["parent"]`.
            Tok::Punct(b'[') if matches!(prev, Some(Tok::Name(_) | Tok::Punct(b')' | b']'))) && next == Some(Tok::Str) => return Reads::Other,
            _ => {}
        }
        i += 1;
    }
    most
}
