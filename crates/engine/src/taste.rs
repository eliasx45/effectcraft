//! The ease taste check (editing harness step 3): does a keyframed move ease the way Elias eases?
//!
//! Every pair of neighbouring keys whose value changes is a *move*. It gets a move type from its
//! layer ([`MoveType`]: an entrance from invisible or off the frame, an exit, an on-screen move,
//! a fade, a scale pop from 0, a one-axis scrub from an edge) and is judged on five criteria learned from his finished comps (25 Oct 2026,
//! 20-08 + 10-10: 730 moves plus 101 keys he changed after an agent handed a comp over):
//!
//! 1. **Peak position**: where the speed peaks in the move. Entrances peak early, exits late,
//!    on → on moves in the middle, fade-ins in the first half.
//! 2. **Peak sharpness vs length**: the fastest frame over the average frame stays under a cap
//!    that grows with the move's length (`a + b ln frames`): a short move peaks lower.
//! 3. **Speed cap**: position moves stay under a share of the frame's width per frame.
//! 4. **Landing length**: an entrance spends a share of its frames on the last 10 % of its
//!    travel (the long settle).
//! 5. **Hard fails**: a linear move, After Effects' default Easy Ease (33/33), a speed jump at a
//!    key between two moves, one key per frame (baked).
//!
//! A move that passes and has the shape of one of his approved curves for its type is
//! [`Verdict::Approved`] ✅; one that passes with another shape is [`Verdict::Close`] 🟡 (with how
//! it differs from the nearest approved curve); one that fails is [`Verdict::Off`] ❌. A ❌ blocks
//! a handover only once its move type has [`Rules::block_at`] approved curves; before that it
//! warns. Curves live in a [`Profile`] (built in, plus his approvals and ⏳ candidates in the
//! config store's [`PROFILE_FILE`]); only he approves one (`taste.approve`).

use effectcraft_keyframe::{Interp, Keyframe, Value as KValue, side_ease, spatial_segment_length};
use effectcraft_project::{Comp, Layer, LayerId, Property, Uid};
use effectcraft_render::EvalCtx;
use effectcraft_time::Tick;
use serde::{Deserialize, Serialize};

/// Where the profile's own entries (approvals, candidates, rules) are stored.
pub const PROFILE_FILE: &str = "taste-profile.json";
/// Largest profile file read (bytes).
const MAX_PROFILE_BYTES: usize = 1 << 20;
/// Most moves one check reports.
pub const MAX_MOVES: usize = 4000;
/// Samples of a move's progress curve for the shape distance and the peak.
const SHAPE_SAMPLES: usize = 96;
/// A run of this many keys, each at most a frame apart, is baked motion.
const BAKED_RUN: usize = 4;
/// Opacity (%) and scale (%) at or under which a layer counts as invisible.
const INVISIBLE: f64 = 5.0;
/// Margin outside the comp frame (px) past which a position is off screen.
const OFF_FRAME: f64 = 40.0;
/// Most child layers looked at to tell whether a rig null's move is an entrance.
const MAX_CHILDREN: usize = 64;
/// Deepest parenting chain followed down to the drawn layers a rig moves.
const MAX_DEPTH: usize = 16;
/// An anchor this close to the content's edge (share of its size) makes a one-axis scale a scrub.
const EDGE_ANCHOR: f64 = 0.03;

// ---------------------------------------------------------------- the profile

/// What a move does, read from its layer and property.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MoveType {
    /// Invisible (opacity ≈ 0, not started yet, scaled to 0, off the frame) → visible.
    Entrance,
    /// Visible → invisible.
    Exit,
    /// On screen → another spot, size or angle on screen.
    Move,
    FadeIn,
    FadeOut,
    /// Scale from ≈ 0 up (the kick under a Motion Library Bounce).
    ScalePop,
    /// One axis of Scale grows or shrinks from an anchor on the content's edge (a progress bar
    /// filling, a wipe): peaks like an on-screen move.
    Scrub,
    /// Any other animated property: only the hard fails apply.
    Other,
}

impl MoveType {
    pub const ALL: [MoveType; 8] = [
        MoveType::Entrance,
        MoveType::Exit,
        MoveType::Move,
        MoveType::FadeIn,
        MoveType::FadeOut,
        MoveType::ScalePop,
        MoveType::Scrub,
        MoveType::Other,
    ];

    pub fn id(self) -> &'static str {
        match self {
            MoveType::Entrance => "entrance",
            MoveType::Exit => "exit",
            MoveType::Move => "move",
            MoveType::FadeIn => "fade-in",
            MoveType::FadeOut => "fade-out",
            MoveType::ScalePop => "scale-pop",
            MoveType::Scrub => "scrub",
            MoveType::Other => "other",
        }
    }

    pub fn words(self) -> &'static str {
        match self {
            MoveType::Entrance => "entrance (invisible → visible)",
            MoveType::Exit => "exit (visible → invisible)",
            MoveType::Move => "on-screen move (on → on)",
            MoveType::FadeIn => "fade in",
            MoveType::FadeOut => "fade out",
            MoveType::ScalePop => "scale pop from 0",
            MoveType::Scrub => "scrub / wipe (one-axis scale from an edge)",
            MoveType::Other => "other property",
        }
    }

    pub fn parse(s: &str) -> Option<MoveType> {
        let q: String = s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect();
        MoveType::ALL.into_iter().find(|m| m.id().replace('-', "") == q)
    }
}

/// One side of a move's ease, normalised: speed in units of the move's average speed, influence
/// in percent (as the Motion Library and the Graphs cards store them).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Side {
    pub speed: f64,
    pub influence: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Elias approved it: moves with its shape are ✅.
    Approved,
    /// ⏳ Found or proposed, waiting for his OK: not used for ✅.
    Candidate,
    /// He said no.
    Rejected,
}

