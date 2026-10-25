//! Synthetic packaging tests; no media decoding is needed.
use crate::{FsServices, Services, Session};
use effectcraft_project::{Footage, FootageKind, ItemKind, Project, Proxy};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let p = std::env::temp_dir().join(format!("effectcraft-collect-{}-{stamp}-{serial}", std::process::id()));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn media(path: &Path) -> Footage {
    Footage { path: path.to_string_lossy().into_owned(), kind: FootageKind::Still, ..Default::default() }
}
fn add(s: &mut Session, name: &str, f: Footage) -> effectcraft_project::ItemId {
    Arc::make_mut(&mut s.project).add_item(name, Default::default(), None, ItemKind::Footage(f))
}

#[test]
fn collect_package_relocates_sequences_and_both_proxy_kinds_without_changing_source() {
    let dir = Temp::new();
    let source = dir.0.join("source");
    std::fs::create_dir(&source).unwrap();
    for (name, bytes) in [("frame01.png", b"one".as_slice()), ("frame02.png", b"two".as_slice()), ("proxy.mov", b"proxy".as_slice())] {
        std::fs::write(source.join(name), bytes).unwrap();
    }
    let mut s = Session::default();
    s.execute("comp.new", json!({"name":"Main", "width":64, "height":64})).unwrap();
    let comp = s.active_comp_id().unwrap();
    let mut sequence = media(&source.join("frame01.png"));
    sequence.kind = FootageKind::Sequence;
    sequence.sequence = vec![sequence.path.clone(), source.join("frame02.png").to_string_lossy().into_owned()];
    let frames = add(&mut s, "Frames", sequence);
    let duplicate = add(&mut s, "Same first frame", media(&source.join("frame01.png")));
    let proxy = Proxy { footage: media(&source.join("proxy.mov")), enabled: true, auto: false };
    let project = Arc::make_mut(&mut s.project);
    project.item_mut(frames).unwrap().proxy = Some(Box::new(proxy.clone()));
    project.item_mut(comp).unwrap().proxy = Some(Box::new(proxy));
    s.path = Some(source.join("Source.ecproj").to_string_lossy().into_owned());
    let before = s.project.to_json();
    let old_path = s.path.clone();
    let dirty = s.is_dirty();
    let undo = s.history.undo.len();
    let destination = dir.0.join("package");
    let result = s.execute("file.collectFiles", json!({"folder": destination})).unwrap();
    assert_eq!(result["copied"], 3, "shared sources are copied once");
    assert_eq!(result["errors"], json!([]));
    assert!(result["warnings"].is_array());
    assert_eq!(s.project.to_json(), before);
    assert_eq!(s.path, old_path);
    assert_eq!(s.is_dirty(), dirty);
    assert_eq!(s.history.undo.len(), undo);
    let project_path = PathBuf::from(result["project"].as_str().unwrap());
    let stored = Project::from_json(&std::fs::read_to_string(&project_path).unwrap()).unwrap();
    for item in stored.items.values() {
        for f in match &item.kind {
            ItemKind::Footage(f) => Some(f),
            _ => None,
        }
        .into_iter()
        .chain(item.proxy.as_ref().map(|p| &p.footage))
        {
            assert!(Path::new(&f.path).is_relative());
            assert!(f.sequence.iter().all(|p| Path::new(p).is_relative()));
        }
    }
    let moved = dir.0.join("recipient");
    std::fs::rename(&destination, &moved).unwrap();
    std::fs::remove_dir_all(&source).unwrap();
    let mut opened = Session::default();
    crate::commands::open_for_tests(&mut opened, &json!({"path": moved.join(project_path.file_name().unwrap())})).unwrap();
    for id in [frames, duplicate, comp] {
        let item = opened.project.item(id).unwrap();
        for f in match &item.kind {
            ItemKind::Footage(f) => Some(f),
            _ => None,
        }
        .into_iter()
        .chain(item.proxy.as_ref().map(|p| &p.footage))
        {
            assert!(Path::new(&f.path).starts_with(&moved), "{}", f.path);
            assert!(Path::new(&f.path).is_file());
            for frame in &f.sequence {
                assert!(Path::new(frame).starts_with(&moved));
                assert!(Path::new(frame).is_file());
            }
        }
    }
    let item = opened.project.item(frames).unwrap();
    let ItemKind::Footage(f) = &item.kind else { panic!("sequence") };
    assert_eq!(std::fs::read(&f.sequence[0]).unwrap(), b"one");
    assert_eq!(std::fs::read(&f.sequence[1]).unwrap(), b"two");
}

struct FailingServices {
    project_write: bool,
}
impl Services for FailingServices {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        FsServices.read_file(path)
    }
    fn write_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        if self.project_write && path.ends_with(".ecproj") {
            return Err(std::io::Error::other("synthetic project write failure"));
        }
        FsServices.write_file(path, data)
    }
    fn copy_file(&self, source: &str, destination: &str) -> std::io::Result<()> {
        if !self.project_write {
            std::fs::write(destination, b"partial copy")?;
            return Err(std::io::Error::other("synthetic copy failure"));
        }
        FsServices.copy_file(source, destination)
    }
    fn supports_native_collection(&self) -> bool {
        true
    }
}

