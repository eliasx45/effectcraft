//! One test binary for the effectcraft-ui-egui integration tests: every `tests/*.rs` file is a
//! module here, so the app is linked once instead of once per file (`autotests = false`).
//!
//! - Run one file's tests: `cargo test -p effectcraft-ui-egui --test ui ui_viewer::` (a name filter).
//! - A new `tests/x.rs` needs its two lines below (`#[path = "../x.rs"] mod x;`); cargo ignores it
//!   otherwise, so `every_test_file_is_a_module` fails until it has them.
//! - Tests of different files share one process and run in parallel.
//!   A test that creates a GPU device holds [`gpu_lock`]. `-- --ignored` runs the opt-in snapshots.

/// One GPU device at a time in this binary: wgpu's OpenGL backend (Mesa llvmpipe on FreeBSD,
/// Linux without Vulkan) panics or crashes when tests create and drive devices concurrently.
/// Tests that run by default and create a device (`Gpu::headless*`, a `.wgpu()` harness,
/// `Harness::render`) hold this guard.
pub fn gpu_lock() -> std::sync::MutexGuard<'static, ()> {
    static GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GPU.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn every_test_file_is_a_module() {
    let main = include_str!("main.rs");
    let missing: Vec<String> = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/tests"))
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|f| f.ends_with(".rs") && !main.contains(&format!("#[path = \"../{f}\"]")))
        .collect();
    assert!(missing.is_empty(), "not compiled: declare each in tests/ui/main.rs (`#[path = \"../x.rs\"] mod x;`): {missing:?}");
}