impl Status {
    pub fn id(self) -> &'static str {
        match self {
            Status::Approved => "approved",
            Status::Candidate => "candidate",
            Status::Rejected => "rejected",
        }
    }
    pub fn mark(self) -> &'static str {
        match self {
            Status::Approved => "✅",
            Status::Candidate => "⏳",
            Status::Rejected => "🚫",
        }
    }
}

/// A curve: his Graphs card, a saved Motion Library curve, or a candidate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Curve {
    pub id: String,
    pub name: String,
    #[serde(rename = "out")]
    pub out: Side,
    #[serde(rename = "in")]
    pub inn: Side,
    /// The move types it is for.
    pub roles: Vec<MoveType>,
    pub status: Status,
    /// `expo-in-out`: the influences follow the move's length (sharper the longer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Where it comes from (a card, a comp and key, a hand fix).
    #[serde(default)]
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// His words when he approved or rejected it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub said: String,
}

impl Curve {
    /// The ease this curve puts on a move of `secs` seconds.
    pub fn at(&self, secs: f64) -> (Side, Side) {
        if self.family.as_deref() == Some("expo-in-out") {
            let o = (65.0 + 30.0 * (secs - 1.0)).clamp(50.0, 90.0);
            return (Side { speed: 0.0, influence: o }, Side { speed: 0.0, influence: (o + 1.5).min(90.0) });
        }
        (self.out, self.inn)
    }
}

/// The five criteria's thresholds (learned from his comps, 25 Oct 2026).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Rules {
    /// A ❌ blocks handover once its move type has this many approved curves (else it warns).
    pub block_at: usize,
    /// Entrances peak in the first this-many percent of the move.
    pub entrance_peak_max: f64,
    /// Exits peak after this percent.
    pub exit_peak_min: f64,
    /// On → on moves peak between these percents.
    pub move_peak: (f64, f64),
    /// Fade-ins peak in the first this-many percent.
    pub fade_in_peak_max: f64,
    /// Fastest frame ÷ average frame ≤ `sharp_a + sharp_b · ln(frames)`.
    pub sharp_a: f64,
    pub sharp_b: f64,
    /// Fastest position frame, in percent of the comp's width per frame.
    pub speed_cap: f64,
    /// The same for exits (his row exits leave at up to ≈ 21 %).
    pub exit_speed_cap: f64,
    /// Entrances spend at least this percent of their frames on the last 10 % of the travel.
    pub entrance_tail_min: f64,
    /// A shape within this (largest gap in progress, %) of an approved curve is that curve.
    pub match_within: f64,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            block_at: 2,
            entrance_peak_max: 30.0,
            exit_peak_min: 75.0,
            move_peak: (30.0, 70.0),
            fade_in_peak_max: 50.0,
            sharp_a: 1.3,
            sharp_b: 2.05,
            speed_cap: 18.0,
            exit_speed_cap: 22.0,
            entrance_tail_min: 30.0,
            match_within: 5.0,
        }
    }
}

impl Rules {
    /// The sharpness cap for a move of `frames` frames.
    pub fn sharp_cap(&self, frames: f64) -> f64 {
        self.sharp_a + self.sharp_b * frames.max(1.0).ln()
    }
}

/// His curves and the thresholds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub rules: Rules,
    pub curves: Vec<Curve>,
}

fn side(speed: f64, influence: f64) -> Side {
    Side { speed, influence }
}

fn builtin(id: &str, name: &str, out: Side, inn: Side, roles: &[MoveType], status: Status, source: &str) -> Curve {
    Curve {
        id: id.into(),
        name: name.into(),
        out,
        inn,
        roles: roles.to_vec(),
        status,
        family: None,
        source: source.into(),
        note: String::new(),
        said: String::new(),
    }
}

/// The built-in profile: Devin Jatho's five Graphs cards and the curves Elias approved (skill
/// effectcraft-motion, 25 Oct 2026), plus his saved Motion Library curves he hasn't approved yet.
pub fn builtin_profile() -> Profile {
    use MoveType::*;
    use Status::*;
    const GRAPHS: &str = "Motion Library → Devin Jatho → Graphs";
    const SAVED: &str = "Motion Library saved curve";
    let mut expo = builtin("expo-in-out-elias", "Expo In-Out (Elias)", side(0.0, 81.0), side(0.0, 82.5), &[Move], Approved, SAVED);
    expo.family = Some("expo-in-out".into());
    expo.note = "on → on; influences follow the length: out = clamp(65 + 30 (s − 1), 50, 90), in = out + 1.5".into();
    let mut kick =
        builtin("pop-kick", "Pop kick (2 f, then ML Bounce)", side(0.0, 0.1), side(0.0, 100.0), &[ScalePop], Approved, "effectcraft-motion skill, 25 Oct");
    kick.note = "scale 0 → 100 over ~2 frames, out 0.1 %; the Motion Library Bounce does the visible motion".into();
    Profile {
        rules: Rules::default(),
        curves: vec![
            builtin("fast-in", "Fast In", side(23.752_732_274_402_83, 0.592_662_210_941_676_9), side(0.0, 92.921_385_819_053), &[Entrance], Approved, GRAPHS),
            builtin(
                "quick-launch",
                "Quick launch · soft landing",
                side(0.0, 39.878_273_010_253_906),
                side(0.0, 90.923_764_970_567_49),
                &[Entrance],
                Approved,
                GRAPHS,
            ),
            builtin("smooth-emphasis", "Smooth emphasis", side(0.0, 70.206_435_521_443_7), side(0.0, 70.073_154_237_535_26), &[Move], Approved, GRAPHS),
            builtin("slow-launch", "Slow launch · quick finish", side(0.0, 83.001_126_183_403_85), side(0.0, 60.121_726_989_746_09), &[Move], Approved, GRAPHS),
            builtin("fast-out", "Fast out", side(0.0, 100.0), side(0.0, 0.887_044_270_833_337), &[Exit], Approved, GRAPHS),
            builtin("expo-out-1", "Expo Out 1 (Elias)", side(0.0, 0.1), side(0.0, 100.0), &[Entrance], Approved, SAVED),
            builtin(
                "entrance-fade",
                "Entrance fade (Elias)",
                side(-0.007_316, 34.818_326_102_362_74),
                side(-0.013_369, 95.585_971_408_420_14),
                &[FadeIn],
                Approved,
                SAVED,
            ),
            expo,
            kick,
            builtin("launch-out", "Launch out (Devin)", side(0.0, 100.0), side(0.0, 3.35), &[Exit], Candidate, SAVED),
            builtin("quick-out-gentle", "Quick out, gentle land (Devin)", side(0.0, 33.33), side(0.0, 83.6), &[Entrance], Candidate, SAVED),
            builtin("snap-in", "Snap in (Devin)", side(0.0, 11.13), side(0.0, 100.0), &[Entrance, ScalePop], Candidate, SAVED),
            builtin("whip", "Whip (Devin)", side(0.0, 90.0), side(0.0, 90.0), &[Move], Candidate, SAVED),
            builtin("reel-pop-in", "Reel Pop-In (Elias)", side(0.0, 5.6), side(0.0, 100.0), &[Entrance, ScalePop], Candidate, SAVED),
        ],
    }
}

