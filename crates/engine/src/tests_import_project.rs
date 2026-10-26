//! File ▸ Import ▸ EffectCraft Project: another project merges into the open one with fresh ids,
//! its references intact, in one undo step.

use std::collections::HashSet;

use effectcraft_project::{Footage, FootageKind, ItemId, ItemKind, LayerSource, Project};
use serde_json::json;

use crate::Session;

fn out_dir(name: &str) -> std::path::PathBuf {
    let d = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-out").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Every item, layer and property uid of `p` (they share one counter).
fn all_ids(p: &Project) -> Vec<u64> {
    let mut v = vec![];
    for (id, it) in &p.items {
        v.push(id.0);
        if let ItemKind::Comp(c) = &it.kind {
            for l in &c.layers {
                v.push(l.id.0);
                fn uids(g: &effectcraft_project::PropGroup, v: &mut Vec<u64>) {
                    v.push(g.uid);
                    for c in &g.children {
                        match c {
                            effectcraft_project::Node::Prop(p) => v.push(p.uid),
                            effectcraft_project::Node::Group(g) => uids(g, v),
                        }
                    }
                }
                uids(&l.props, &mut v);
            }
        }
    }
    v
}

/// A project with "Main" nesting "Inner" under a parented null, a data file next to it by a
/// relative path and a footage file that's gone. Saved as `<dir>/Library Card.ecproj`.
fn source_project(dir: &std::path::Path) -> String {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Inner", "width": 64, "height": 64, "frameRate": 30, "duration": 2})).unwrap();
    let inner = s.active_comp_id().unwrap();
    s.execute("layer.newSolid", json!({"name": "Fill", "color": "#ff0000"})).unwrap();
    s.execute("comp.new", json!({"name": "Main", "width": 64, "height": 64, "frameRate": 30, "duration": 2})).unwrap();
    let main = s.active_comp_id().unwrap();
    let nest = s.execute("layer.addItem", json!({"comp": main.0, "item": inner.0, "time": 0.0})).unwrap()["layer"].as_u64().unwrap();
    let rig = s.execute("layer.newNull", json!({"name": "Rig"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("layer.setParent", json!({"layers": [nest], "parent": rig})).unwrap();
    std::fs::write(dir.join("data.json"), b"{\"v\": 1}").unwrap();
    let p = std::sync::Arc::make_mut(&mut s.project);
    p.add_item("data.json", Default::default(), None, ItemKind::Footage(Footage { path: "data.json".into(), kind: FootageKind::Data, ..Default::default() }));
    p.add_item(
        "gone.mov",
        Default::default(),
        None,
        ItemKind::Footage(Footage { path: "/offline/gone.mov".into(), kind: FootageKind::Video, ..Default::default() }),
    );
    let path = dir.join("Library Card.ecproj").to_string_lossy().to_string();
    s.execute("file.saveAs", json!({"path": path})).unwrap();
    path
}

#[test]
fn import_project_merges_with_fresh_ids_and_one_undo() {
    let dir = out_dir("import-project");
    let path = source_project(&dir);

    let mut t = Session::default();
    t.execute("comp.new", json!({"name": "Show", "width": 64, "height": 64, "frameRate": 30, "duration": 2})).unwrap();
    t.execute("layer.newSolid", json!({"name": "Back", "color": "#00ff00"})).unwrap();
    let before = t.project.items.len();
    let before_ids: HashSet<u64> = all_ids(&t.project).into_iter().collect();

    let r = t.execute("file.importProject", json!({"path": path})).unwrap();
    let folder = ItemId(r["folder"].as_u64().unwrap());
    assert_eq!(t.project.item(folder).unwrap().name, "Library Card.ecproj");
    // Inner, Main, the Solids folder and its solid, two footage items, and the new folder.
    assert_eq!(t.project.items.len(), before + 7);
    let imported: Vec<ItemId> = r["items"].as_array().unwrap().iter().map(|v| ItemId(v.as_u64().unwrap())).collect();
    // Everything lands in the folder (the Solids folder keeps its solid).
    let top = |mut id: ItemId| {
        while let Some(up) = t.project.item(id).and_then(|i| i.parent).filter(|p| *p != folder) {
            id = up;
        }
        t.project.item(id).and_then(|i| i.parent)
    };
    assert!(imported.iter().all(|id| top(*id) == Some(folder)), "everything lands in the folder");

    // No id is taken twice, and none of the open project's ids moved.
    let ids = all_ids(&t.project);
    let unique: HashSet<u64> = ids.iter().copied().collect();
    assert_eq!(ids.len(), unique.len(), "ids collide");
    assert!(before_ids.is_subset(&unique));
    assert!(t.project.next_id > *ids.iter().max().unwrap());

    // References follow the new ids: Main nests the imported Inner, under the imported null.
    let main = ItemId(r["primaryComp"].as_u64().unwrap());
    assert_eq!(t.project.item(main).unwrap().name, "Main");
    assert_eq!(r["rootComps"], json!([main.0]));
    let comp = t.project.comp(main).unwrap();
    let nest = comp.layers.iter().find(|l| matches!(l.source, LayerSource::Comp { .. })).unwrap();
    let item = nest.source.item().unwrap();
    assert_eq!(t.project.item(item).unwrap().name, "Inner");
    assert!(imported.contains(&item));
    let parent = nest.parent.unwrap();
    assert_eq!(comp.layer(parent).unwrap().name, "Rig");

    // A relative footage path resolves next to the imported project; a missing one is reported.
    let data = t.project.items.values().find_map(|i| match &i.kind {
        ItemKind::Footage(f) if i.name == "data.json" => Some(f.path.clone()),
        _ => None,
    });
    assert_eq!(data.map(std::path::PathBuf::from), Some(dir.join("data.json")));
    assert_eq!(r["missingMedia"], json!(["/offline/gone.mov"]));

    // One undo step removes all of it; redo brings it back.
    t.execute("edit.undo", json!({})).unwrap();
    assert_eq!(t.project.items.len(), before);
    t.execute("edit.redo", json!({})).unwrap();
    assert_eq!(t.project.items.len(), before + 7);

    // Importing again keeps the copies apart, and File ▸ Import routes .ecproj here.
    let r2 = t.execute("file.import", json!({"paths": [path]})).unwrap();
    let again = &r2["projects"][0];
    assert_ne!(again["primaryComp"], r["primaryComp"]);
    assert!(r2["items"].as_array().unwrap().is_empty(), "a project is not footage");
    let ids = all_ids(&t.project);
    assert_eq!(ids.len(), ids.iter().copied().collect::<HashSet<_>>().len());
}

#[test]
fn import_project_rejects_bad_files_and_leaves_the_project() {
    let dir = out_dir("import-project-bad");
    let mut t = Session::default();
    t.execute("comp.new", json!({"name": "Show", "width": 64, "height": 64, "frameRate": 30, "duration": 2})).unwrap();
    let before = t.project.items.len();

    std::fs::write(dir.join("junk.ecproj"), b"not a project").unwrap();
    assert!(t.execute("file.importProject", json!({"path": dir.join("junk.ecproj").to_string_lossy()})).is_err());
    assert!(t.execute("file.importProject", json!({"path": dir.join("absent.ecproj").to_string_lossy()})).is_err());

    // Regression: an id at the top of the range saturated instead of overflowing (opening and
    // importing must not panic), and the import refuses it.
    let mut p = Project::default();
    let id = p.add_item("Huge", Default::default(), None, ItemKind::Folder);
    let mut it = p.items.remove(&id).unwrap();
    it.id = ItemId(u64::MAX);
    p.items.insert(it.id, it);
    let huge = dir.join("huge.ecproj");
    std::fs::write(&huge, p.to_json()).unwrap();
    assert_eq!(Project::from_json(&p.to_json()).unwrap().next_id, u64::MAX);
    let e = t.execute("file.importProject", json!({"path": huge.to_string_lossy()})).unwrap_err();
    assert!(e.to_string().contains("out of range"), "{e}");
    assert_eq!(t.project.items.len(), before);
}

/// Multi-agent fairness: `wait: false` reads a (big) project on a background thread and merges it
/// on the session's thread when done, with the blocking import's result and one undo step.
#[test]
fn import_project_in_the_background_matches_the_blocking_import() {
    let dir = out_dir("import-project-background");
    let path = source_project(&dir);
    let mut blocking = Session::default();
    blocking.execute("comp.new", json!({"name": "Host"})).unwrap();
    let expected = blocking.execute("file.importProject", json!({"path": path})).unwrap();
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Host"})).unwrap();
    let started = s.execute("file.importProject", json!({"path": path, "wait": false})).unwrap();
    let job = started["job"].as_str().unwrap().to_string();
    s.wait_jobs();
    let rec = s.job_log.iter().find(|r| r.id == job).unwrap();
    assert_eq!(rec.status, "done", "{}", rec.message);
    assert_eq!(rec.result, expected);
    assert_eq!(s.project.items.len(), blocking.project.items.len());
    // A file that isn't a project fails the task, leaving the project as it was.
    let bad = dir.join("bad.ecproj");
    std::fs::write(&bad, "not json").unwrap();
    let before = s.project.items.len();
    let started = s.execute("file.importProject", json!({"path": bad.to_string_lossy(), "wait": false})).unwrap();
    let job = started["job"].as_str().unwrap().to_string();
    s.wait_jobs();
    assert_eq!(s.job_log.iter().find(|r| r.id == job).unwrap().status, "failed");
    assert_eq!(s.project.items.len(), before);
}

/// Ordinary File > Open must resolve media exactly as project import does, before its lazy
/// background check. Relative paths belong to the document, including proxies and sequences.
#[test]
fn opening_project_anchors_relative_footage_sequences_and_proxies() {
    use crate::project::{Footage, FootageKind, Proxy};
    let dir = out_dir("open-relative-media");
    let mut s = Session::default();
    let p = std::sync::Arc::make_mut(&mut s.project);
    let id = p.add_item(
        "frames",
        crate::color::Label::None,
        None,
        ItemKind::Footage(Footage {
            path: "frames/one.png".into(),
            kind: FootageKind::Sequence,
            sequence: vec!["frames/one.png".into(), "frames/two.png".into()],
            ..Default::default()
        }),
    );
    p.item_mut(id).unwrap().proxy = Some(Box::new(Proxy { footage: Footage { path: "proxy.mov".into(), ..Default::default() }, enabled: true, auto: false }));
    let absolute = dir.join("absolute.wav").to_string_lossy().into_owned();
    let aid = p.add_item("absolute", crate::color::Label::None, None, ItemKind::Footage(Footage { path: absolute.clone(), ..Default::default() }));
    let path = dir.join("show.ecproj");
    std::fs::write(&path, p.to_json()).unwrap();
    let mut opened = Session::default();
    opened.execute("file.open", json!({"path": path})).unwrap();
    let item = opened.project.item(id).unwrap();
    let ItemKind::Footage(f) = &item.kind else { panic!("footage") };
    assert_eq!(std::path::Path::new(&f.path), dir.join("frames/one.png"));
    assert_eq!(f.sequence.iter().map(std::path::PathBuf::from).collect::<Vec<_>>(), [dir.join("frames/one.png"), dir.join("frames/two.png")]);
    assert_eq!(std::path::Path::new(&item.proxy.as_ref().unwrap().footage.path), dir.join("proxy.mov"));
    let ItemKind::Footage(f) = &opened.project.item(aid).unwrap().kind else { panic!("footage") };
    assert_eq!(f.path, absolute);
    assert!(!opened.is_dirty());
}
