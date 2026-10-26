//! The property tree: groups and properties with stable uids and match ids, addressed by paths.

use effectcraft_keyframe::{Keyframe, Value};
use effectcraft_time::Tick;
use serde::{Deserialize, Serialize};

pub type Uid = u64;

/// How a property is presented and edited.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ParamUi {
    /// Plain number with an optional valid range and a (narrower) slider range.
    #[default]
    Number,
    Slider {
        min: f64,
        max: f64,
        slider_min: f64,
        slider_max: f64,
        decimals: u8,
    },
    Percent,
    /// Angle in degrees, shown as `revolutions x +degrees`.
    Angle,
    Pixels,
    Point,
    Point3,
    Color,
    Checkbox,
    Popup {
        options: Vec<String>,
    },
    Layer,
    /// A mask of the effect's own layer by 1-based index, 0 = none (a scalar value): an effect's
    /// "Path" popup.
    Mask,
    Path,
    Text,
    /// A file path (a string value) chosen with a "Choose…" button, offering files with these
    /// extensions: Apply Color LUT's LUT, an OCIO config.
    File {
        filters: Vec<String>,
    },
    Gradient,
    /// Not shown in Effect Controls / the timeline (internal data).
    Hidden,
}

/// Expression attached to a property.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Expression {
    pub text: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Property {
    pub uid: Uid,
    /// Stable id within its parent kind (`position`, `blurriness`…).
    #[serde(rename = "match")]
    pub match_id: String,
    pub name: String,
    /// Static value (used when there are no keyframes).
    pub value: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<Keyframe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr: Option<Expression>,
    // Fields at their default value are left out of project files (they read back as it).
    #[serde(default, skip_serializing_if = "is_number_ui")]
    pub ui: ParamUi,
    /// Motion-path semantics (Position, Anchor Point, points).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub spatial: bool,
    /// Only hold keyframes (Source Text, checkboxes, popups).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hold_only: bool,
    /// Valid only on 3D layers (Orientation, X/Y Rotation…).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub three_d_only: bool,
    /// Valid only on 2D layers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub two_d_only: bool,
    /// Dimensions shown/edited (2 for 2D position etc.); 0 = all.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub shown_dims: u8,
    /// Can't be keyframed (header-like values, e.g. Mask Mode). Shown without a stopwatch.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub static_only: bool,
    /// Included in the saved Graph Editor Set, independently of selection.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub graph_editor: bool,
}

fn is_number_ui(ui: &ParamUi) -> bool {
    matches!(ui, ParamUi::Number)
}

fn is_zero_u8(v: &u8) -> bool {
    *v == 0
}

impl Property {
    pub fn new(uid: Uid, match_id: &str, name: &str, value: Value) -> Property {
        let hold_only = !value.interpolates();
        Property {
            uid,
            match_id: match_id.into(),
            name: name.into(),
            value,
            keys: vec![],
            expr: None,
            ui: ParamUi::Number,
            spatial: false,
            hold_only,
            three_d_only: false,
            two_d_only: false,
            shown_dims: 0,
            static_only: false,
            graph_editor: false,
        }
    }
    pub fn with_ui(mut self, ui: ParamUi) -> Property {
        self.ui = ui;
        self
    }
    pub fn spatial(mut self) -> Property {
        self.spatial = true;
        self
    }
    pub fn is_animated(&self) -> bool {
        !self.keys.is_empty()
    }
    pub fn has_expression(&self) -> bool {
        self.expr.as_ref().is_some_and(|e| e.enabled && !e.text.trim().is_empty())
    }
    /// Keyframed value at `t` (no expression).
    pub fn value_at(&self, t: Tick) -> Value {
        if self.keys.is_empty() {
            return self.value.clone();
        }
        effectcraft_keyframe::evaluate(&self.keys, t, self.spatial).unwrap_or_else(|| self.value.clone())
    }
    /// Set the value at `t`: adds/replaces a key when animated, else sets the static value.
    pub fn set_value_at(&mut self, t: Tick, v: Value) {
        if self.keys.is_empty() {
            self.value = v;
        } else {
            let mut k = Keyframe::new(t, v);
            if self.hold_only {
                k = k.hold();
            }
            effectcraft_keyframe::insert_key_preserving_ease(&mut self.keys, k, self.spatial);
        }
    }
    /// Toggle the stopwatch: on → one key at `t` with the current value; off → keep value at `t`.
    pub fn set_animated(&mut self, on: bool, t: Tick) {
        if on && self.keys.is_empty() {
            let mut k = Keyframe::new(t, self.value.clone());
            if self.hold_only {
                k = k.hold();
            }
            self.keys.push(k);
        } else if !on && !self.keys.is_empty() {
            self.value = self.value_at(t);
            self.keys.clear();
        }
    }
}