/// What the config store keeps: the curves that differ from the built-ins (approvals,
/// candidates, rejections) and the rules when changed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Stored {
    #[serde(default)]
    pub rules: Option<Rules>,
    #[serde(default)]
    pub curves: Vec<Curve>,
}

impl Profile {
    /// The built-ins with `stored` on top (a stored curve replaces the built-in of its id).
    pub fn with(stored: &Stored) -> Profile {
        let mut p = builtin_profile();
        if let Some(r) = &stored.rules {
            p.rules = r.clone();
        }
        for c in &stored.curves {
            match p.curves.iter_mut().find(|x| x.id == c.id) {
                Some(x) => *x = c.clone(),
                None => p.curves.push(c.clone()),
            }
        }
        p
    }

    /// The profile from a config store (the built-ins when there is none or it can't be read).
    pub fn load(store: Option<&dyn crate::config::ConfigStore>) -> (Profile, Stored) {
        let stored: Stored =
            store.and_then(|s| s.read_bounded(PROFILE_FILE, MAX_PROFILE_BYTES).ok().flatten()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        (Profile::with(&stored), stored)
    }

    pub fn approved_for(&self, m: MoveType) -> impl Iterator<Item = &Curve> {
        self.curves.iter().filter(move |c| c.status == Status::Approved && c.roles.contains(&m))
    }

    /// Whether a ❌ on a move of type `m` blocks a handover.
    pub fn blocks(&self, m: MoveType) -> bool {
        m != MoveType::Other && self.approved_for(m).count() >= self.rules.block_at.max(1)
    }

    /// A curve by id or name (ignoring case and punctuation; a unique prefix is enough).
    pub fn find(&self, q: &str) -> Option<&Curve> {
        let k = squash(q);
        if k.is_empty() {
            return None;
        }
        if let Some(c) = self.curves.iter().find(|c| squash(&c.id) == k || squash(&c.name) == k) {
            return Some(c);
        }
        let mut hits = self.curves.iter().filter(|c| squash(&c.id).starts_with(&k) || squash(&c.name).starts_with(&k));
        let first = hits.next()?;
        hits.next().is_none().then_some(first)
    }
}

pub fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

// ---------------------------------------------------------------- shapes

/// Progress (0 → 1) at time share `x` of a 2-key move with normalised eases: After Effects'
/// temporal Bezier, control points at the influences.
pub fn progress(out: Side, inn: Side, x: f64) -> f64 {
    let (oi, ii) = ((out.influence / 100.0).clamp(0.0, 1.0), (inn.influence / 100.0).clamp(0.0, 1.0));
    let p1 = (oi, out.speed * oi);
    let p2 = (1.0 - ii, 1.0 - inn.speed * ii);
    let bez = |t: f64, a: f64, b: f64| {
        let u = 1.0 - t;
        3.0 * u * u * t * a + 3.0 * u * t * t * b + t * t * t
    };
    let x = x.clamp(0.0, 1.0);
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..48 {
        let t = (lo + hi) / 2.0;
        if bez(t, p1.0, p2.0) < x { lo = t } else { hi = t }
    }
    bez((lo + hi) / 2.0, p1.1, p2.1)
}

/// Largest gap between two shapes' progress over the move, in percent.
pub fn distance(a: (Side, Side), b: (Side, Side)) -> f64 {
    (0..=SHAPE_SAMPLES)
        .map(|i| {
            let x = i as f64 / SHAPE_SAMPLES as f64;
            (progress(a.0, a.1, x) - progress(b.0, b.1, x)).abs()
        })
        .fold(0.0, f64::max)
        * 100.0
}

/// The measured criteria of a shape over `frames` frames.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Shape {
    /// Where the speed peaks (% of the move).
    pub peak_pos: f64,
    /// Fastest frame ÷ average frame.
    pub sharpness: f64,
    /// Largest share of the move covered in one frame (0..1).
    pub peak_share: f64,
    /// Frames spent on the last 10 % of the travel.
    pub tail10: f64,
}