#[test]
fn collection_failures_remove_incomplete_output_and_preserve_the_source() {
    let dir = Temp::new();
    let source = dir.0.join("source.png");
    std::fs::write(&source, b"source").unwrap();
    for project_write in [false, true] {
        let mut s = Session::default();
        add(&mut s, "Image", media(&source));
        s.services = Arc::new(FailingServices { project_write });
        let before = s.project.to_json();
        let destination = dir.0.join(if project_write { "write-failure" } else { "copy-failure" });
        assert!(s.execute("file.collectFiles", json!({"folder": destination})).is_err());
        assert!(!destination.exists(), "partial output is removed");
        assert_eq!(s.project.to_json(), before);
        assert_eq!(std::fs::read(&source).unwrap(), b"source");
    }
    let mut s = Session::default();
    add(&mut s, "Missing", media(&dir.0.join("missing.png")));
    let destination = dir.0.join("missing-output");
    assert!(s.execute("file.collectFiles", json!({"folder": destination})).is_err());
    assert!(!destination.exists());
}

#[test]
fn collection_refuses_existing_empty_or_populated_destinations() {
    let dir = Temp::new();
    let mut s = Session::default();
    s.execute("comp.new", json!({"name":"Main"})).unwrap();
    let destination = dir.0.join("existing");
    std::fs::create_dir(&destination).unwrap();
    assert!(s.execute("file.collectFiles", json!({"folder":destination})).is_err());
    assert!(destination.is_dir());
    std::fs::write(destination.join("keep"), b"untouched").unwrap();
    assert!(s.execute("file.collectFiles", json!({"folder":destination})).is_err());
    assert_eq!(std::fs::read(destination.join("keep")).unwrap(), b"untouched");
}

#[cfg(unix)]
#[test]
fn collection_refuses_destination_symlinks_without_touching_their_targets() {
    let dir = Temp::new();
    let target = dir.0.join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("keep"), b"untouched").unwrap();
    let link = dir.0.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let mut s = Session::default();
    s.execute("comp.new", json!({"name":"Main"})).unwrap();
    assert!(s.execute("file.collectFiles", json!({"folder":link})).is_err());
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read(target.join("keep")).unwrap(), b"untouched");
}

