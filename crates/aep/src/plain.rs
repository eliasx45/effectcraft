//! Effects imported from After Effects before EffectCraft knew them, saved as plain property
//! groups (`ADBE Bevel Alpha` with `ADBE Bevel Alpha-0001`… children) that render nothing.
//! [`native_effect`] rebuilds such a group as the native effect it names, with the values,
//! keyframes and expressions the import kept, converted the way the import converts them now.

use effectcraft_keyframe::Value;
use effectcraft_project::{GroupKind, Node, ParamUi, PropGroup, Property, build::Ids};

use crate::effects;

/// Deeper than any effect nests (Compositing Options ▸ Masks ▸ …).
const MAX_DEPTH: usize = 16;

/// Whether `g` is a plain group (under a layer's `effects`) that [`native_effect`] can turn into
/// a native effect. Cheap: no copy.
pub fn is_convertible(g: &PropGroup) -> bool {
    matches!(g.kind, GroupKind::Plain) && effects::resolve(&g.match_id).is_some_and(|(spec, _)| !needs_import_context(spec))
}

/// Layer selectors need the comp's layer indices, which the import resolves and a saved group no
/// longer has. (Points are carried as the import carries them: in layer pixels.)
fn needs_import_context(spec: &effectcraft_effects::EffectSpec) -> bool {
    spec.params.iter().any(|p| matches!(p.ui, ParamUi::Layer))
}

/// The native effect a plain effect group `g` stands for, or None when its match name maps to no
/// native effect (third-party plug-ins, pseudo effects) or a stored value doesn't fit the native
/// parameter it maps to (the parameters don't line up). The group keeps its uid, name and
/// enabled state; mapped properties keep their uids (expressions referring to them still work).
/// Parameters without a native counterpart stay on the effect as they were, as the import keeps
/// them; Compositing Options at their defaults are dropped as the import drops them.
pub fn native_effect(g: &PropGroup, ids: &mut Ids, layer_size: [f64; 2]) -> Option<PropGroup> {
    if !matches!(g.kind, GroupKind::Plain) {
        return None;
    }
    let (spec, reviewed) = effects::resolve(&g.match_id)?;
    if needs_import_context(spec) {
        return None;
    }
    let mut out = effectcraft_effects::instantiate(spec, ids, &g.name, layer_size);
    out.uid = g.uid;
    out.enabled = g.enabled;
    let mut extra = Vec::new();
    carry(g, &g.match_id, spec, reviewed, &mut out, &mut extra, 0)?;
    out.children.extend(extra);
    Some(out)
}

fn carry(
    g: &PropGroup,
    ae_effect: &str,
    spec: &effectcraft_effects::EffectSpec,
    reviewed: bool,
    out: &mut PropGroup,
    extra: &mut Vec<Node>,
    depth: usize,
) -> Option<()> {
    if depth >= MAX_DEPTH {
        return None;
    }
    for child in &g.children {
        let old = match child {
            Node::Group(sub) => {
                carry(sub, ae_effect, spec, reviewed, out, extra, depth + 1)?;
                continue;
            }
            Node::Prop(p) => p,
        };
        if default_builtin(old) {
            continue;
        }
        let Some(path) = effects::native_param(spec, reviewed, ae_effect, &old.match_id) else {
            extra.push(Node::Prop(old.clone()));
            continue;
        };
        let target = out.prop_mut(path)?;
        let template = target.value.clone();
        let scale = crate::param_scale(spec.id, path);
        let remap = crate::param_remap(spec.id, path);
        let convert = |v: &Value| -> Option<Value> {
            let v = match (v, remap) {
                (Value::Scalar(x), Some(f)) => Value::Scalar(f(*x)),
                _ => v.clone(),
            };
            let v = crate::adapt_value(crate::scale_value(v, scale), &template);
            (std::mem::discriminant(&v) == std::mem::discriminant(&template)).then_some(v)
        };
        // An unauthored point in AE's raw fixed-point units keeps the native default, as on import
        // (`unauthored_raw_default_point`).
        if old.keys.is_empty() && old.expr.is_none() && matches!(&old.value, Value::Vec2(v) if v.iter().any(|c| c.abs() >= 10_000.0)) {
            continue;
        }
        let value = convert(&old.value)?;
        let mut keys = old.keys.clone();
        for k in &mut keys {
            k.value = convert(&k.value)?;
        }
        target.value = value;
        target.keys = keys;
        target.expr = old.expr.clone();
        target.uid = old.uid;
    }
    Some(())
}

/// A Compositing Options property at the value the import leaves out (see
/// `import_native_effect_params`).
fn default_builtin(p: &Property) -> bool {
    let default = match (p.match_id.as_str(), &p.value) {
        ("ADBE Effect Mask Opacity", Value::Scalar(v)) => *v == 100.0,
        ("ADBE Force CPU GPU", Value::Scalar(v)) => *v == 1.0,
        _ => false,
    };
    default && p.keys.is_empty() && p.expr.is_none()
}

