//! Motion Library: publish a library folder to a folder backend, browse, search and apply its
//! cards, footage left out as relinkable placeholders, the update after a re-publish, and
//! failures (offline, damaged catalog) as messages.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use effectcraft_motionlib::{Client, DirRemote, DirStore};
use effectcraft_project::{Footage, FootageKind, ItemKind};
use effectcraft_time::FrameRate;
use effectcraft_time::Tick;
use serde_json::{Value, json};

use crate::Session;

fn out_dir(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-out").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A card's project: comp `name` using an image and a movie (absolute paths in `media`).
fn card_project(dir: &Path, media: &Path, name: &str) -> String {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": name, "width": 64, "height": 64, "frameRate": 30, "duration": 2})).unwrap();
    s.execute("layer.newSolid", json!({"name": "Fill", "color": "#ff0000"})).unwrap();
    let (png, mov) = (media.join("badge.png"), media.join("presenter.mov"));
    std::fs::write(&png, b"not really a png").unwrap();
    std::fs::write(&mov, vec![7u8; 4096]).unwrap();
    let p = Arc::make_mut(&mut s.project);
    p.add_item(
        "badge.png",
        Default::default(),
        None,
        ItemKind::Footage(Footage { path: png.to_string_lossy().into(), kind: FootageKind::Still, width: 8, height: 8, has_video: true, ..Default::default() }),
    );
    p.add_item(
        "presenter.mov",
        Default::default(),
        None,
        ItemKind::Footage(Footage {
            path: mov.to_string_lossy().into(),
            kind: FootageKind::Video,
            width: 1920,
            height: 1080,
            frame_rate: FrameRate { num: 30, den: 1 },
            duration: Tick::from_seconds_f64(2.0),
            has_video: true,
            ..Default::default()
        }),
    );
    let path = dir.join(format!("{name}.ecproj"));
    s.execute("file.saveAs", json!({"path": path.to_string_lossy()})).unwrap();
    path.to_string_lossy().to_string()
}

fn write_index(lib: &Path, entries: &[Value]) {
    let index = json!({"format": "effectcraft-library", "version": 1, "name": "Test Library", "entries": entries});
    std::fs::write(lib.join("library.json"), serde_json::to_vec_pretty(&index).unwrap()).unwrap();
}

fn entry(id: &str, name: &str, category: &str, file: &str, thumb: &Path) -> Value {
    json!({"id": id, "name": name, "category": category, "kind": "comp", "file": file, "comp": name, "description": "A reel card that pops",
           "thumbnail": thumb.to_string_lossy(), "preview": ""})
}

fn session_with(remote: &Path, cache: &Path) -> Session {
    let s = Session::default();
    s.motion_library.set(Some(Client::new(Arc::new(DirRemote::new(remote)), Arc::new(DirStore::new(cache)))));
    s
}

#[test]
fn publish_browse_apply_relink_and_update() {
    let root = out_dir("motion-library");
    let (lib, media, remote, cache) = (root.join("lib"), root.join("media"), root.join("remote"), root.join("cache"));
    for d in [&lib, &media, &remote] {
        std::fs::create_dir_all(d).unwrap();
    }
    let projects = lib.join("projects");
    std::fs::create_dir_all(&projects).unwrap();
    let thumb = media.join("thumb.png");
    std::fs::write(&thumb, b"thumbnail bytes").unwrap();
    card_project(&projects, &media, "Reel Card");
    card_project(&projects, &media, "Other Thing");
    write_index(
        &lib,
        &[
            entry("reel", "Reel Card", "Devin Jatho/UI", "projects/Reel Card.ecproj", &thumb),
            entry("other", "Other Thing", "Elsewhere", "projects/Other Thing.ecproj", &thumb),
        ],
    );

    // Publish only the chosen folder; the movie stays home as a placeholder.
    let mut pubs = Session::default();
    let r = pubs.execute("library.publish", json!({"library": lib.to_string_lossy(), "folders": ["Devin Jatho"], "remote": remote.to_string_lossy()})).unwrap();
    assert_eq!((r["version"].as_u64(), r["cards"].as_u64(), r["needsFootage"].as_u64()), (Some(1), Some(1), Some(1)), "{r}");
    assert_eq!(r["skipped"][0]["reason"], "video");
    let again =
        pubs.execute("library.publish", json!({"library": lib.to_string_lossy(), "folders": ["Devin Jatho"], "remote": remote.to_string_lossy()})).unwrap();
    assert_eq!((again["unchanged"].as_bool(), again["uploads"].as_u64()), (Some(true), Some(0)), "{again}");

    // Another machine: the movie isn't there.
    std::fs::remove_file(media.join("presenter.mov")).unwrap();
    let mut s = session_with(&remote, &cache);
    let st = s.execute("library.sync", json!({})).unwrap();
    assert_eq!((st["version"].as_u64(), st["cards"].as_u64()), (Some(1), Some(1)), "{st}");
    assert!(st["update"].is_null());
    let cat = s.execute("library.catalog", json!({"folder": "Devin Jatho"})).unwrap();
    assert_eq!(cat["total"], 1);
    assert_eq!(cat["cards"][0]["needsFootage"], true);
    assert!(cat["folders"].as_array().unwrap().iter().any(|f| f["folder"] == "Devin Jatho/UI"));
    let found = s.execute("library.search", json!({"query": "reel pops"})).unwrap();
    assert_eq!(found["entries"][0]["id"], "reel", "{found}");
    assert_eq!(found["entries"][0]["source"], "motion");
    let card = s.execute("library.card", json!({"id": "reel"})).unwrap();
    assert_eq!(card["placeholders"][0]["name"], "presenter.mov");
    assert_eq!(card["placeholders"][0]["fps"], 30.0);

    // Apply into the open comp: one undo step, the image from the cache, the movie missing.
    s.execute("comp.new", json!({"name": "Edit", "width": 64, "height": 64, "frameRate": 30, "duration": 4})).unwrap();
    let edit = s.active_comp_id().unwrap();
    let undo_before = s.history.undo.len();
    let r = s.execute("library.apply", json!({"id": "reel"})).unwrap();
    assert!(r["layer"].as_u64().is_some(), "{r}");
    assert_eq!(s.project.comp(edit).unwrap().layers.len(), 1);
    assert_eq!(s.history.undo.len(), undo_before + 1);
    let needs = r["needsFootage"].as_array().unwrap();
    assert_eq!(needs.len(), 1);
    assert_eq!(needs[0]["missing"], true);
    let item = effectcraft_project::ItemId(needs[0]["item"].as_u64().unwrap());
    let ItemKind::Footage(f) = &s.project.item(item).unwrap().kind else { panic!("not footage") };
    assert_eq!((f.width, f.height, f.duration), (1920, 1080, Tick::from_seconds_f64(2.0)));
    let badge = s.project.items.values().find(|i| i.name == "badge.png" && r["import"]["items"].as_array().unwrap().contains(&json!(i.id.0))).unwrap();
    let ItemKind::Footage(bf) = &badge.kind else { panic!() };
    assert!(bf.path.starts_with(&*cache.to_string_lossy()), "{}", bf.path);
    // Relink the placeholder to this edit's footage.
    let footage = media.join("todays.mov");
    std::fs::write(&footage, vec![1u8; 64]).unwrap();
    s.execute("footage.relink", json!({"item": item.0, "path": footage.to_string_lossy()})).unwrap();
    let ItemKind::Footage(f) = &s.project.item(item).unwrap().kind else { panic!() };
    assert_eq!(f.path, footage.to_string_lossy());
    s.execute("edit.undo", json!({})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.project.comp(edit).unwrap().layers.is_empty());

    // Re-publish with a new card: the chip's update, adopted on request.
    card_project(&projects, &media, "New Card");
    write_index(
        &lib,
        &[
            entry("reel", "Reel Card", "Devin Jatho/UI", "projects/Reel Card.ecproj", &thumb),
            entry("new", "New Card", "Devin Jatho/UI", "projects/New Card.ecproj", &thumb),
        ],
    );
    let r = pubs.execute("library.publish", json!({"library": lib.to_string_lossy(), "folders": "Devin Jatho", "remote": remote.to_string_lossy()})).unwrap();
    assert_eq!((r["version"].as_u64(), r["new"].as_u64()), (Some(2), Some(1)), "{r}");
    let st = s.execute("library.sync", json!({})).unwrap();
    assert_eq!(st["version"], 1);
    assert_eq!(st["update"]["new"], 1);
    assert_eq!(st["update"]["label"], "Library updated: 1 new");
    let st = s.execute("library.update", json!({})).unwrap();
    assert_eq!((st["version"].as_u64(), st["cards"].as_u64(), st["adopted"].as_bool()), (Some(2), Some(2), Some(true)));

    // A fresh session over the same cache browses offline.
    let offline = session_with(&root.join("nowhere"), &cache);
    let st = { offline }.execute("library.sync", json!({})).unwrap();
    assert_eq!(st["version"], 2);
    assert!(st["error"].as_str().unwrap().starts_with("Offline"), "{st}");
}

#[test]
fn failures_are_messages() {
    let root = out_dir("motion-library-bad");
    let (remote, cache) = (root.join("remote"), root.join("cache"));
    std::fs::create_dir_all(&remote).unwrap();
    // Not set up.
    let mut s = Session::default();
    let st = s.execute("library.status", json!({})).unwrap();
    if st["configured"] == false {
        assert!(s.execute("library.apply", json!({"id": "x"})).is_err());
    }
    // A damaged head, then a head naming a manifest that isn't there.
    std::fs::write(remote.join("head.json"), b"{not json").unwrap();
    let mut s = session_with(&remote, &cache);
    let st = s.execute("library.sync", json!({})).unwrap();
    assert!(st["error"].as_str().is_some_and(|e| e.contains("library head")), "{st}");
    assert!(s.execute("library.catalog", json!({})).is_err());
    let head = json!({"format": "effectcraft-motion-library", "schema": 1, "version": 3, "manifest": {"hash": "0".repeat(64), "size": 10, "gz": true}});
    std::fs::write(remote.join("head.json"), head.to_string()).unwrap();
    let st = s.execute("library.sync", json!({})).unwrap();
    assert!(st["error"].as_str().is_some_and(|e| e.contains("not found")), "{st}");
    // A damaged blob is refused (hash check), not imported.
    let blob_dir = remote.join("blobs");
    std::fs::create_dir_all(&blob_dir).unwrap();
    std::fs::write(blob_dir.join(format!("{}.gz", "0".repeat(64))), b"garbage").unwrap();
    let st = s.execute("library.sync", json!({})).unwrap();
    assert!(st["error"].as_str().is_some_and(|e| e.contains(&"0".repeat(64)) || e.contains("damaged")), "{st}");
    assert_eq!(st["cards"], 0);
}

#[test]
fn configure_keeps_the_token_out_of_replies() {
    let root = out_dir("motion-library-config");
    let mut s = Session { config: Some(Arc::new(crate::config::MemoryConfig::default())), ..Default::default() };
    let r = s
        .execute(
            "library.configure",
            json!({"remote": root.join("remote").to_string_lossy(), "token": "s3cret-token", "cache": root.join("cache").to_string_lossy()}),
        )
        .unwrap();
    assert_eq!(r["tokenSet"], true);
    let st = s.execute("library.status", json!({})).unwrap();
    assert_eq!(st["configured"], true);
    for v in [&r, &st] {
        assert!(!v.to_string().contains("s3cret-token"), "{v}");
    }
    // A folder that isn't a library yet: a message, no catalog.
    let st = s.execute("library.sync", json!({})).unwrap();
    assert!(st["error"].as_str().is_some_and(|e| e.contains("not found")), "{st}");
}

/// `catalogAll`: the rest of the library is listed with thumbnails only (applied on the Mac):
/// browsable, never applied here, and left out of `library.search` (insertable cards only).
#[test]
fn catalog_all_lists_on_the_mac_cards() {
    let root = out_dir("motion-library-catalog-all");
    let (lib, media, remote, cache) = (root.join("lib"), root.join("media"), root.join("remote"), root.join("cache"));
    for d in [&lib, &media, &remote, &lib.join("projects")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let thumb = media.join("thumb.png");
    std::fs::write(&thumb, b"thumbnail bytes").unwrap();
    card_project(&lib.join("projects"), &media, "Reel Card");
    card_project(&lib.join("projects"), &media, "Other Thing");
    let index = json!({"format": "effectcraft-library", "version": 1, "name": "Test Library",
        "collections": {"order": {"Tapa": 0, "Devin Jatho": 1}, "folders": ["Tapa"]},
        "entries": [entry("reel", "Reel Card", "Devin Jatho/UI", "projects/Reel Card.ecproj", &thumb),
                    entry("other", "Other Thing", "Graphs", "projects/Other Thing.ecproj", &thumb)],
        "catalog_only": [{"id": "ease", "name": "Snappy ease", "category": "Graphs", "kind": "mac", "type_label": "Ease", "thumbnail": thumb.to_string_lossy()}]});
    std::fs::write(lib.join("library.json"), serde_json::to_vec(&index).unwrap()).unwrap();
    let mut pubs = Session::default();
    let r = pubs
        .execute(
            "library.publish",
            json!({"library": lib.to_string_lossy(), "folders": ["Devin Jatho"], "catalogAll": true, "remote": remote.to_string_lossy()}),
        )
        .unwrap();
    assert_eq!((r["cards"].as_u64(), r["onMac"].as_u64()), (Some(3), Some(2)), "{r}");

    let mut s = session_with(&remote, &cache);
    s.execute("library.sync", json!({})).unwrap();
    let cat = s.execute("library.catalog", json!({"folder": "Graphs"})).unwrap();
    assert_eq!(cat["total"], 2, "{cat}");
    assert!(cat["cards"].as_array().unwrap().iter().all(|c| c["onMac"] == true));
    let folders: Vec<&str> = cat["folders"].as_array().unwrap().iter().filter_map(|f| f["folder"].as_str()).collect();
    assert_eq!(folders, ["Tapa", "Devin Jatho", "Devin Jatho/UI", "Graphs"]);
    let card = s.execute("library.card", json!({"id": "ease"})).unwrap();
    assert_eq!((card["kindLabel"].as_str(), card["onMac"].as_bool()), (Some("Ease"), Some(true)));
    let err = s.execute("library.apply", json!({"id": "other"})).unwrap_err().to_string();
    assert!(err.contains("on the Mac"), "{err}");
    let found = s.execute("library.search", json!({"query": "other thing"})).unwrap();
    assert!(found["entries"].as_array().unwrap().iter().all(|e| e["source"] != "motion"), "{found}");
    assert!(s.execute("library.apply", json!({"id": "reel"})).is_ok());
}

/// On the Mac that keeps the library (its folder added with `library.addFolder`), a card listed
/// "on the Mac" applies from the local files (one undo step), shows as local, and reveals its
/// file; one the folder doesn't list stays on the Mac.
#[test]
fn on_the_mac_cards_apply_from_the_local_library() {
    let root = out_dir("motion-library-local");
    let (lib, media, remote, cache) = (root.join("lib"), root.join("media"), root.join("remote"), root.join("cache"));
    for d in [&lib, &media, &remote, &lib.join("projects")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let thumb = media.join("thumb.png");
    std::fs::write(&thumb, b"thumbnail bytes").unwrap();
    card_project(&lib.join("projects"), &media, "Reel Card");
    card_project(&lib.join("projects"), &media, "Other Thing");
    let index = json!({"format": "effectcraft-library", "version": 1, "name": "Test Library",
        "entries": [entry("reel", "Reel Card", "Devin Jatho/UI", "projects/Reel Card.ecproj", &thumb),
                    entry("other", "Other Thing", "Graphs", "projects/Other Thing.ecproj", &thumb)],
        "catalog_only": [{"id": "ease", "name": "Snappy ease", "category": "Graphs", "kind": "mac", "type_label": "Ease", "thumbnail": thumb.to_string_lossy()}]});
    std::fs::write(lib.join("library.json"), serde_json::to_vec(&index).unwrap()).unwrap();
    Session::default()
        .execute(
            "library.publish",
            json!({"library": lib.to_string_lossy(), "folders": ["Devin Jatho"], "catalogAll": true, "remote": remote.to_string_lossy()}),
        )
        .unwrap();

    let mut s = session_with(&remote, &cache);
    s.execute("library.sync", json!({})).unwrap();
    assert!(s.execute("library.apply", json!({"id": "other"})).unwrap_err().to_string().contains("on the Mac"));
    s.execute("library.addFolder", json!({"path": lib.to_string_lossy()})).unwrap();
    let card = s.execute("library.card", json!({"id": "other"})).unwrap();
    assert_eq!((card["onMac"].as_bool(), card["local"].as_bool()), (Some(false), Some(true)), "{card}");
    assert_eq!(s.execute("library.card", json!({"id": "ease"})).unwrap()["onMac"], true);

    s.execute("comp.new", json!({"name": "Main", "width": 64, "height": 64, "frameRate": 30, "duration": 4})).unwrap();
    let undo = s.history.undo.len();
    let r = s.execute("library.apply", json!({"id": "other", "time": 1.0})).unwrap();
    assert_eq!(r["source"], "local", "{r}");
    assert!(r["layer"].as_u64().is_some(), "{r}");
    assert_eq!(s.history.undo.len(), undo + 1, "one undo step");
    assert_eq!(s.active_comp().unwrap().layers.iter().next().unwrap().name, "Other Thing");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.active_comp().unwrap().layers.iter().count(), 0);
    // `remote: true` still goes to the bucket (which lists it on the Mac only).
    assert!(s.execute("library.apply", json!({"id": "other", "remote": true})).is_err());

    let r = s.execute("library.reveal", json!({"id": "other"})).unwrap();
    assert!(r["url"].as_str().unwrap().ends_with("/projects"), "{r}");
    assert!(s.execute("library.reveal", json!({"id": "ease"})).is_err());
    let found = s.execute("library.search", json!({"query": "other thing", "source": "motion"})).unwrap();
    assert_eq!(found["entries"][0]["id"], "other", "{found}");
}

/// `library.saveCard` (Save to Library on the Mac): a dry run says the size and what stays home,
/// over `askAboveMB` it waits for `confirm`, then the card's files and a new catalog version go
/// up; the card stays saved when the library is published again.
#[test]
fn save_card_uploads_one_card_and_it_stays_saved() {
    let root = out_dir("motion-library-save");
    let (lib, media, remote, cache) = (root.join("lib"), root.join("media"), root.join("remote"), root.join("cache"));
    for d in [&lib, &media, &remote, &lib.join("projects")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let thumb = media.join("thumb.png");
    std::fs::write(&thumb, b"thumbnail bytes").unwrap();
    card_project(&lib.join("projects"), &media, "Reel Card");
    card_project(&lib.join("projects"), &media, "Other Thing");
    write_index(
        &lib,
        &[
            entry("reel", "Reel Card", "Devin Jatho/UI", "projects/Reel Card.ecproj", &thumb),
            entry("other", "Other Thing", "Graphs", "projects/Other Thing.ecproj", &thumb),
        ],
    );
    let publish = json!({"library": lib.to_string_lossy(), "folders": ["Devin Jatho"], "catalogAll": true, "remote": remote.to_string_lossy()});
    Session::default().execute("library.publish", publish.clone()).unwrap();

    let mut s = Session { config: Some(Arc::new(crate::config::MemoryConfig::default())), ..Default::default() };
    s.execute("library.configure", json!({"remote": remote.to_string_lossy(), "token": "publish-token", "cache": cache.to_string_lossy()})).unwrap();
    s.execute("library.sync", json!({})).unwrap();
    assert!(s.execute("library.saveCard", json!({"id": "other", "wait": true})).unwrap_err().to_string().contains("library folder"));
    s.execute("library.addFolder", json!({"path": lib.to_string_lossy()})).unwrap();

    let dry = s.execute("library.saveCard", json!({"id": "other", "dryRun": true, "wait": true})).unwrap();
    assert!(dry["uploads"].as_u64().unwrap() >= 1 && dry["uploadMB"].as_f64().is_some(), "{dry}");
    let skipped: Vec<&str> = dry["skipped"].as_array().unwrap().iter().filter_map(|k| k["file"].as_str()).collect();
    assert_eq!(skipped, ["presenter.mov"], "video stays home: {dry}");
    let ask = s.execute("library.saveCard", json!({"id": "other", "askAboveMB": 0, "wait": true})).unwrap();
    assert_eq!(ask["needsConfirm"], true, "{ask}");
    assert_eq!(s.execute("library.status", json!({})).unwrap()["version"], 1, "nothing uploaded yet");

    let saved = s.execute("library.saveCard", json!({"id": "other", "askAboveMB": 0, "confirm": true, "wait": true})).unwrap();
    assert_eq!(saved["version"], 2, "{saved}");
    let card = s.execute("library.card", json!({"id": "other"})).unwrap();
    assert_eq!(card["onMac"], false, "{card}");
    assert!(!card["files"].as_array().unwrap().is_empty(), "its files are in the bucket: {card}");

    // Published again from the folders: the saved card keeps its files.
    Session::default().execute("library.publish", publish).unwrap();
    let mut other = session_with(&remote, &root.join("cache2"));
    other.execute("library.sync", json!({})).unwrap();
    assert_eq!(other.execute("library.card", json!({"id": "other"})).unwrap()["onMac"], false);
    other.execute("comp.new", json!({"name": "Main", "width": 64, "height": 64, "frameRate": 30, "duration": 4})).unwrap();
    assert_eq!(other.execute("library.apply", json!({"id": "other"})).unwrap()["source"], "remote");
}