/// What a group represents, with its non-animatable header data.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum GroupKind {
    #[default]
    Plain,
    /// An instance in an indexed list (masks, effects, shape contents, animators): these can be
    /// renamed, reordered, duplicated and deleted.
    Indexed,
    Mask {
        mode: MaskMode,
        inverted: bool,
        color: [u8; 3],
        locked: bool,
        /// Layer ▸ Mask ▸ Motion Blur.
        #[serde(default)]
        motion_blur: MaskMotionBlur,
        /// Layer ▸ Mask ▸ Feather Falloff.
        #[serde(default)]
        feather_falloff: FeatherFalloff,
        /// Layer ▸ Mask and Shape Path ▸ RotoBezier: tangents follow the vertices automatically.
        #[serde(default)]
        roto_bezier: bool,
    },
    Effect {
        /// Effect spec id, e.g. `ec.blur.gaussian`.
        effect: String,
    },
    /// A motion tracker (Motion Trackers ▸ Tracker n) with its Tracker panel settings.
    Tracker { settings: Box<crate::tracking::TrackerSettings> },
    /// The Essential Properties of a precomp layer (master properties): one child per control
    /// of the nested comp's Essential Graphics; `overridden` lists the children (uids) whose
    /// values this instance overrides.
    Essential {
        #[serde(default)]
        overridden: Vec<Uid>,
    },
}

/// Mask motion blur (sub-frame samples of an animated mask path).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MaskMotionBlur {
    /// Blur when the layer's Motion Blur switch is on.
    #[default]
    SameAsLayer,
    On,
    Off,
}

/// How a mask's feather ramps: Smooth (Gaussian) or Linear.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FeatherFalloff {
    #[default]
    Smooth,
    Linear,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MaskMode {
    None,
    #[default]
    Add,
    Subtract,
    Intersect,
    Lighten,
    Darken,
    Difference,
}

impl MaskMode {
    pub const ALL: [MaskMode; 7] =
        [MaskMode::None, MaskMode::Add, MaskMode::Subtract, MaskMode::Intersect, MaskMode::Lighten, MaskMode::Darken, MaskMode::Difference];
    pub fn label(self) -> &'static str {
        match self {
            MaskMode::None => "None",
            MaskMode::Add => "Add",
            MaskMode::Subtract => "Subtract",
            MaskMode::Intersect => "Intersect",
            MaskMode::Lighten => "Lighten",
            MaskMode::Darken => "Darken",
            MaskMode::Difference => "Difference",
        }
    }
    pub fn from_name(s: &str) -> Option<MaskMode> {
        MaskMode::ALL.into_iter().find(|m| m.label().eq_ignore_ascii_case(s))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PropGroup {
    pub uid: Uid,
    #[serde(rename = "match")]
    pub match_id: String,
    pub name: String,
    #[serde(default)]
    pub kind: GroupKind,
    /// fx switch for effects, eye for shape groups, etc.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// An effect's colour label (After Effects 26.5), shown in Effect Controls and the Timeline;
    /// `None` is unlabelled. Absent in older projects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<effectcraft_color::Label>,
    #[serde(default)]
    pub children: Vec<Node>,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "node")]
pub enum Node {
    Prop(Property),
    Group(PropGroup),
}

// Hand-written so the common case streams: serde's derive for an internally tagged enum buffers
// every node (and so, nested, nearly the whole file) into its private `Content` tree before
// converting it. Serialization always writes `"node"` first, so read it and hand the rest of the
// map straight to the variant; only a tag found later falls back to buffering (as JSON values).
impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Node, D::Error> {
        d.deserialize_any(NodeVisitor)
    }
}

const NODE_VARIANTS: &[&str] = &["Prop", "Group"];

#[derive(Clone, Copy)]
enum NodeTag {
    Prop,
    Group,
}

impl<'de> Deserialize<'de> for NodeTag {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<NodeTag, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = NodeTag;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("variant identifier")
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<NodeTag, E> {
                match s {
                    "Prop" => Ok(NodeTag::Prop),
                    "Group" => Ok(NodeTag::Group),
                    _ => Err(E::unknown_variant(s, NODE_VARIANTS)),
                }
            }
            fn visit_u64<E: serde::de::Error>(self, i: u64) -> Result<NodeTag, E> {
                match i {
                    0 => Ok(NodeTag::Prop),
                    1 => Ok(NodeTag::Group),
                    _ => Err(E::invalid_value(serde::de::Unexpected::Unsigned(i), &"variant index 0 <= i < 2")),
                }
            }
        }
        d.deserialize_identifier(V)
    }
}