#[cfg(test)]
mod tests {
    use effectcraft_keyframe::Keyframe;
    use effectcraft_project::Expression;
    use effectcraft_time::Tick;

    use super::*;

    fn prop(uid: u64, m: &str, v: Value) -> Node {
        Node::Prop(Property::new(uid, m, m, v))
    }

    fn plain(uid: u64, m: &str, children: Vec<Node>) -> PropGroup {
        let mut g = PropGroup::new(uid, m, m);
        g.children = children;
        g
    }

    fn compositing(uid: u64, opacity: f64) -> Node {
        Node::Group(plain(
            uid,
            "ADBE Effect Built In Params",
            vec![
                Node::Group(plain(uid + 1, "ADBE Effect Mask Parade", vec![])),
                prop(uid + 2, "ADBE Effect Mask Opacity", Value::Scalar(opacity)),
                prop(uid + 3, "ADBE Force CPU GPU", Value::Scalar(1.0)),
            ],
        ))
    }

    /// 20-08's Bevel Alpha and Scatter, as an import that didn't know them saved them.
    #[test]
    fn plain_bevel_alpha_and_scatter_become_native_effects() {
        let mut next = 1000;
        let mut angle = Property::new(11, "ADBE Bevel Alpha-0002", "Light Angle", Value::Scalar(-35.0));
        angle.keys = vec![Keyframe::new(Tick::ZERO, Value::Scalar(-35.0)), Keyframe::new(Tick::from_seconds_f64(1.0), Value::Scalar(90.0))];
        angle.expr = Some(Expression { text: "value + 1".into(), enabled: true });
        let mut bevel = plain(
            10,
            "ADBE Bevel Alpha",
            vec![
                prop(12, "ADBE Bevel Alpha-0001", Value::Scalar(3.0)),
                Node::Prop(angle),
                prop(13, "ADBE Bevel Alpha-0003", Value::Color([1.0, 0.5, 0.25, 1.0])),
                prop(14, "ADBE Bevel Alpha-0004", Value::Scalar(0.399993896484375)),
                compositing(15, 100.0),
            ],
        );
        bevel.name = "Bevel Alpha".into();
        bevel.enabled = false;
        let g = native_effect(&bevel, &mut Ids(&mut next), [100.0, 100.0]).unwrap();
        assert_eq!(g.kind, GroupKind::Effect { effect: "ec.perspective.bevelalpha".into() });
        assert_eq!((g.uid, g.name.as_str(), g.enabled), (10, "Bevel Alpha", false));
        assert_eq!(g.prop("edgeThickness").unwrap().value, Value::Scalar(3.0));
        let a = g.prop("lightAngle").unwrap();
        assert_eq!((a.uid, &a.value), (11, &Value::Scalar(-35.0)));
        assert_eq!(a.keys.iter().map(|k| k.value.clone()).collect::<Vec<_>>(), vec![Value::Scalar(-35.0), Value::Scalar(90.0)]);
        assert_eq!(a.expr.as_ref().map(|e| e.text.as_str()), Some("value + 1"));
        assert_eq!(g.prop("lightColor").unwrap().value, Value::Color([1.0, 0.5, 0.25, 1.0]));
        assert_eq!(g.prop("lightIntensity").unwrap().value, Value::Scalar(0.399993896484375));
        // Default Compositing Options are dropped, as the import drops them.
        assert_eq!(g.children.len(), 4);
        assert!(g.children.iter().all(|n| matches!(n, Node::Prop(p) if !p.match_id.starts_with("ADBE"))));

        let scatter = plain(
            20,
            "ADBE Scatter",
            vec![
                prop(21, "ADBE Scatter-0001", Value::Scalar(5.0)),
                prop(22, "ADBE Scatter-0002", Value::Scalar(3.0)),
                prop(23, "ADBE Scatter-0003", Value::Scalar(1.0)),
                compositing(24, 50.0),
            ],
        );
        let g = native_effect(&scatter, &mut Ids(&mut next), [100.0, 100.0]).unwrap();
        assert_eq!(g.kind, GroupKind::Effect { effect: "ec.stylize.scatter".into() });
        assert_eq!(g.prop("amount").unwrap().value, Value::Scalar(5.0));
        // AE popups count from 1 (3 = Vertical); checkboxes are 0 / 1.
        assert_eq!(g.prop("grain").unwrap().value, Value::Enum(2));
        assert_eq!(g.prop("randomizeEveryFrame").unwrap().value, Value::Bool(true));
        // A changed Effect Opacity stays on the effect, as the import keeps it.
        assert!(g.get("ADBE Effect Mask Opacity").is_some_and(|p| p.value == Value::Scalar(50.0)));
    }