#[test]
fn collection_reports_unbundled_text_fonts_in_result_and_saved_report() {
    let dir = Temp::new();
    let mut s = Session::default();
    s.execute("comp.new", json!({"name":"Main"})).unwrap();
    s.execute("layer.newText", json!({"text":"Synthetic font dependency"})).unwrap();
    let result = s.execute("file.collectFiles", json!({"folder":dir.0.join("package")})).unwrap();
    assert!(!result["warnings"].as_array().unwrap().is_empty());
    let report: serde_json::Value = serde_json::from_slice(&std::fs::read(result["report"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(report["warnings"], result["warnings"]);
    assert_eq!(result["copied"], 0);
}

struct UnsupportedServices;
impl Services for UnsupportedServices {
    fn read_file(&self, _: &str) -> std::io::Result<Vec<u8>> {
        panic!("unsupported collection must not read")
    }
    fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
        panic!("unsupported collection must not write/download")
    }
}
#[test]
fn collection_rejects_virtual_services_before_creating_native_files() {
    let dir = Temp::new();
    let mut s = Session::default();
    s.execute("comp.new", json!({"name":"Main"})).unwrap();
    s.services = Arc::new(UnsupportedServices);
    let destination = dir.0.join("unsupported");
    assert!(s.execute("file.collectFiles", json!({"folder":destination})).is_err());
    assert!(!destination.exists());
}

#[test]
fn collection_rejects_nonfinite_project_before_creating_output() {
    let dir = Temp::new();
    let mut s = Session::default();
    s.execute("comp.new", json!({"name":"Main"})).unwrap();
    let comp = s.active_comp_id().unwrap();
    Arc::make_mut(&mut s.project).comp_mut(comp).unwrap().pixel_aspect = f64::NAN;
    let destination = dir.0.join("invalid");
    let error = s.execute("file.collectFiles", json!({"folder":destination})).unwrap_err();
    assert!(error.to_string().contains("can't be saved"), "{error}");
    assert!(!destination.exists());
    assert!(s.project.comp(comp).unwrap().pixel_aspect.is_nan());
}

#[test]
fn collection_preserves_file_version_and_open_composition_metadata() {
    let dir = Temp::new();
    let mut s = Session::default();
    s.execute("comp.new", json!({"name":"First"})).unwrap();
    s.set_time(effectcraft_time::Tick(17));
    s.execute("comp.new", json!({"name":"Second"})).unwrap();
    s.set_time(effectcraft_time::Tick(29));
    let open = s.open_comps_to_save();
    assert!(open.is_some());
    let result = s.execute("file.collectFiles", json!({"folder":dir.0.join("package")})).unwrap();
    let text = std::fs::read_to_string(result["project"].as_str().unwrap()).unwrap();
    let meta = effectcraft_project::file_meta(&text);
    assert_eq!(meta.saved_by.as_deref(), Some(effectcraft_project::APP_VERSION));
    assert_eq!(meta.open_comps, open);
}

#[test]
fn collection_keeps_embedded_data_without_requiring_the_old_source() {
    let dir = Temp::new();
    let mut s = Session::default();
    let data = r#"{"value":42}"#;
    let id = add(
        &mut s,
        "Data",
        Footage {
            path: dir.0.join("deleted.json").to_string_lossy().into_owned(),
            kind: FootageKind::Data,
            data: Some(data.into()),
            missing: true,
            ..Default::default()
        },
    );
    let original = s.project.to_json();
    let result = s.execute("file.collectFiles", json!({"folder":dir.0.join("package")})).unwrap();
    assert_eq!(result["copied"], 0);
    assert_eq!(s.project.to_json(), original);
    let stored = Project::from_json(&std::fs::read_to_string(result["project"].as_str().unwrap()).unwrap()).unwrap();
    let ItemKind::Footage(f) = &stored.item(id).unwrap().kind else { panic!("data") };
    assert_eq!(f.data.as_deref(), Some(data));
    assert_eq!(f.data_format(), Some("json"));
    assert!(!f.missing);
    let packaged = Path::new(result["project"].as_str().unwrap()).parent().unwrap().join(&f.path);
    assert_eq!(std::fs::read_to_string(packaged).unwrap(), data);
}

#[test]
fn collection_assigns_portable_unique_names_to_duplicate_and_case_colliding_basenames() {
    let dir = Temp::new();
    let mut s = Session::default();
    let mut ids = Vec::new();
    for (folder, name) in [("one", "same.png"), ("two", "same.png"), ("case", "Frame.png"), ("case", "frame.png")] {
        let parent = dir.0.join(folder);
        std::fs::create_dir_all(&parent).unwrap();
        let source = parent.join(name);
        // Identical bytes keep this test valid on case-insensitive hosts too.
        std::fs::write(&source, b"synthetic").unwrap();
        ids.push(add(&mut s, name, media(&source)));
    }
    let result = s.execute("file.collectFiles", json!({"folder":dir.0.join("package")})).unwrap();
    assert_eq!(result["copied"], 4);
    let path = Path::new(result["project"].as_str().unwrap());
    let stored = Project::from_json(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut names = std::collections::BTreeSet::new();
    for id in ids {
        let ItemKind::Footage(f) = &stored.item(id).unwrap().kind else { panic!("footage") };
        assert!(names.insert(f.path.to_lowercase()), "case-insensitive collision: {}", f.path);
        assert!(Path::new(&f.path).is_relative());
        assert_eq!(std::fs::read(path.parent().unwrap().join(&f.path)).unwrap(), b"synthetic");
    }
}

#[test]
fn collection_preserves_and_reports_placeholders() {
    let dir = Temp::new();
    let mut s = Session::default();
    let id = s.execute("file.importPlaceholder", json!({"name":"Offline shot"})).unwrap()["item"].as_u64().unwrap();
    let result = s.execute("file.collectFiles", json!({"folder":dir.0.join("package")})).unwrap();
    let stored = Project::from_json(&std::fs::read_to_string(result["project"].as_str().unwrap()).unwrap()).unwrap();
    let ItemKind::Footage(f) = &stored.item(effectcraft_project::ItemId(id)).unwrap().kind else { panic!("placeholder") };
    assert!(f.missing);
    assert!(f.path.is_empty());
    assert!(result["warnings"].as_array().unwrap().iter().any(|w| w["kind"] == "placeholder"));
}

#[test]
fn collection_keeps_embedded_csv_and_tsv_formats_when_reopened() {
    let dir = Temp::new();
    for (extension, data, format) in [("csv", "name,value\nA,42", "csv"), ("tsv", "name\tvalue\nA\t42", "tsv"), ("tab", "name\tvalue\nA\t42", "tsv")] {
        let mut s = Session::default();
        let id = add(
            &mut s,
            "Data",
            Footage {
                path: dir.0.join(format!("missing.{extension}")).to_string_lossy().into_owned(),
                kind: FootageKind::Data,
                data: Some(data.into()),
                ..Default::default()
            },
        );
        let result = s.execute("file.collectFiles", json!({"folder":dir.0.join(extension)})).unwrap();
        assert_eq!(result["embedded"], 1);
        let mut opened = Session::default();
        crate::commands::open_for_tests(&mut opened, &json!({"path":result["project"]})).unwrap();
        let ItemKind::Footage(f) = &opened.project.item(id).unwrap().kind else { panic!("data") };
        assert_eq!(f.data_format(), Some(format));
        assert_eq!(f.data.as_deref(), Some(data));
        assert_eq!(std::fs::read_to_string(&f.path).unwrap(), data);
        assert!(!Path::new(&f.path).file_name().unwrap().to_string_lossy().starts_with("missing"));
    }
}