pub fn measure(out: Side, inn: Side, frames: f64) -> Shape {
    let n = frames.round().clamp(1.0, 4000.0) as usize;
    let mut best = (0usize, f64::MIN);
    let mut prev = 0.0;
    for i in 0..400usize {
        let p = progress(out, inn, (i + 1) as f64 / 400.0);
        if p - prev > best.1 {
            best = (i, p - prev);
        }
        prev = p;
    }
    let mut share: f64 = 0.0;
    let mut at90 = n;
    let mut prev = 0.0;
    for k in 0..n {
        let p = progress(out, inn, (k + 1) as f64 / n as f64);
        share = share.max(p - prev);
        if at90 == n && p >= 0.9 - 1e-9 {
            at90 = k + 1;
        }
        prev = p;
    }
    Shape { peak_pos: (best.0 as f64 + 0.5) / 4.0, sharpness: share * n as f64, peak_share: share, tail10: (n - at90) as f64 }
}

/// How shape `m` differs from curve `c` in words: "launch softer (out 12 % vs 0.1 %)".
pub fn differs(m: (Side, Side), c: (Side, Side)) -> String {
    let mut parts = vec![];
    let pct = |x: f64| if x < 1.0 { format!("{x:.1} %") } else { format!("{x:.0} %") };
    if (m.0.influence - c.0.influence).abs() > 2.0 || (m.0.speed - c.0.speed).abs() > 0.5 {
        let softer = m.0.influence > c.0.influence || m.0.speed < c.0.speed - 0.5;
        parts.push(format!("launch {} (out {} vs {})", if softer { "softer" } else { "sharper" }, pct(m.0.influence), pct(c.0.influence)));
    }
    if (m.1.influence - c.1.influence).abs() > 2.0 || (m.1.speed - c.1.speed).abs() > 0.5 {
        let longer = m.1.influence > c.1.influence;
        parts.push(format!("landing {} (in {} vs {})", if longer { "longer" } else { "shorter" }, pct(m.1.influence), pct(c.1.influence)));
    }
    if parts.is_empty() { "same influences, different speeds at the keys".into() } else { parts.join(", ") }
}

// ---------------------------------------------------------------- reading moves

/// A key pair of a property that changes value.
#[derive(Clone, Debug, PartialEq)]
pub struct MoveRef {
    pub layer: LayerId,
    pub uid: Uid,
    /// The pair's first key (0-based).
    pub key: usize,
}

/// One judged move.
#[derive(Clone, Debug, PartialEq)]
pub struct Judged {
    pub at: MoveRef,
    pub layer_name: String,
    /// Readable path (`transform/position`).
    pub path: String,
    pub property: String,
    /// Comp seconds of the two keys.
    pub t0: f64,
    pub t1: f64,
    pub frames: f64,
    pub kind: MoveType,
    pub verdict: Verdict,
    pub blocking: bool,
    /// Normalised eases (None for linear / baked moves).
    pub ease: Option<(Side, Side)>,
    pub shape: Option<Shape>,
    /// Peak speed in px per frame (position moves).
    pub px_per_frame: Option<f64>,
    /// The same in percent of the comp's width per frame.
    pub frame_pct: Option<f64>,
    /// The approved curve it matches.
    pub curve: Option<String>,
    /// The nearest approved curve for its type (or any approved curve): (id, name, gap %).
    pub nearest: Option<(String, String, f64)>,
    pub problems: Vec<String>,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Approved,
    Close,
    Off,
}

impl Verdict {
    pub fn id(self) -> &'static str {
        match self {
            Verdict::Approved => "approved",
            Verdict::Close => "close",
            Verdict::Off => "off",
        }
    }
    pub fn mark(self) -> &'static str {
        match self {
            Verdict::Approved => "✅",
            Verdict::Close => "🟡",
            Verdict::Off => "❌",
        }
    }
}

/// Properties the criteria apply to (by match id); others get the hard fails only.
fn checked_match(m: &str) -> bool {
    matches!(m, "position" | "xPosition" | "yPosition" | "zPosition" | "scale" | "rotation" | "zRotation" | "opacity")
}

/// The opacity (%) of `l` at its layer time `t`.
fn opacity_at(l: &Layer, t: Tick) -> f64 {
    l.transform().and_then(|g| g.prop("opacity")).map(|p| p.value_at(t).components().first().copied().unwrap_or(100.0)).unwrap_or(100.0)
}

/// Whether `l` shows nothing at comp time `ct`: not started or ended, opacity or scale ≈ 0.
fn invisible(l: &Layer, ct: Tick, frame: Tick) -> bool {
    if ct + frame <= l.in_point || ct >= l.out_point {
        return true;
    }
    let t = l.layer_time(ct);
    if opacity_at(l, t) <= INVISIBLE {
        return true;
    }
    let scale = l.transform().and_then(|g| g.prop("scale")).map(|p| p.value_at(t).components()).unwrap_or_default();
    scale.iter().take(2).any(|s| s.abs() <= INVISIBLE)
}

/// Whether `l` (or, for a rig null, every one of its first children) is invisible at `ct`.
fn hidden(comp: &Comp, l: &Layer, ct: Tick, frame: Tick) -> bool {
    if invisible(l, ct, frame) {
        return true;
    }
    let kids: Vec<&Layer> = comp.layers.iter().filter(|c| c.parent == Some(l.id)).take(MAX_CHILDREN).collect();
    let is_null = matches!(l.source, effectcraft_project::LayerSource::Null);
    is_null && !kids.is_empty() && kids.iter().all(|c| invisible(c, ct, frame))
}