#[path = "../render_queue_snapshot.rs"]
mod render_queue_snapshot;
#[path = "../shared_docking_ui.rs"]
mod shared_docking_ui;
#[path = "../ui_3d.rs"]
mod ui_3d;
#[path = "../ui_agent_audit.rs"]
mod ui_agent_audit;
#[path = "../ui_angles.rs"]
mod ui_angles;
#[path = "../ui_audio_scrub.rs"]
mod ui_audio_scrub;
#[path = "../ui_azerty_shortcuts.rs"]
mod ui_azerty_shortcuts;
#[path = "../ui_blank_context_menus.rs"]
mod ui_blank_context_menus;
#[path = "../ui_camera_tracker.rs"]
mod ui_camera_tracker;
#[path = "../ui_character_eyedropper.rs"]
mod ui_character_eyedropper;
#[path = "../ui_color_hex.rs"]
mod ui_color_hex;
#[path = "../ui_comp_presets.rs"]
mod ui_comp_presets;
#[path = "../ui_control_channel.rs"]
mod ui_control_channel;
#[path = "../ui_control_modifiers.rs"]
mod ui_control_modifiers;
#[path = "../ui_dock_resize.rs"]
mod ui_dock_resize;
#[path = "../ui_drag_drop.rs"]
mod ui_drag_drop;
#[path = "../ui_effect_controls.rs"]
mod ui_effect_controls;
#[path = "../ui_fingerprints.rs"]
mod ui_fingerprints;
#[path = "../ui_float_windows.rs"]
mod ui_float_windows;
#[path = "../ui_font_search.rs"]
mod ui_font_search;
#[path = "../ui_frame_step.rs"]
mod ui_frame_step;
#[path = "../ui_fx_console.rs"]
mod ui_fx_console;
#[path = "../ui_fx_editors.rs"]
mod ui_fx_editors;
#[path = "../ui_gpu.rs"]
mod ui_gpu;
#[path = "../ui_gradient_editor.rs"]
mod ui_gradient_editor;
#[path = "../ui_gradient_handles.rs"]
mod ui_gradient_handles;
#[path = "../ui_gradient_value.rs"]
mod ui_gradient_value;
#[path = "../ui_graph_fit.rs"]
mod ui_graph_fit;
#[path = "../ui_graph_handles.rs"]
mod ui_graph_handles;
#[path = "../ui_inline_edit.rs"]
mod ui_inline_edit;
#[path = "../ui_keyframes.rs"]
mod ui_keyframes;
#[path = "../ui_large_project.rs"]
mod ui_large_project;
#[path = "../ui_layer_panel_zoom.rs"]
mod ui_layer_panel_zoom;
#[path = "../ui_live_preview.rs"]
mod ui_live_preview;
#[path = "../ui_local_assets.rs"]
mod ui_local_assets;
#[path = "../ui_locked_color.rs"]
mod ui_locked_color;
#[path = "../ui_menus.rs"]
mod ui_menus;
#[path = "../ui_panel_focus.rs"]
mod ui_panel_focus;
#[path = "../ui_panel_menu.rs"]
mod ui_panel_menu;
#[path = "../ui_panel_windows.rs"]
mod ui_panel_windows;
#[path = "../ui_panels.rs"]
mod ui_panels;
#[path = "../ui_parameter_spacing.rs"]
mod ui_parameter_spacing;
#[path = "../ui_playback_cache_edge.rs"]
mod ui_playback_cache_edge;
#[path = "../ui_preview_align.rs"]
mod ui_preview_align;
#[path = "../ui_project_delete.rs"]
mod ui_project_delete;
#[path = "../ui_project_keyboard.rs"]
mod ui_project_keyboard;
#[path = "../ui_project_rows.rs"]
mod ui_project_rows;
#[path = "../ui_properties.rs"]
mod ui_properties;
#[path = "../ui_ram_preview.rs"]
mod ui_ram_preview;
#[path = "../ui_render_queue.rs"]
mod ui_render_queue;
#[path = "../ui_render_time.rs"]
mod ui_render_time;
#[path = "../ui_script_console.rs"]
mod ui_script_console;
#[path = "../ui_scriptui.rs"]
mod ui_scriptui;
#[path = "../ui_scrolling.rs"]
mod ui_scrolling;
#[path = "../ui_scrub_expression.rs"]
mod ui_scrub_expression;
#[path = "../ui_settings_info.rs"]
mod ui_settings_info;
#[path = "../ui_sharp_zoom.rs"]
mod ui_sharp_zoom;
#[path = "../ui_smart_freeze.rs"]
mod ui_smart_freeze;
#[path = "../ui_solo_small_tabs.rs"]
mod ui_solo_small_tabs;
#[path = "../ui_sound_preview.rs"]
mod ui_sound_preview;
#[path = "../ui_source_icons.rs"]
mod ui_source_icons;
#[path = "../ui_tabs.rs"]
mod ui_tabs;
#[path = "../ui_tail.rs"]
mod ui_tail;
#[path = "../ui_text_edit.rs"]
mod ui_text_edit;
#[path = "../ui_theme_favorites.rs"]
mod ui_theme_favorites;
#[path = "../ui_time_zoom_controls.rs"]
mod ui_time_zoom_controls;
#[path = "../ui_timeline_columns.rs"]
mod ui_timeline_columns;
#[path = "../ui_timeline_shapes.rs"]
mod ui_timeline_shapes;
#[path = "../ui_timeline_switches.rs"]
mod ui_timeline_switches;
#[path = "../ui_timeline_trim.rs"]
mod ui_timeline_trim;
#[path = "../ui_timeline_zoom.rs"]
mod ui_timeline_zoom;
#[path = "../ui_toolbar_gradient.rs"]
mod ui_toolbar_gradient;
#[path = "../ui_tracker.rs"]
mod ui_tracker;
#[path = "../ui_unreadable_footage.rs"]
mod ui_unreadable_footage;
#[path = "../ui_viewer.rs"]
mod ui_viewer;
#[path = "../ui_viewers.rs"]
mod ui_viewers;
#[path = "../ui_visual_review.rs"]
mod ui_visual_review;
#[path = "../ui_web_gpu_storage.rs"]
mod ui_web_gpu_storage;
#[path = "../ui_web_job_gpu.rs"]
mod ui_web_job_gpu;
#[path = "../ui_web_offload.rs"]
mod ui_web_offload;
#[path = "../ui_workflow_selection.rs"]
mod ui_workflow_selection;
#[path = "../ui_workspace_focus.rs"]
mod ui_workspace_focus;
#[path = "../viewer_geometry.rs"]
mod viewer_geometry;
#[path = "../viewer_passive_display.rs"]
mod viewer_passive_display;

#[path = "../ui_effect_names.rs"]
mod ui_effect_names;

#[path = "../ui_form_fit.rs"]
mod ui_form_fit;

#[path = "../ui_group_rename.rs"]
mod ui_group_rename;

#[path = "../ui_object_matte.rs"]
mod ui_object_matte;

#[path = "../ui_presets_folder.rs"]
mod ui_presets_folder;

#[path = "../ui_quick_offset.rs"]
mod ui_quick_offset;

#[path = "../ui_video_preview_off.rs"]
mod ui_video_preview_off;

#[path = "../ui_effect_drop_highlight.rs"]
mod ui_effect_drop_highlight;
#[path = "../ui_media_browser_cache.rs"]
mod ui_media_browser_cache;
#[path = "../ui_media_browser_copy_path.rs"]
mod ui_media_browser_copy_path;
#[path = "../ui_timeline_effect_label.rs"]
mod ui_timeline_effect_label;
#[path = "../ui_viewer_trackpad.rs"]
mod ui_viewer_trackpad;
