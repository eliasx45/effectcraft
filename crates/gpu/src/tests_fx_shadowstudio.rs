//! GPU Shadow Studio 2 / 3 (`fx_shadowstudio`) vs the CPU effect (the oracle): the defaults,
//! a hard cast (no softness) with the alpha threshold, eased opacity, Source Opacity, Shadow
//! Offset and a coloured, translucent shadow, few and many samples, in every direction, on
//! layers and adjustment layers, at full and half resolution, 8 and 32 bpc.

use effectcraft_keyframe::Value;

use crate::tests::{c, effect_case, n};

const V3: &str = "ec.perspective.shadowstudio3";
const V2: &str = "ec.perspective.shadowstudio2";

#[test]
fn shadow_studio_3_matches_cpu() {
    // Defaults (Shadow Length 200, Light Softness 20 % of Radius 200).
    effect_case(V3, &[]);
    // Short and soft.
    effect_case(V3, &[("PESS3-0004", n(24.0)), ("PESS3-0076", n(30.0)), ("PESS3-0075", n(20.0))]);
    // Hard cast to the right, opacity rising to the end, eased, threshold 0.25, half Source Opacity.
    effect_case(
        V3,
        &[
            ("PESS3-0002", n(90.0)),
            ("PESS3-0004", n(18.0)),
            ("PESS3-0075", n(0.0)),
            ("PESS3-0086", n(20.0)),
            ("PESS3-0087", n(80.0)),
            ("PESS3-0084", n(25.0)),
            ("PESS3-0078", n(0.25)),
            ("PESS3-0026", n(50.0)),
        ],
    );
    // Up and left, coloured and translucent, few samples, Shadow Offset 0.6, threshold 0.
    effect_case(
        V3,
        &[
            ("PESS3-0002", n(-60.0)),
            ("PESS3-0004", n(40.0)),
            ("PESS3-0075", n(5.0)),
            ("PESS3-0076", n(80.0)),
            ("PESS3-0006", Value::Color([0.2, 0.4, 0.9, 0.7])),
            ("PESS3-0022", n(6.0)),
            ("PESS3-0027", n(0.6)),
            ("PESS3-0038", n(70.0)),
            ("PESS3-0078", n(0.0)),
        ],
    );
    // Many samples (more than the length: one step per pixel), Shadow Offset 2, no source.
    effect_case(V3, &[("PESS3-0002", n(200.0)), ("PESS3-0004", n(15.0)), ("PESS3-0022", n(300.0)), ("PESS3-0027", n(2.0)), ("PESS3-0026", n(0.0))]);
    // Shadow Opacity 0: the source alone (padded).
    effect_case(V3, &[("PESS3-0004", n(12.0)), ("PESS3-0038", n(0.0))]);
    // Length 0: one step on the source itself.
    effect_case(V3, &[("PESS3-0004", n(0.0)), ("PESS3-0075", n(10.0)), ("PESS3-0006", c(0.8, 0.1, 0.1))]);
}

#[test]
fn shadow_studio_2_matches_cpu() {
    effect_case(V2, &[("PESS2-0004", n(30.0)), ("PESS2-0005", n(8.0))]);
    effect_case(
        V2,
        &[("PESS2-0002", n(300.0)), ("PESS2-0004", n(20.0)), ("PESS2-0005", n(0.0)), ("PESS2-0008", n(90.0)), ("PESS2-0009", n(10.0)), ("PESS2-0027", n(40.0))],
    );
}