    /// 20-08's Pre-comp 5 › Shape Layer 2: AE's Halo Alpha (-0005) has no native counterpart, so
    /// the parameters after it map through the reviewed table, not by index.
    #[test]
    fn plain_cc_light_burst_becomes_native_with_its_center() {
        let mut next = 1000;
        let mut burst = plain(
            1,
            "CC Light Burst 2.5",
            vec![
                prop(2, "CC Light Burst 2.5-0001", Value::Vec2([521.25, 415.5])),
                prop(3, "CC Light Burst 2.5-0002", Value::Scalar(120.0)),
                prop(4, "CC Light Burst 2.5-0003", Value::Scalar(50.0)),
                prop(5, "CC Light Burst 2.5-0004", Value::Scalar(2.0)),
                prop(6, "CC Light Burst 2.5-0005", Value::Scalar(0.0)),
                prop(7, "CC Light Burst 2.5-0006", Value::Scalar(1.0)),
                prop(8, "CC Light Burst 2.5-0007", Value::Color([1.0, 0.9, 0.25, 1.0])),
                compositing(9, 100.0),
            ],
        );
        burst.enabled = false;
        assert!(is_convertible(&burst));
        let g = native_effect(&burst, &mut Ids(&mut next), [1080.0, 1920.0]).unwrap();
        assert_eq!(g.kind, GroupKind::Effect { effect: "ec.generate.cclightburst".into() });
        assert!(!g.enabled);
        assert_eq!(g.prop("center").unwrap().value, Value::Vec2([521.25, 415.5]));
        assert_eq!(g.prop("intensity").unwrap().value, Value::Scalar(120.0));
        assert_eq!(g.prop("burst").unwrap().value, Value::Enum(1));
        assert_eq!(g.prop("setColor").unwrap().value, Value::Bool(true));
        assert_eq!(g.prop("color").unwrap().value, Value::Color([1.0, 0.9, 0.25, 1.0]));
        // Halo Alpha stays on the effect, unmapped, as the import keeps it.
        assert!(g.get("CC Light Burst 2.5-0005").is_some());
    }

    #[test]
    fn unknown_and_native_groups_stay_as_they_are() {
        let mut next = 1000;
        for m in ["PEDG2", "Pseudo/textevo", "", "ADBE Effect Built In Params"] {
            let g = plain(1, m, vec![prop(2, &format!("{m}-0001"), Value::Scalar(1.0))]);
            assert!(!is_convertible(&g), "{m}");
            assert!(native_effect(&g, &mut Ids(&mut next), [10.0, 10.0]).is_none(), "{m}");
        }
        // Already native.
        let mut g = plain(1, "ADBE Scatter", vec![]);
        g.kind = GroupKind::Effect { effect: "ec.stylize.scatter".into() };
        assert!(!is_convertible(&g));
        assert!(native_effect(&g, &mut Ids(&mut next), [10.0, 10.0]).is_none());
    }

    #[test]
    fn junk_never_crashes_and_mismatched_values_stay_plain() {
        let mut next = 1000;
        let ids = &mut Ids(&mut next);
        // A value that doesn't fit the parameter it maps to: the parameters don't line up.
        let g = plain(1, "ADBE Scatter", vec![prop(2, "ADBE Scatter-0001", Value::Color([1.0; 4]))]);
        assert!(native_effect(&g, ids, [10.0, 10.0]).is_none());
        let mut keyed = Property::new(2, "ADBE Scatter-0002", "Grain", Value::Scalar(1.0));
        keyed.keys = vec![Keyframe::new(Tick::ZERO, Value::Str("x".into()))];
        assert!(native_effect(&plain(1, "ADBE Scatter", vec![Node::Prop(keyed)]), ids, [10.0, 10.0]).is_none());
        // Odd numbers, names and values: kept as unmapped properties, never a panic.
        let g = plain(
            1,
            "adbe scatter",
            vec![
                prop(2, "ADBE Scatter-0000", Value::Scalar(1.0)),
                prop(3, "ADBE Scatter-9999", Value::Scalar(1.0)),
                prop(4, "ADBE Scatter-+001", Value::Scalar(1.0)),
                prop(5, "ADBE Scatter-", Value::Scalar(1.0)),
                prop(6, "-0001", Value::Scalar(1.0)),
                prop(7, "ADBE Scatter-0002", Value::Scalar(f64::MAX)),
                prop(8, "ADBE Scatter-0003", Value::Scalar(f64::NAN)),
                prop(9, "ADBE Scatter-0001", Value::Scalar(-1e300)),
            ],
        );
        let out = native_effect(&g, ids, [0.0, 0.0]).unwrap();
        assert_eq!(out.children.iter().filter(|n| matches!(n, Node::Prop(p) if p.match_id.contains('-'))).count(), 5);
        // Nesting deeper than any effect: left plain.
        let mut deep = plain(100, "x", vec![]);
        for i in 0..64 {
            deep = plain(101 + i, "x", vec![Node::Group(deep)]);
        }
        assert!(native_effect(&plain(1, "ADBE Scatter", vec![Node::Group(deep)]), ids, [10.0, 10.0]).is_none());
    }
}