/// The drawn layers whose place `l` sets: `l` itself when it has a picture, and every layer
/// parented to it (directly or further down) that has one.
fn drawn_parts<'a>(comp: &'a Comp, l: &'a Layer) -> Vec<&'a Layer> {
    let drawn = |x: &Layer| x.source.is_av() && x.switches.video && !x.switches.guide;
    let mut out: Vec<&Layer> = if drawn(l) { vec![l] } else { vec![] };
    let mut frontier = vec![l.id];
    let mut seen = 0usize;
    for _ in 0..MAX_DEPTH {
        if frontier.is_empty() || seen >= MAX_CHILDREN {
            break;
        }
        let kids: Vec<&Layer> = comp.layers.iter().filter(|c| c.parent.is_some_and(|p| frontier.contains(&p))).take(MAX_CHILDREN).collect();
        seen += kids.len();
        frontier = kids.iter().map(|c| c.id).collect();
        out.extend(kids.into_iter().filter(|c| drawn(c)));
    }
    out
}

/// Whether nothing `l` places shows at comp time `ct`: every drawn part is off (in/out point,
/// opacity 0) or its content box lies wholly outside the frame. `None` when that can't be told
/// (no drawn part, or none with a content box).
fn out_of_frame(ctx: &EvalCtx, l: &Layer, ct: Tick) -> Option<bool> {
    let parts = drawn_parts(ctx.comp, l);
    let c = ctx.at(ct);
    let (w, h) = (ctx.comp.width as f64, ctx.comp.height as f64);
    let frame = [[0.0, 0.0], [w, 0.0], [w, h], [0.0, h]];
    let mut measured = false;
    for x in parts {
        if !crate::layout::visibility(&c, x).0 {
            continue;
        }
        let Some(g) = crate::layout::layer_geometry(&c, x) else { continue };
        if !g.projected || !g.corners.iter().flatten().all(|v| v.is_finite()) {
            continue;
        }
        measured = true;
        let inside = crate::layout::polygon_area(&crate::layout::clip_convex(&g.corners, &frame));
        if inside > 1.0 {
            return Some(false);
        }
    }
    measured.then_some(true)
}

/// Whether the anchor-only fallback puts `v` (a Position value) off the frame: only for
/// unparented layers, whose Position is in comp pixels.
fn off_frame(comp: &Comp, l: &Layer, v: &KValue) -> bool {
    if l.parent.is_some() {
        return false;
    }
    let c = v.components();
    let (x, y) = (c.first().copied().unwrap_or(0.0), c.get(1).copied().unwrap_or(0.0));
    let (w, h) = (comp.width as f64, comp.height as f64);
    x < -OFF_FRAME || y < -OFF_FRAME || x > w + OFF_FRAME || y > h + OFF_FRAME
}

/// Whether a Scale move `a → b` (layer time `t`) changes one axis only, from an anchor on the
/// content's edge along that axis.
fn scrub(ctx: &EvalCtx, l: &Layer, ca: &[f64], cb: &[f64], t: Tick) -> bool {
    let d = |k: usize| (cb.get(k).copied().unwrap_or(0.0) - ca.get(k).copied().unwrap_or(0.0)).abs();
    let axis = match (d(0) > 0.5, d(1) > 0.5) {
        (true, false) => 0,
        (false, true) => 1,
        _ => return false,
    };
    let Some(b) = effectcraft_render::content_bounds(&ctx.at(l.comp_time(t)), l) else { return false };
    let anchor = l.transform().and_then(|g| g.prop("anchor")).map(|p| p.value_at(t).components()).unwrap_or_default();
    let (Some(&lo), Some(&hi), Some(&at)) = (b.get(axis), b.get(axis + 2), anchor.get(axis)) else { return false };
    let size = (hi - lo).abs();
    size > 1e-6 && ((at - lo).abs() <= EDGE_ANCHOR * size || (hi - at).abs() <= EDGE_ANCHOR * size)
}

/// The move type of key pair `i` → `i + 1` of `prop` (match id `m`) on `l`.
pub fn classify(ctx: &EvalCtx, l: &Layer, m: &str, prop: &Property, i: usize) -> MoveType {
    let comp = ctx.comp;
    let (Some(a), Some(b)) = (prop.keys.get(i), prop.keys.get(i + 1)) else { return MoveType::Other };
    let (ca, cb) = (a.value.components(), b.value.components());
    let first = |c: &[f64]| c.first().copied().unwrap_or(0.0);
    if m == "opacity" {
        return if first(&cb) >= first(&ca) { MoveType::FadeIn } else { MoveType::FadeOut };
    }
    if !checked_match(m) {
        return MoveType::Other;
    }
    if m == "scale" && scrub(ctx, l, &ca, &cb, a.time) {
        return MoveType::Scrub;
    }
    let tiny = |c: &[f64]| !c.is_empty() && c.iter().take(2).all(|s| s.abs() <= INVISIBLE);
    if m == "scale" && tiny(&ca) {
        return MoveType::ScalePop;
    }
    if m == "scale" && tiny(&cb) {
        return MoveType::Exit;
    }
    let frame = comp.frame_rate.frame_duration();
    let (t0, t1) = (l.comp_time(a.time), l.comp_time(b.time));
    let is_pos = m == "position" || m.ends_with("Position");
    // Off the frame: the content boxes of what the layer places (itself, a rig's children), or
    // for a layer with none to measure, its own Position.
    let gone = |t: Tick, v: &KValue| out_of_frame(ctx, l, t).unwrap_or_else(|| m == "position" && off_frame(comp, l, v));
    if hidden(comp, l, t0, frame) || (is_pos && gone(t0, &a.value)) {
        return MoveType::Entrance;
    }
    // The last frame of a layer that ends with its move counts as gone.
    if hidden(comp, l, t1, frame) || (is_pos && gone(t1, &b.value)) || t1 + frame >= l.out_point {
        return MoveType::Exit;
    }
    MoveType::Move
}