/// A map key: the tag, or any other field (kept for the buffering fallback).
enum NodeKey {
    Tag,
    Other(String),
}

impl<'de> Deserialize<'de> for NodeKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<NodeKey, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = NodeKey;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a field name")
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<NodeKey, E> {
                Ok(if s == "node" { NodeKey::Tag } else { NodeKey::Other(s.to_owned()) })
            }
        }
        d.deserialize_identifier(V)
    }
}

struct NodeVisitor;

impl<'de> serde::de::Visitor<'de> for NodeVisitor {
    type Value = Node;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("internally tagged enum Node")
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Node, A::Error> {
        use serde::de::Error;
        use serde::de::value::MapAccessDeserializer;
        let Some(first) = map.next_key::<NodeKey>()? else { return Err(A::Error::missing_field("node")) };
        let mut rest = serde_json::Map::new();
        let tag = match first {
            // Fast path: stream the remaining fields into the variant.
            NodeKey::Tag => {
                return match map.next_value::<NodeTag>()? {
                    NodeTag::Prop => Property::deserialize(MapAccessDeserializer::new(map)).map(Node::Prop),
                    NodeTag::Group => PropGroup::deserialize(MapAccessDeserializer::new(map)).map(Node::Group),
                };
            }
            NodeKey::Other(k) => {
                rest.insert(k, map.next_value()?);
                let mut tag = None;
                while let Some(k) = map.next_key::<NodeKey>()? {
                    match k {
                        NodeKey::Tag if tag.is_some() => return Err(A::Error::duplicate_field("node")),
                        NodeKey::Tag => tag = Some(map.next_value::<NodeTag>()?),
                        NodeKey::Other(k) => {
                            rest.insert(k, map.next_value()?);
                        }
                    }
                }
                tag.ok_or_else(|| A::Error::missing_field("node"))?
            }
        };
        let rest = serde_json::Value::Object(rest);
        match tag {
            NodeTag::Prop => Property::deserialize(rest).map(Node::Prop),
            NodeTag::Group => PropGroup::deserialize(rest).map(Node::Group),
        }
        .map_err(A::Error::custom)
    }
}

impl Node {
    pub fn uid(&self) -> Uid {
        match self {
            Node::Prop(p) => p.uid,
            Node::Group(g) => g.uid,
        }
    }
    pub fn match_id(&self) -> &str {
        match self {
            Node::Prop(p) => &p.match_id,
            Node::Group(g) => &g.match_id,
        }
    }
    pub fn name(&self) -> &str {
        match self {
            Node::Prop(p) => &p.name,
            Node::Group(g) => &g.name,
        }
    }
    pub fn as_prop(&self) -> Option<&Property> {
        if let Node::Prop(p) = self { Some(p) } else { None }
    }
    pub fn as_group(&self) -> Option<&PropGroup> {
        if let Node::Group(g) = self { Some(g) } else { None }
    }
    pub fn as_group_mut(&mut self) -> Option<&mut PropGroup> {
        if let Node::Group(g) = self { Some(g) } else { None }
    }
}

/// How far a path got ([`PropGroup::resolve_path`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathWalk {
    /// The segments that named a child, as written (dotted match ids kept whole).
    pub segs: Vec<String>,
    /// The child index each of `segs` named, level by level.
    pub indices: Vec<usize>,
    /// What was left when a segment named nothing: the failing segment first (the rest of its
    /// `/`-chunk, dots kept), then the later `/` segments.
    pub rest: Vec<String>,
}

/// One step of a property path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seg {
    Uid(Uid),
    /// Match id and 1-based occurrence among siblings with that match id.
    Match(String, usize),
    /// 1-based child index.
    Index(usize),
}

/// Parse `transform/position`, `effects/#1/blurriness`, `effects/@57/blurriness`,
/// `contents/rect#2/size`.
/// `@57` (a whole path that is one uid).
fn bare_uid(path: &str) -> Option<Uid> {
    path.trim().strip_prefix('@')?.parse().ok()
}

pub fn parse_path(s: &str) -> Vec<Seg> {
    s.split(['/', '.']).filter(|x| !x.is_empty()).map(parse_seg).collect()
}

/// One path segment: `@uid`, `#index`, `match#n` or `match`.
fn parse_seg(seg: &str) -> Seg {
    if let Some(u) = seg.strip_prefix('@').and_then(|u| u.parse().ok()) {
        Seg::Uid(u)
    } else if let Some(i) = seg.strip_prefix('#').and_then(|u| u.parse().ok()) {
        Seg::Index(i)
    } else if let Some((m, n)) = seg.rsplit_once('#')
        && let Ok(n) = n.parse()
    {
        Seg::Match(m.to_string(), n)
    } else {
        Seg::Match(seg.to_string(), 1)
    }
}

