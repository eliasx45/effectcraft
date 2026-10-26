//! One test binary for the effectcraft-host integration tests: every `tests/*.rs` file is a
//! module here, so the fully wired host is linked once instead of once per file (`autotests = false`).
//!
//! - Run one file's tests: `cargo test -p effectcraft-host --test host repros::` (a name filter).
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
    assert!(missing.is_empty(), "not compiled: declare each in tests/host/main.rs (`#[path = \"../x.rs\"] mod x;`): {missing:?}");
}

#[path = "../background_presets.rs"]
mod background_presets;
#[path = "../comp_tools.rs"]
mod comp_tools;
#[path = "../determinism.rs"]
mod determinism;
#[path = "../exam_tasks.rs"]
mod exam_tasks;
#[path = "../exr_layers.rs"]
mod exr_layers;
#[path = "../figma_import.rs"]
mod figma_import;
#[path = "../footage_reload.rs"]
mod footage_reload;
#[path = "../gif_animates.rs"]
mod gif_animates;
#[path = "../plugins.rs"]
mod plugins;
#[path = "../relative_media.rs"]
mod relative_media;
#[path = "../relink.rs"]
mod relink;
#[path = "../repros.rs"]
mod repros;
#[path = "../timeline_interop.rs"]
mod timeline_interop;
#[path = "../vector_import.rs"]
mod vector_import;
#[path = "../webm_metadata.rs"]
mod webm_metadata;