/// The pair's normalised eases (the dimension that changes most, or the motion path) and its
/// value change, or `None` when the value doesn't change.
pub fn normalised(prop: &Property, i: usize) -> Option<(Side, Side, f64)> {
    let (a, b) = (prop.keys.get(i)?, prop.keys.get(i + 1)?);
    let dt = (b.time.seconds() - a.time.seconds()).max(1e-9);
    let spatial = prop.spatial && matches!(a.value, KValue::Vec2(_) | KValue::Vec3(_));
    let (d, dv) = if spatial {
        (0, spatial_segment_length(&prop.keys, i))
    } else {
        let (ca, cb) = (a.value.components(), b.value.components());
        ca.iter().zip(&cb).enumerate().map(|(d, (x, y))| (d, y - x)).max_by(|p, q| p.1.abs().total_cmp(&q.1.abs()))?
    };
    if dv.abs() < 1e-9 || !dv.is_finite() {
        return None;
    }
    let o = side_ease(&prop.keys, i, d, prop.spatial, true)?;
    let n = side_ease(&prop.keys, i + 1, d, prop.spatial, false)?;
    let k = dt / dv;
    Some((side(o.speed * k, o.influence * 100.0), side(n.speed * k, n.influence * 100.0), dv.abs()))
}

/// Runs of keys at most a frame apart ([`BAKED_RUN`] or more): `(first, last)` key indices.
fn baked_runs(keys: &[Keyframe], frame: f64) -> Vec<(usize, usize)> {
    let mut out = vec![];
    let mut start = 0usize;
    for i in 1..=keys.len() {
        let close = match (keys.get(i - 1), keys.get(i)) {
            (Some(a), Some(b)) => b.time.seconds() - a.time.seconds() <= frame * 1.01,
            _ => false,
        };
        if !close {
            if i - start >= BAKED_RUN {
                out.push((start, i - 1));
            }
            start = i;
        }
    }
    out
}

/// The speed jump at key `i` (between the moves `i − 1 → i` and `i → i + 1`): the speeds in and
/// out in units of each move's average speed, when they differ visibly.
fn speed_jump(prop: &Property, i: usize) -> Option<(f64, f64)> {
    let k = prop.keys.get(i)?;
    if i == 0 || k.in_interp == Interp::Hold || k.out_interp == Interp::Hold {
        return None;
    }
    let (_, inn, _) = normalised(prop, i - 1)?;
    let (out, _, _) = normalised(prop, i)?;
    let (si, so) = (inn.speed, out.speed);
    let big = si.abs().max(so.abs());
    if big < 0.5 {
        return None;
    }
    let opposite = si * so < 0.0 && si.abs().min(so.abs()) > 0.2;
    (opposite || si.abs().min(so.abs()) < 0.5 * big).then_some((si, so))
}

/// Judge every move of `ctx.comp`'s layers whose keys overlap `start..=end` (comp seconds); `only`
/// limits it to some layers. Moves on properties other than position, scale, rotation and
/// opacity are only checked for the hard fails, and only with `all`. (`ctx` gives the project,
/// for the content boxes of what a layer places.)
pub fn check(profile: &Profile, ctx: &EvalCtx, only: Option<&[LayerId]>, start: f64, end: f64, all: bool) -> (Vec<Judged>, bool) {
    let comp = ctx.comp;
    let mut out: Vec<Judged> = vec![];
    let mut truncated = false;
    let fps = comp.frame_rate.as_f64();
    if !fps.is_finite() || fps <= 0.0 {
        return (out, false);
    }
    let frame = 1.0 / fps;
    for l in comp.layers.iter() {
        if only.is_some_and(|ids| !ids.contains(&l.id)) {
            continue;
        }
        let mut props: Vec<(String, &Property)> = vec![];
        l.props.walk("", &mut |path, p| {
            if p.keys.len() >= 2 && p.value.interpolates() {
                props.push((path.to_string(), p));
            }
        });
        for (_, p) in props {
            let m = p.match_id.as_str();
            if !all && !checked_match(m) {
                continue;
            }
            let path = crate::commands::match_path_of(&l.props, p.uid).unwrap_or_else(|| p.name.clone());
            let runs = baked_runs(&p.keys, frame * 100.0 / l.stretch.abs().max(1e-6));
            for i in 0..p.keys.len().saturating_sub(1) {
                if out.len() >= MAX_MOVES {
                    truncated = true;
                    return (out, truncated);
                }
                let (Some(a), Some(b)) = (p.keys.get(i), p.keys.get(i + 1)) else { continue };
                let (t0, t1) = (l.comp_time(a.time).seconds(), l.comp_time(b.time).seconds());
                if t0.max(t1) < start - 1e-9 || t0.min(t1) > end + 1e-9 || a.value == b.value || a.out_interp == Interp::Hold {
                    continue;
                }
                // A baked run is one finding, on its first pair.
                if let Some(&(r0, r1)) = runs.iter().find(|(r0, r1)| (*r0..*r1).contains(&i)) {
                    if i == r0 {
                        out.push(judged_baked(profile, ctx, l, p, &path, i, r1, fps));
                    }
                    continue;
                }
                out.push(judge(profile, ctx, l, p, &path, i, fps));
            }
        }
    }
    (out, truncated)
}