/// Most `.`-separated pieces one segment may join into a dotted match id
/// (`ec.stylize.deepglow` is 3): bounds the work a hostile path can cause.
const MAX_DOTTED_PIECES: usize = 16;

impl PropGroup {
    /// An instance in an indexed list (effect, mask, shape item, text animator, tracker): these
    /// can be renamed, removed, duplicated and reordered; fixed groups (Transform…) can't.
    pub fn is_instance(&self) -> bool {
        matches!(self.kind, GroupKind::Indexed | GroupKind::Effect { .. } | GroupKind::Mask { .. } | GroupKind::Tracker { .. })
    }

    pub fn new(uid: Uid, match_id: &str, name: &str) -> PropGroup {
        PropGroup { uid, match_id: match_id.into(), name: name.into(), kind: GroupKind::Plain, enabled: true, label: None, children: vec![] }
    }
    pub fn with(mut self, n: impl Into<Node>) -> PropGroup {
        self.children.push(n.into());
        self
    }

    fn child_index(&self, seg: &Seg) -> Option<usize> {
        match seg {
            Seg::Uid(u) => self.children.iter().position(|c| c.uid() == *u),
            Seg::Index(i) => (*i >= 1 && *i <= self.children.len()).then(|| i - 1),
            Seg::Match(m, n) => {
                self.children.iter().enumerate().filter(|(_, c)| c.match_id() == m || c.name().eq_ignore_ascii_case(m)).nth(n.saturating_sub(1)).map(|(i, _)| i)
            }
        }
    }

    /// Walk `path` from this group: the segments that resolved (each as written, a dotted match
    /// id such as `ec.stylize.deepglow` kept whole), the child index chain they name, and the
    /// pieces left over when a segment named nothing (empty when the whole path resolved).
    ///
    /// `/` always separates segments; `.` separates them too (`transform.position`) unless the
    /// dotted text names a child itself: match ids of effects and their params may contain dots,
    /// and the longest dotted run that names a child wins.
    pub fn resolve_path(&self, path: &str) -> PathWalk {
        let mut walk = PathWalk::default();
        let mut g = Some(self);
        let chunks: Vec<&str> = path.split('/').filter(|x| !x.is_empty()).collect();
        for (c, chunk) in chunks.iter().enumerate() {
            let pieces: Vec<&str> = chunk.split('.').filter(|x| !x.is_empty()).collect();
            let mut j = 0;
            while j < pieces.len() {
                let found = g.and_then(|grp| {
                    let max = pieces.len().min(j.saturating_add(MAX_DOTTED_PIECES));
                    (j + 1..=max).rev().find_map(|k| {
                        let text = pieces.get(j..k)?.join(".");
                        grp.child_index(&parse_seg(&text)).map(|i| (k, i, text))
                    })
                });
                let Some((k, i, text)) = found else {
                    walk.rest.push(pieces.get(j..).unwrap_or_default().join("."));
                    walk.rest.extend(chunks.iter().skip(c + 1).map(|x| x.to_string()));
                    return walk;
                };
                walk.segs.push(text);
                walk.indices.push(i);
                g = g.and_then(|grp| grp.children.get(i)).and_then(Node::as_group);
                j = k;
            }
        }
        walk
    }

