//! One test binary for the effectcraft-export integration tests: every `tests/*.rs` file is a
//! module here, so the encoders is linked once instead of once per file (`autotests = false`).
//!
//! - Run one file's tests: `cargo test -p effectcraft-export --test export roundtrip::` (a name filter).
//! - A new `tests/x.rs` needs its two lines below (`#[path = "../x.rs"] mod x;`); cargo ignores it
//!   otherwise, so `every_test_file_is_a_module` fails until it has them.
//! - Tests of different files share one process and run in parallel.

#[test]
fn every_test_file_is_a_module() {
    let main = include_str!("main.rs");
    let missing: Vec<String> = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/tests"))
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|f| f.ends_with(".rs") && !main.contains(&format!("#[path = \"../{f}\"]")))
        .collect();
    assert!(missing.is_empty(), "not compiled: declare each in tests/export/main.rs (`#[path = \"../x.rs\"] mod x;`): {missing:?}");
}

#[path = "../audio_sample_positions.rs"]
mod audio_sample_positions;
#[path = "../h264_groups.rs"]
mod h264_groups;
#[path = "../hevc_av1_roundtrip.rs"]
mod hevc_av1_roundtrip;
#[path = "../mov_audio_sample_positions.rs"]
mod mov_audio_sample_positions;
#[path = "../native_audio_sample_positions.rs"]
mod native_audio_sample_positions;
#[path = "../opus_roundtrip.rs"]
mod opus_roundtrip;
#[path = "../output_modules.rs"]
mod output_modules;
#[path = "../pipelined.rs"]
mod pipelined;
#[path = "../render_settings.rs"]
mod render_settings;
#[path = "../roundtrip.rs"]
mod roundtrip;
#[path = "../uncompressed_mov.rs"]
mod uncompressed_mov;
#[path = "../vp9_roundtrip.rs"]
mod vp9_roundtrip;