fn base(comp: &Comp, l: &Layer, p: &Property, path: &str, i: usize, j: usize, fps: f64, kind: MoveType) -> Judged {
    let t = |k: usize| p.keys.get(k).map(|k| l.comp_time(k.time).seconds()).unwrap_or(0.0);
    let _ = comp;
    Judged {
        at: MoveRef { layer: l.id, uid: p.uid, key: i },
        layer_name: l.name.clone(),
        path: path.to_string(),
        property: p.name.clone(),
        t0: t(i),
        t1: t(j),
        frames: ((t(j) - t(i)) * fps * 100.0).round() / 100.0,
        kind,
        verdict: Verdict::Off,
        blocking: false,
        ease: None,
        shape: None,
        px_per_frame: None,
        frame_pct: None,
        curve: None,
        nearest: None,
        problems: vec![],
        message: String::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn judged_baked(profile: &Profile, ctx: &EvalCtx, l: &Layer, p: &Property, path: &str, i: usize, last: usize, fps: f64) -> Judged {
    let kind = classify(ctx, l, p.match_id.as_str(), p, i);
    let mut j = base(ctx.comp, l, p, path, i, last, fps, kind);
    j.blocking = profile.blocks(kind);
    j.problems.push(format!("baked: {} keys one frame apart", last - i + 1));
    j.message = format!(
        "{} {}: {} keys one frame apart (baked motion). Use 2–4 keys with one of his curves (or an expression), never one key per frame.",
        l.name,
        p.name,
        last - i + 1
    );
    j
}

/// The approved curves a move of `kind` may match, and the fallback list (every approved curve).
fn candidates(profile: &Profile, kind: MoveType) -> Vec<&Curve> {
    let mine: Vec<&Curve> = profile.approved_for(kind).collect();
    if mine.is_empty() { profile.curves.iter().filter(|c| c.status == Status::Approved).collect() } else { mine }
}

fn nearest<'a>(list: &[&'a Curve], m: (Side, Side), secs: f64) -> Option<(&'a Curve, f64)> {
    list.iter().map(|c| (*c, distance(m, c.at(secs)))).min_by(|a, b| a.1.total_cmp(&b.1))
}

#[allow(clippy::too_many_arguments)]
fn judge(profile: &Profile, ctx: &EvalCtx, l: &Layer, p: &Property, path: &str, i: usize, fps: f64) -> Judged {
    let comp = ctx.comp;
    let m = p.match_id.as_str();
    let kind = classify(ctx, l, m, p, i);
    let mut j = base(comp, l, p, path, i, i + 1, fps, kind);
    j.blocking = profile.blocks(kind);
    let r = &profile.rules;
    let (Some(a), Some(b)) = (p.keys.get(i), p.keys.get(i + 1)) else { return j };
    let secs = (j.t1 - j.t0).abs().max(1e-9);
    let frames = (secs * fps).max(1.0);
    let name = format!("{} {} ({})", l.name, p.name, kind.id());
    // A spring expression (Motion Library Bounce) shapes what shows; the keys are only its kick.
    if p.has_expression() && matches!(kind, MoveType::ScalePop | MoveType::Entrance | MoveType::Exit) {
        j.verdict = Verdict::Close;
        j.message =
            format!("{name}: 🟡 an expression drives this property (its keys are the kick, not the visible ease); check it with ease_graph or a render");
        j.problems.clear();
        return j;
    }
    // Hard fails.
    if a.out_interp == Interp::Linear || b.in_interp == Interp::Linear {
        j.problems.push("linear: constant speed, no ease".into());
    }
    let Some((out, inn, dv)) = normalised(p, i) else {
        j.message = format!("{name}: no ease could be read");
        return j;
    };
    let ease = (out, inn);
    j.ease = Some(ease);
    let easy = |s: Side| s.speed.abs() < 0.05 && (s.influence - 100.0 / 3.0).abs() < 0.6;
    if a.out_interp == Interp::Bezier && b.in_interp == Interp::Bezier && easy(out) && easy(inn) {
        j.problems.push("Easy Ease (33/33): After Effects' default, not one of his curves".into());
    }
    if let Some((si, so)) = speed_jump(p, i) {
        j.problems.push(format!("speed jump at its first key: arrives at {si:.2}× and leaves at {so:.2}× the average speed"));
    }
    // The five criteria (the first four on the moves they apply to).
    let shape = measure(out, inn, frames);
    j.shape = Some(shape);
    let pos = shape.peak_pos;
    let linear = j.problems.iter().any(|x| x.starts_with("linear"));
    if !linear {
        match kind {
            MoveType::Entrance if pos > r.entrance_peak_max => {
                j.problems.push(format!("peaks at {pos:.0} % of the move: entrances peak in the first {:.0} %", r.entrance_peak_max))
            }
            MoveType::Exit if pos < r.exit_peak_min => j.problems.push(format!("peaks at {pos:.0} %: exits peak after {:.0} %", r.exit_peak_min)),
            MoveType::Move | MoveType::Scrub if pos < r.move_peak.0 || pos > r.move_peak.1 => {
                {
                let what = if kind == MoveType::Scrub { "scrubs" } else { "on-screen moves" };
                j.problems.push(format!("peaks at {pos:.0} %: {what} peak between {:.0} and {:.0} %", r.move_peak.0, r.move_peak.1))
            }
            }
            MoveType::FadeIn if pos > r.fade_in_peak_max => {
                j.problems.push(format!("peaks at {pos:.0} %: fade-ins peak in the first {:.0} %", r.fade_in_peak_max))
            }
            _ => {}
        }
        let cap = r.sharp_cap(frames);
        if kind != MoveType::Other && kind != MoveType::ScalePop && shape.sharpness > cap {
            j.problems.push(format!(
                "too sharp for {frames:.0} frames: its fastest frame is {:.1}× the average (cap {cap:.1}×; {:.0} % of the way in one frame)",
                shape.sharpness,
                shape.peak_share * 100.0
            ));
        }
        if kind == MoveType::Entrance && shape.tail10 < r.entrance_tail_min / 100.0 * frames - 0.5 {
            j.problems.push(format!(
                "lands too soon: {:.0} of its {frames:.0} frames on the last 10 % of the way (entrances settle for at least {:.0} % of their frames)",
                shape.tail10, r.entrance_tail_min
            ));
        }
    }
    if m == "position" || m.ends_with("Position") {
        let pct = shape.peak_share * dv / (comp.width.max(1) as f64) * 100.0;
        j.px_per_frame = Some((shape.peak_share * dv * 10.0).round() / 10.0);
        j.frame_pct = Some((pct * 10.0).round() / 10.0);
        let cap = if kind == MoveType::Exit { r.exit_speed_cap } else { r.speed_cap };
        if pct > cap {
            j.problems.push(format!("too fast: {pct:.0} % of the frame's width in one frame at its peak (cap {cap:.0} %)"));
        }
    }
    // The shape against his curves.
    let mine: Vec<&Curve> = profile.approved_for(kind).collect();
    let near_mine = nearest(&mine, ease, secs);
    let pool = candidates(profile, kind);
    // The fix only ever suggests a curve approved for this move type.
    j.nearest = near_mine.map(|(c, d)| (c.id.clone(), c.name.clone(), (d * 10.0).round() / 10.0));
    let near = near_mine.or_else(|| nearest(&pool, ease, secs));
    let none_yet = if near_mine.is_none() { format!(" No approved {} curve yet (taste.propose files one as ⏳).", kind.id()) } else { String::new() };
    // A scale pop's kick: a couple of frames from 0, out ≈ 0 (the Bounce does the rest).
    let kick = kind == MoveType::ScalePop && frames <= 3.5 && out.influence <= 2.0;
    if j.problems.is_empty() {
        if let Some((c, d)) = near_mine.filter(|(_, d)| *d <= r.match_within) {
            j.verdict = Verdict::Approved;
            j.curve = Some(c.name.clone());
            j.message = format!("{name}: {} {}", Verdict::Approved.mark(), if d < 0.5 { c.name.clone() } else { format!("{} ({d:.1} % off)", c.name) });
            return j;
        }
        if kick {
            j.verdict = Verdict::Approved;
            j.curve = Some("Pop kick (2 f, then ML Bounce)".into());
            j.message = format!("{name}: ✅ pop kick ({frames:.0} f, out {:.1} %)", out.influence);
            return j;
        }
        j.verdict = Verdict::Close;
        j.message = match near {
            Some((c, d)) => format!(
                "{name}: 🟡 in his usual range, not one of his approved curves: like {}, {d:.0} % off ({}).{none_yet}",
                c.name,
                differs(ease, c.at(secs))
            ),
            None => format!("{name}: 🟡 in his usual range; no approved curve to compare with"),
        };
        return j;
    }
    j.verdict = Verdict::Off;
    let label = if near_mine.is_some() { "Nearest approved" } else { "Closest of his curves (approved for other moves)" };
    let fix = near.map(|(c, d)| format!(" {label}: {} ({d:.0} % off: {}).{none_yet}", c.name, differs(ease, c.at(secs)))).unwrap_or_default();
    let warn = if j.blocking { "" } else { " (warn only: fewer than the approved curves this move type needs to block)" };
    j.message = format!("{name}: ❌ {}.{fix}{warn}", j.problems.join("; "));
    j
}