    pub fn node(&self, path: &str) -> Option<&Node> {
        let walk = self.resolve_path(path);
        if !walk.rest.is_empty() {
            return None;
        }
        let (last, init) = walk.indices.split_last()?;
        let mut g = self;
        for i in init {
            g = g.children.get(*i)?.as_group()?;
        }
        g.children.get(*last)
    }
    pub fn node_mut(&mut self, path: &str) -> Option<&mut Node> {
        let walk = self.resolve_path(path);
        if !walk.rest.is_empty() {
            return None;
        }
        let (last, init) = walk.indices.split_last()?;
        let mut g = self;
        for i in init {
            g = g.children.get_mut(*i)?.as_group_mut()?;
        }
        g.children.get_mut(*last)
    }
    /// A property by path; a bare `@uid` finds the property anywhere below this group.
    pub fn prop(&self, path: &str) -> Option<&Property> {
        if let Some(u) = bare_uid(path) {
            return self.find(u);
        }
        self.node(path)?.as_prop()
    }
    pub fn prop_mut(&mut self, path: &str) -> Option<&mut Property> {
        if let Some(u) = bare_uid(path) {
            return self.find_mut(u);
        }
        match self.node_mut(path)? {
            Node::Prop(p) => Some(p),
            _ => None,
        }
    }
    pub fn group(&self, path: &str) -> Option<&PropGroup> {
        self.node(path)?.as_group()
    }
    pub fn group_mut(&mut self, path: &str) -> Option<&mut PropGroup> {
        self.node_mut(path)?.as_group_mut()
    }
    /// Direct child property by match id.
    pub fn get(&self, match_id: &str) -> Option<&Property> {
        self.children.iter().find_map(|c| c.as_prop().filter(|p| p.match_id == match_id))
    }
    pub fn get_mut(&mut self, match_id: &str) -> Option<&mut Property> {
        self.children.iter_mut().find_map(|c| match c {
            Node::Prop(p) if p.match_id == match_id => Some(p),
            _ => None,
        })
    }
    pub fn sub(&self, match_id: &str) -> Option<&PropGroup> {
        self.children.iter().find_map(|c| c.as_group().filter(|g| g.match_id == match_id))
    }
    pub fn sub_mut(&mut self, match_id: &str) -> Option<&mut PropGroup> {
        self.children.iter_mut().find_map(|c| match c {
            Node::Group(g) if g.match_id == match_id => Some(g),
            _ => None,
        })
    }
    pub fn groups(&self) -> impl Iterator<Item = &PropGroup> {
        self.children.iter().filter_map(Node::as_group)
    }
    pub fn props(&self) -> impl Iterator<Item = &Property> {
        self.children.iter().filter_map(Node::as_prop)
    }

    /// Visit every property depth-first with its uid-path (`@g/@g/@p`).
    pub fn walk<'a>(&'a self, prefix: &str, f: &mut dyn FnMut(&str, &'a Property)) {
        for c in &self.children {
            let p = format!("{prefix}@{}", c.uid());
            match c {
                Node::Prop(pr) => f(&p, pr),
                Node::Group(g) => g.walk(&format!("{p}/"), f),
            }
        }
    }
    pub fn walk_mut(&mut self, f: &mut dyn FnMut(&mut Property)) {
        for c in &mut self.children {
            match c {
                Node::Prop(p) => f(p),
                Node::Group(g) => g.walk_mut(f),
            }
        }
    }
    /// Uid path (`@1/@2/@3`) of the node with `uid`.
    pub fn path_of(&self, uid: Uid) -> Option<String> {
        for c in &self.children {
            if c.uid() == uid {
                return Some(format!("@{uid}"));
            }
            if let Node::Group(g) = c
                && let Some(p) = g.path_of(uid)
            {
                return Some(format!("@{}/{p}", g.uid));
            }
        }
        None
    }
    /// Match-id path (`transform/position`, `effects/ec.blur.gaussian#2/blurriness`) of the node
    /// with `uid`: portable between layers with the same structure (keyframe paste, pick-whip).
    pub fn match_path_of(&self, uid: Uid) -> Option<String> {
        for (i, c) in self.children.iter().enumerate() {
            let n = self.children[..i].iter().filter(|o| o.match_id() == c.match_id()).count() + 1;
            let seg = if n == 1 { c.match_id().to_string() } else { format!("{}#{n}", c.match_id()) };
            if c.uid() == uid {
                return Some(seg);
            }
            if let Node::Group(g) = c
                && let Some(p) = g.match_path_of(uid)
            {
                return Some(format!("{seg}/{p}"));
            }
        }
        None
    }
    /// Chain of nodes from this group's children down to `uid` (inclusive).
    pub fn node_chain(&self, uid: Uid) -> Option<Vec<&Node>> {
        for c in &self.children {
            if c.uid() == uid {
                return Some(vec![c]);
            }
            if let Node::Group(g) = c
                && let Some(mut rest) = g.node_chain(uid)
            {
                rest.insert(0, c);
                return Some(rest);
            }
        }
        None
    }
    /// Human path (`Transform/Position`) of the node with `uid`.
    pub fn name_path_of(&self, uid: Uid) -> Option<String> {
        for c in &self.children {
            if c.uid() == uid {
                return Some(c.name().to_string());
            }
            if let Node::Group(g) = c
                && let Some(p) = g.name_path_of(uid)
            {
                return Some(format!("{}/{p}", g.name));
            }
        }
        None
    }
    /// Find a property anywhere in the tree by uid.
    pub fn find(&self, uid: Uid) -> Option<&Property> {
        for c in &self.children {
            match c {
                Node::Prop(p) if p.uid == uid => return Some(p),
                Node::Group(g) => {
                    if let Some(p) = g.find(uid) {
                        return Some(p);
                    }
                }
                _ => {}
            }
        }
        None
    }
    pub fn find_mut(&mut self, uid: Uid) -> Option<&mut Property> {
        for c in &mut self.children {
            match c {
                Node::Prop(p) if p.uid == uid => return Some(p),
                Node::Group(g) => {
                    if let Some(p) = g.find_mut(uid) {
                        return Some(p);
                    }
                }
                _ => {}
            }
        }
        None
    }
    pub fn find_group(&self, uid: Uid) -> Option<&PropGroup> {
        if self.uid == uid {
            return Some(self);
        }
        self.groups().find_map(|g| g.find_group(uid))
    }
    pub fn find_group_mut(&mut self, uid: Uid) -> Option<&mut PropGroup> {
        if self.uid == uid {
            return Some(self);
        }
        self.children.iter_mut().find_map(|c| match c {
            Node::Group(g) => g.find_group_mut(uid),
            _ => None,
        })
    }
    /// Parent group of the node with `uid`.
    pub fn parent_of(&self, uid: Uid) -> Option<&PropGroup> {
        if self.children.iter().any(|c| c.uid() == uid) {
            return Some(self);
        }
        self.groups().find_map(|g| g.parent_of(uid))
    }
    /// Parent group of the node with `uid`.
    pub fn parent_of_mut(&mut self, uid: Uid) -> Option<&mut PropGroup> {
        if self.children.iter().any(|c| c.uid() == uid) {
            return Some(self);
        }
        self.children.iter_mut().find_map(|c| match c {
            Node::Group(g) => g.parent_of_mut(uid),
            _ => None,
        })
    }
    /// Any property in this subtree animated (keys or expression).
    pub fn any_animated(&self) -> bool {
        self.children.iter().any(|c| match c {
            Node::Prop(p) => p.is_animated() || p.has_expression(),
            Node::Group(g) => g.any_animated(),
        })
    }
    /// Re-assign fresh uids (after duplicating a subtree).
    pub fn reassign_uids(&mut self, next: &mut Uid) {
        *next += 1;
        self.uid = *next;
        for c in &mut self.children {
            match c {
                Node::Prop(p) => {
                    *next += 1;
                    p.uid = *next;
                }
                Node::Group(g) => g.reassign_uids(next),
            }
        }
    }
    pub fn max_uid(&self) -> Uid {
        self.children
            .iter()
            .map(|c| match c {
                Node::Prop(p) => p.uid,
                Node::Group(g) => g.max_uid(),
            })
            .fold(self.uid, Uid::max)
    }
}