/// Counts of approved / close / off moves and of blocking ones.
pub fn summary(moves: &[Judged]) -> (usize, usize, usize, usize) {
    let n = |v: Verdict| moves.iter().filter(|m| m.verdict == v).count();
    (n(Verdict::Approved), n(Verdict::Close), n(Verdict::Off), moves.iter().filter(|m| m.verdict == Verdict::Off && m.blocking).count())
}

/// The curves used in `comps` that no profile curve matches, grouped by shape: each group's
/// representative shape, its move types, how often it is used and a few examples
/// (`comp › layer › path @ s`).
pub struct Found {
    pub ease: (Side, Side),
    pub kinds: Vec<MoveType>,
    pub uses: usize,
    pub examples: Vec<String>,
    pub nearest: Option<(String, f64)>,
}

pub fn unmatched_curves(profile: &Profile, comps: &[(&str, EvalCtx)], max_groups: usize) -> Vec<Found> {
    let mut groups: Vec<Found> = vec![];
    for (cname, ctx) in comps {
        let (moves, _) = check(profile, ctx, None, f64::MIN, f64::MAX, false);
        for mv in moves {
            let Some(ease) = mv.ease else { continue };
            if mv.frames < 2.5 || mv.problems.iter().any(|p| p.starts_with("linear") || p.starts_with("baked")) {
                continue;
            }
            let secs = mv.t1 - mv.t0;
            let known = profile.curves.iter().map(|c| (c, distance(ease, c.at(secs)))).min_by(|a, b| a.1.total_cmp(&b.1));
            if known.is_some_and(|(_, d)| d <= profile.rules.match_within) {
                continue;
            }
            let ex = format!("{cname} › {} › {} @ {:.2} s", mv.layer_name, mv.path, mv.t0);
            let at = groups.iter().position(|g| distance(g.ease, ease) <= profile.rules.match_within);
            let room = groups.len() < max_groups;
            match at.and_then(|k| groups.get_mut(k)) {
                Some(g) => {
                    g.uses += 1;
                    if !g.kinds.contains(&mv.kind) {
                        g.kinds.push(mv.kind);
                    }
                    if g.examples.len() < 4 {
                        g.examples.push(ex);
                    }
                }
                None if room => groups.push(Found {
                    ease,
                    kinds: vec![mv.kind],
                    uses: 1,
                    examples: vec![ex],
                    nearest: known.map(|(c, d)| (c.name.clone(), (d * 10.0).round() / 10.0)),
                }),
                None => {}
            }
        }
    }
    groups.sort_by_key(|g| std::cmp::Reverse(g.uses));
    groups
}