impl From<Property> for Node {
    fn from(p: Property) -> Node {
        Node::Prop(p)
    }
}
impl From<PropGroup> for Node {
    fn from(g: PropGroup) -> Node {
        Node::Group(g)
    }
}

#[cfg(test)]
mod node_serde_tests {
    use super::*;
    use effectcraft_keyframe::ShapePath;

    /// `Node` as serde derives it (the reference the hand-written `Deserialize` must match).
    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "node")]
    enum RefNode {
        Prop(Property),
        Group(RefGroup),
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct RefGroup {
        uid: Uid,
        #[serde(rename = "match")]
        match_id: String,
        name: String,
        #[serde(default)]
        kind: GroupKind,
        #[serde(default = "yes")]
        enabled: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<effectcraft_color::Label>,
        #[serde(default)]
        children: Vec<RefNode>,
    }

    impl From<RefNode> for Node {
        fn from(n: RefNode) -> Node {
            match n {
                RefNode::Prop(p) => Node::Prop(p),
                RefNode::Group(g) => Node::Group(PropGroup {
                    uid: g.uid,
                    match_id: g.match_id,
                    name: g.name,
                    kind: g.kind,
                    enabled: g.enabled,
                    label: g.label,
                    children: g.children.into_iter().map(Node::from).collect(),
                }),
            }
        }
    }

    fn tree() -> Node {
        let mut pos = Property::new(2, "position", "Position", Value::Vec2([1.0, 2.5])).spatial();
        pos.set_animated(true, Tick::ZERO);
        pos.set_value_at(Tick::from_seconds_f64(1.0), Value::Vec2([3.0, -4.0]));
        pos.expr = Some(Expression { text: "wiggle(2, 5)".into(), enabled: true });
        let blur = Property::new(3, "blurriness", "Blurriness", Value::Scalar(0.1 + 0.2)).with_ui(ParamUi::Slider {
            min: 0.0,
            max: 1e3,
            slider_min: 0.0,
            slider_max: 50.0,
            decimals: 1,
        });
        let path = Property::new(5, "path", "Mask Path", Value::path(ShapePath::rect([0.0, 0.0], 10.0, 20.0)));
        let mut mask = PropGroup::new(4, "mask", "Mask 1");
        mask.kind = GroupKind::Mask {
            mode: MaskMode::Subtract,
            inverted: true,
            color: [1, 2, 3],
            locked: false,
            motion_blur: MaskMotionBlur::Off,
            feather_falloff: FeatherFalloff::Linear,
            roto_bezier: true,
        };
        mask.children.push(path.into());
        let mut fx = PropGroup::new(6, "ec.blur.gaussian", "Gaussian Blur");
        fx.kind = GroupKind::Effect { effect: "ec.blur.gaussian".into() };
        fx.enabled = false;
        fx.label = Some(effectcraft_color::Label::Red);
        fx.children.push(blur.into());
        let mut root = PropGroup::new(1, "root", "Root");
        root.children.extend([pos.into(), mask.into(), fx.into(), PropGroup::new(7, "empty", "Empty").into()]);
        root.into()
    }

    #[test]
    fn node_serializes_like_the_derive_and_reads_back() {
        let n = tree();
        let json = serde_json::to_string(&n).unwrap();
        let reference: RefNode = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&reference).unwrap(), json);
        assert_eq!(serde_json::to_string_pretty(&reference).unwrap(), serde_json::to_string_pretty(&n).unwrap());
        let back: Node = serde_json::from_str(&json).unwrap();
        assert_eq!(back, n);
        assert_eq!(Node::from(reference), n);
        // Pretty files and in-memory JSON values (the control channel) read the same.
        assert_eq!(serde_json::from_str::<Node>(&serde_json::to_string_pretty(&n).unwrap()).unwrap(), n);
        assert_eq!(serde_json::from_value::<Node>(serde_json::to_value(&n).unwrap()).unwrap(), n);
    }

    /// Moves `"node"` to the end of every object that has it (the buffering fallback).
    fn tag_last(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(o) => {
                if let Some(t) = o.remove("node") {
                    o.insert("node".into(), t);
                }
                o.values_mut().for_each(tag_last);
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(tag_last),
            _ => {}
        }
    }

    #[test]
    fn node_reads_the_tag_anywhere_and_ignores_unknown_fields() {
        let n = tree();
        let mut v = serde_json::to_value(&n).unwrap();
        tag_last(&mut v);
        let text = serde_json::to_string(&v).unwrap();
        assert!(!text.starts_with(r#"{"node""#), "{text}");
        assert_eq!(serde_json::from_str::<Node>(&text).unwrap(), n);
        assert_eq!(serde_json::from_value::<Node>(v).unwrap(), n);

        let prop = |s: &str| serde_json::from_str::<Node>(s).unwrap();
        let p = Property::new(9, "opacity", "Opacity", Value::Scalar(50.0));
        let first = r#"{"node":"Prop","uid":9,"match":"opacity","name":"Opacity","value":{"t":"Scalar","v":50.0}}"#;
        let middle = r#"{"uid":9,"match":"opacity","node":"Prop","name":"Opacity","value":{"t":"Scalar","v":50.0}}"#;
        let last = r#"{"uid":9,"match":"opacity","name":"Opacity","value":{"t":"Scalar","v":50.0},"node":"Prop"}"#;
        let unknown = r#"{"node":"Prop","future":[1,{"x":2}],"uid":9,"match":"opacity","name":"Opacity","value":{"t":"Scalar","v":50.0}}"#;
        let unknown_late = r#"{"future":{"node":"Group"},"uid":9,"match":"opacity","name":"Opacity","node":"Prop","value":{"t":"Scalar","v":50.0}}"#;
        // Missing fields with defaults read back as them (`hold_only` is false when absent).
        let mut p_saved = p.clone();
        p_saved.hold_only = false;
        for s in [first, middle, last, unknown, unknown_late] {
            assert_eq!(prop(s), Node::Prop(p_saved.clone()), "{s}");
            assert_eq!(serde_json::from_str::<RefNode>(s).map(Node::from).unwrap(), Node::Prop(p_saved.clone()), "{s}");
        }
        let g = PropGroup::new(8, "transform", "Transform");
        for s in [
            r#"{"node":"Group","uid":8,"match":"transform","name":"Transform"}"#,
            r#"{"uid":8,"match":"transform","name":"Transform","node":"Group"}"#,
            r#"{"uid":8,"node":"Group","match":"transform","name":"Transform","kind":{"kind":"Plain"},"enabled":true,"children":[]}"#,
        ] {
            assert_eq!(prop(s), Node::Group(g.clone()), "{s}");
        }
    }

    #[test]
    fn node_errors_like_the_derive() {
        for (s, want) in [
            (r#"{"uid":9}"#, "missing field `node`"),
            (r#"{}"#, "missing field `node`"),
            (r#"{"node":"Layer","uid":9}"#, "unknown variant `Layer`"),
            (r#"{"uid":9,"node":"Layer"}"#, "unknown variant `Layer`"),
            (r#"{"node":"Prop","uid":9}"#, "missing field `match`"),
            (r#"{"uid":9,"node":"Group"}"#, "missing field `match`"),
            (r#"{"uid":9,"node":"Prop","node":"Prop"}"#, "duplicate field `node`"),
            (r#"[1,2]"#, "invalid type"),
            (r#""Prop""#, "invalid type"),
        ] {
            let e = serde_json::from_str::<Node>(s).unwrap_err().to_string();
            assert!(e.contains(want), "{s}: {e}");
        }
    }
}

#[cfg(test)]
mod ae_behavior_tests {
    use super::*;

    #[test]
    fn animated_property_edit_preserves_inserted_bezier_tangent() {
        let mut p = Property::new(1, "rotation", "Rotation", Value::Scalar(0.0));
        p.keys = vec![Keyframe::new(Tick::ZERO, Value::Scalar(0.0)).eased(), Keyframe::new(Tick::from_seconds_f64(2.0), Value::Scalar(100.0)).eased()];
        p.set_value_at(Tick::from_seconds_f64(1.0), Value::Scalar(30.0));
        assert_eq!(p.keys.len(), 3);
        assert!((p.keys[1].in_ease[0].speed - 75.0).abs() < 1e-8);
        assert!((p.keys[1].out_ease[0].speed - 75.0).abs() < 1e-8);
    }
}

#[cfg(test)]
mod dotted_path_tests {
    use super::*;

    fn num(uid: Uid, m: &str, name: &str) -> Property {
        Property::new(uid, m, name, Value::Scalar(1.0))
    }

    /// A layer root with Transform, then Gaussian Blur and Deep Glow (dotted match ids, a dotted
    /// param id), then a second Gaussian Blur.
    fn root() -> PropGroup {
        let effects = PropGroup::new(10, "effects", "Effects")
            .with(PropGroup::new(11, "ec.blur.gaussian", "Gaussian Blur").with(num(12, "blurriness", "Blurriness")))
            .with(PropGroup::new(20, "ec.stylize.deepglow", "Deep Glow (Native)").with(num(21, "PEDG2-0001", "Radius")).with(num(22, "PEDG2.0002", "Exposure")))
            .with(PropGroup::new(30, "ec.blur.gaussian", "Gaussian Blur 2").with(num(31, "blurriness", "Blurriness")));
        PropGroup::new(1, "root", "root").with(PropGroup::new(2, "transform", "Transform").with(num(3, "position", "Position"))).with(effects)
    }

    #[test]
    fn dotted_match_ids_resolve_as_one_segment() {
        let r = root();
        let uid = |p: &str| r.prop(p).map(|p| p.uid);
        assert_eq!(uid("effects/ec.stylize.deepglow/PEDG2-0001"), Some(21));
        assert_eq!(uid("effects/ec.stylize.deepglow/PEDG2.0002"), Some(22));
        assert_eq!(uid("effects.ec.stylize.deepglow.PEDG2.0002"), Some(22));
        assert_eq!(uid("effects/#2/PEDG2-0001"), Some(21));
        assert_eq!(uid("effects/Deep Glow (Native)/PEDG2-0001"), Some(21));
        assert_eq!(uid("effects/ec.blur.gaussian/blurriness"), Some(12));
        assert_eq!(uid("effects/ec.blur.gaussian#2/blurriness"), Some(31));
        // `.` still separates segments that are not one match id.
        assert_eq!(uid("transform.position"), Some(3));
        assert_eq!(uid("transform/position"), Some(3));
        assert_eq!(uid("effects/ec.stylize.nope/PEDG2-0001"), None);
        assert_eq!(r.group("effects/ec.stylize.deepglow").map(|g| g.uid), Some(20));
        // Every match path round-trips.
        for u in [3, 12, 21, 22, 31] {
            let p = r.match_path_of(u).unwrap_or_default();
            assert_eq!(uid(&p), Some(u), "{p}");
        }
        let walk = r.resolve_path("effects/ec.stylize.nope/PEDG2-0001");
        assert_eq!(walk.segs, vec!["effects".to_string()]);
        assert_eq!(walk.rest, vec!["ec.stylize.nope".to_string(), "PEDG2-0001".to_string()]);
    }

    #[test]
    fn hostile_paths_resolve_to_nothing_quickly() {
        let r = root();
        assert!(r.prop(&".".repeat(100_000)).is_none());
        assert!(r.prop(&"a.".repeat(50_000)).is_none());
        assert!(r.prop("").is_none());
    }
}
