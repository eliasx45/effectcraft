//! MCP protocol round-trips, in-process (headless) and against a fake control channel (bridge).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

use serde_json::{Value, json};

use crate::{Backend, McpServer, Session, base64, png_size};

fn server() -> McpServer {
    McpServer::new(Backend::headless(Session::default()))
}

/// Send a request, return its `result` (panics on a JSON-RPC error).
fn rpc(s: &mut McpServer, id: u64, method: &str, params: Value) -> Value {
    let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    let reply: Value = serde_json::from_str(&s.handle_line(&line).expect("reply")).unwrap();
    assert_eq!(reply["jsonrpc"], "2.0");
    assert_eq!(reply["id"], id);
    assert!(reply.get("error").is_none(), "{method}: {reply}");
    reply["result"].clone()
}

/// Call a tool; returns (content, isError).
fn call(s: &mut McpServer, name: &str, args: Value) -> (Vec<Value>, bool) {
    let r = rpc(s, 99, "tools/call", json!({"name": name, "arguments": args}));
    (r["content"].as_array().unwrap().clone(), r["isError"].as_bool().unwrap())
}

/// Call a tool that returns JSON text; panics if it failed.
fn call_json(s: &mut McpServer, name: &str, args: Value) -> Value {
    let (c, err) = call(s, name, args.clone());
    assert!(!err, "{name} {args}: {c:?}");
    assert_eq!(c[0]["type"], "text");
    serde_json::from_str(c[0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn property_group_error_points_to_layer_inspection() {
    let mut s = server();
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "Main"}}));
    let l = call_json(&mut s, "execute_command", json!({"command": "layer.newText", "params": {"text": "Hi"}}))["layer"].clone();
    call_json(&mut s, "execute_command", json!({"command": "comp.renderer", "params": {"renderer": "advanced3d"}}));
    for tool in ["get_property", "set_property", "add_keyframe"] {
        let mut args = json!({"layer": l, "path": "geometryOptions", "time": 0});
        if tool != "get_property" {
            args["value"] = json!(0);
        }
        let (content, err) = call(&mut s, tool, args);
        assert!(err, "{tool}: {content:?}");
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains("is a property group"), "{tool}: {text}");
        assert!(text.contains("get_layer"), "{tool}: {text}");
    }
    let tree = call_json(&mut s, "get_layer", json!({"layer": l, "flat": true}));
    assert!(tree["properties"].as_array().unwrap().iter().any(|p| p["path"] == "geometryOptions/extrusionDepth"));
    assert!(call_json(&mut s, "get_property", json!({"layer": l, "path": "geometryOptions/extrusionDepth"}))["value"].is_number());
}

#[test]
fn initialize_and_list_tools() {
    let mut s = server();
    let r = rpc(&mut s, 1, "initialize", json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}));
    assert_eq!(r["protocolVersion"], "2025-03-26");
    assert_eq!(r["serverInfo"]["name"], "effectcraft");
    assert!(r["capabilities"]["tools"].is_object());
    // Unknown revision: we answer with our latest.
    let r = rpc(&mut s, 2, "initialize", json!({"protocolVersion": "1999-01-01"}));
    assert_eq!(r["protocolVersion"], crate::server::PROTOCOL_VERSIONS[0]);
    // Notifications get no reply.
    assert!(s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
    assert_eq!(rpc(&mut s, 3, "ping", json!({})), json!({}));

    let tools = rpc(&mut s, 4, "tools/list", json!({}))["tools"].as_array().unwrap().clone();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for want in [
        "command_list",
        "command_run",
        "run_script",
        "doc_inspect",
        "get_comp",
        "get_layer",
        "get_property",
        "set_property",
        "add_keyframe",
        "render_preview",
        "open_project",
        "save_project",
        "undo",
        "redo",
    ] {
        assert!(names.contains(&want), "missing {want}");
    }
    assert!(!names.contains(&"screenshot"), "bridge-only tools are hidden headless");
    for t in &tools {
        assert!(t["description"].as_str().unwrap().len() > 20);
        assert_eq!(t["inputSchema"]["type"], "object");
    }
}

#[test]
fn protocol_errors() {
    let mut s = server();
    let r: Value = serde_json::from_str(&s.handle_line("{not json").unwrap()).unwrap();
    assert_eq!(r["error"]["code"], -32700);
    let r: Value = serde_json::from_str(&s.handle_line(r#"{"jsonrpc":"2.0","id":5,"method":"nope"}"#).unwrap()).unwrap();
    assert_eq!(r["error"]["code"], -32601);
    let r: Value = serde_json::from_str(&s.handle_line(r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"screenshot"}}"#).unwrap()).unwrap();
    assert_eq!(r["error"]["code"], -32602);
    // Batches.
    let r: Value =
        serde_json::from_str(&s.handle_line(r#"[{"jsonrpc":"2.0","id":7,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/x"}]"#).unwrap()).unwrap();
    assert_eq!(r.as_array().unwrap().len(), 1);
    // Tool failures are in-band.
    let (c, err) = call(&mut s, "execute_command", json!({"command": "no.such"}));
    assert!(err);
    assert!(c[0]["text"].as_str().unwrap().contains("unknown command"));
    let (_, err) = call(&mut s, "render_frame", json!({}));
    assert!(err, "no comp yet");
    // Unknown params are rejected with the accepted keys, not silently ignored.
    let (c, err) = call(&mut s, "execute_command", json!({"command": "comp.new", "params": {"nmae": "X"}}));
    assert!(err);
    assert!(c[0]["text"].as_str().unwrap().contains("accepted: name, width"), "{c:?}");
}

#[test]
fn headless_workflow() {
    let mut s = server();
    rpc(&mut s, 1, "initialize", json!({"protocolVersion": "2025-06-18"}));

    let cmds = call_json(&mut s, "list_commands", json!({"filter": "layer.new"}));
    assert!(cmds.as_array().unwrap().iter().any(|c| c["id"] == "layer.newSolid"));

    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "Main", "width": 320, "height": 180, "duration": 4}}));
    let l =
        call_json(&mut s, "execute_command", json!({"command": "layer.newSolid", "params": "{\"color\":\"#ff0000\",\"width\":100,\"height\":100}"}))["layer"]
            .clone();
    assert!(l.is_u64());

    let comp = call_json(&mut s, "get_comp", json!({"comp": "Main"}));
    assert_eq!(comp["width"], 320);
    assert_eq!(comp["layers"][0]["id"], l);

    // Static set + read back.
    let p = call_json(&mut s, "set_property", json!({"layer": l, "path": "transform/opacity", "value": 50}));
    assert_eq!(p["value"], 50.0);
    assert_eq!(call_json(&mut s, "get_property", json!({"layer": "#1", "path": "transform/opacity"}))["value"], 50.0);

    // Keyframes with easing.
    let p = call_json(
        &mut s,
        "add_keyframe",
        json!({"layer": l, "path": "transform/position", "keys": [{"time": 0, "value": [0, 90]}, {"time": 2, "value": [320, 90]}], "interpolation": "easyEase"}),
    );
    assert_eq!(p["keys"].as_array().unwrap().len(), 2);
    assert_eq!(p["keys"][0]["out"], "Bezier");
    let mid = call_json(&mut s, "get_property", json!({"layer": l, "path": "transform/position", "time": 1.0}));
    assert!((mid["value"][0].as_f64().unwrap() - 160.0).abs() < 1.0, "{mid}");

    // Comp-time keys on an offset layer (#257): the key lands where the Timeline shows it.
    call_json(&mut s, "execute_command", json!({"command": "layer.timing", "params": {"layer": l, "start": 1.0}}));
    let p = call_json(
        &mut s,
        "add_keyframe",
        json!({"layer": l, "path": "transform/scale", "keys": [{"time": 1.5, "value": [50, 50]}, {"time": 2.5, "value": [100, 100]}], "timeBase": "comp", "interpolation": "hold"}),
    );
    let times: Vec<f64> = p["keys"].as_array().unwrap().iter().map(|k| k["time"].as_f64().unwrap()).collect();
    // Comp times snap to the comp's frames (29.97 fps) before becoming layer time.
    assert!((times[0] - 0.5).abs() < 0.02 && (times[1] - 1.5).abs() < 0.02, "layer-time keys: {times:?}");
    assert_eq!(p["keys"][0]["out"], "Hold", "the comp-time keys were selected for the interpolation");
    call_json(&mut s, "set_property", json!({"layer": l, "path": "transform/scale", "value": [75, 75], "time": 3.0, "timeBase": "comp"}));
    let p = call_json(&mut s, "get_property", json!({"layer": l, "path": "transform/scale"}));
    assert!(p["keys"].as_array().unwrap().iter().any(|k| (k["time"].as_f64().unwrap() - 2.0).abs() < 0.02), "{p}");

    // Expression via set_property.
    let p = call_json(&mut s, "set_property", json!({"layer": l, "path": "transform/rotation", "expression": "time * 90"}));
    assert_eq!(p["expression"], "time * 90");

    // Property tree carries paths; flat mode too.
    let tree = call_json(&mut s, "get_layer", json!({"layer": l, "flat": true}));
    let props = tree["properties"].as_array().unwrap();
    assert!(props.iter().any(|p| p["path"] == "transform/position" && p["keys"] == 2));

    // Render: an inline PNG at the requested size.
    let (c, err) = call(&mut s, "render_frame", json!({"time": 1.0, "max_side": 160}));
    assert!(!err, "{c:?}");
    assert_eq!(c[0]["type"], "image");
    assert_eq!(c[0]["mimeType"], "image/png");
    let png = base64::decode(c[0]["data"].as_str().unwrap()).unwrap();
    assert_eq!(png_size(&png), Some((160, 90)));
    let info: Value = serde_json::from_str(c[1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(info["width"], 160);

    // Undo / redo.
    let before = call_json(&mut s, "get_project", json!({}))["undo"].as_array().unwrap().len();
    let u = call_json(&mut s, "undo", json!({"steps": 2}));
    assert_eq!(u["steps"], 2);
    assert_eq!(u["undoStack"].as_array().unwrap().len(), before - 2);
    call_json(&mut s, "redo", json!({"steps": 2}));
    assert_eq!(call_json(&mut s, "get_project", json!({}))["undo"].as_array().unwrap().len(), before);

    // Save + reopen.
    let dir = std::env::temp_dir().join(format!("ec-mcp-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.ecproj").to_string_lossy().into_owned();
    let saved = call_json(&mut s, "save_project", json!({"path": path}));
    assert_eq!(saved["dirty"], false);
    call_json(&mut s, "open_project", json!({"new": true}));
    assert!(call(&mut s, "get_comp", json!({})).1, "empty project has no comp");
    let proj = call_json(&mut s, "open_project", json!({"path": path}));
    assert_eq!(proj["items"].as_array().unwrap().len(), 3, "comp + Solids folder + solid: {proj}");
    let p = call_json(&mut s, "get_property", json!({"comp": "Main", "layer": "#1", "path": "transform/position"}));
    assert_eq!(p["keys"].as_array().unwrap().len(), 2);
    let _ = std::fs::remove_dir_all(dir);
}

/// An expression with a syntax error is kept but disabled; set_property and get_property report
/// its error, and `evaluated` is the static value that renders (#163).
#[test]
fn expression_syntax_errors_reach_the_reply() {
    let mut s = McpServer::new(Backend::headless(effectcraft_host::session()));
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "A", "width": 64, "height": 64}}));
    call_json(&mut s, "execute_command", json!({"command": "layer.newSolid", "params": {"name": "S", "color": "#ff0000", "width": 32, "height": 8}}));
    let at = |expr: &str| json!({"layer": "S", "path": "transform/rotation", "expression": expr});
    call_json(&mut s, "set_property", json!({"layer": "S", "path": "transform/rotation", "value": 30}));
    let set = call_json(&mut s, "set_property", at("thisIsBroken("));
    let got = call_json(&mut s, "get_property", json!({"layer": "S", "path": "transform/rotation"}));
    for r in [&set, &got] {
        assert_eq!((&r["expression"], &r["evaluated"]), (&json!("thisIsBroken("), &json!(30.0)), "{r}");
        assert!(r["expressionError"].as_str().unwrap().contains("SyntaxError"), "{r}");
    }
    // A runtime error and a valid expression report as before.
    let r = call_json(&mut s, "set_property", at("nope*2"));
    assert!(r["expressionError"].as_str().unwrap().contains("nope is not defined"), "{r}");
    let r = call_json(&mut s, "set_property", at("60"));
    assert_eq!((&r["evaluated"], r.get("expressionError")), (&json!(60.0), None), "{r}");
    let flat = call_json(&mut s, "get_layer", json!({"layer": "S", "flat": true}));
    let rotation = flat["properties"].as_array().unwrap().iter().find(|p| p["path"] == "transform/rotation").unwrap();
    assert_eq!(rotation["uid"], r["uid"]);
    assert_eq!(rotation["rawValue"], 30.0);
    assert_eq!(rotation["evaluatedValue"], 60.0);
    assert_eq!(rotation["effectiveEnabled"], true);
}

#[test]
fn stdio_loop() {
    let mut s = server();
    let input = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "open_project", "arguments": {"demo": true}}}),
    ]
    .iter()
    .map(|v| v.to_string() + "\n")
    .collect::<String>();
    let mut out = Vec::new();
    s.serve(std::io::Cursor::new(input), &mut out).unwrap();
    let lines: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["id"], 2);
    assert_eq!(lines[1]["result"]["isError"], false);
}

/// Regression for #258: a dirty headless project must survive an MCP disconnect.
#[test]
fn headless_autosave_on_eof_preserves_unsaved_project() {
    let dir = std::env::temp_dir().join(format!("ec-mcp-autosave-eof-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut s = server().with_autosave(&dir).unwrap();
    let input = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "execute_command", "arguments": {"command": "comp.new", "params": {"name": "Keep my work", "width": 320, "height": 180}}}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "execute_command", "arguments": {"command": "layer.newText", "params": {"text": "Unsaved title"}}}}),
    ].iter().map(|v| v.to_string() + "\n").collect::<String>();
    let mut output = Vec::new();
    s.serve(std::io::Cursor::new(input), &mut output).unwrap();
    let path = s.backend().session().unwrap().autosave.last_path.clone();
    assert!(path.is_some(), "disconnect discarded the dirty project without an auto-save");
    let path = path.unwrap();
    let mut fresh = server();
    call_json(&mut fresh, "open_project", json!({"path": path}));
    let comp = call_json(&mut fresh, "get_comp", json!({"comp": "Keep my work"}));
    assert_eq!(comp["layers"].as_array().unwrap().len(), 1);
    assert!(s.backend().session().unwrap().is_dirty(), "an auto-save must not mark the original project saved");
    std::fs::remove_dir_all(dir).unwrap();
}

fn autosave_test_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ec-mcp-autosave-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn autosave_status(s: &mut McpServer) -> Value {
    rpc(s, 1, "initialize", json!({}))["_meta"]["effectcraftAutoSave"].clone()
}

#[test]
fn autosave_is_durable_before_reply_and_queries_do_not_rotate() {
    let dir = autosave_test_dir("durable");
    let mut s = server().with_autosave(&dir).unwrap();
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "Durable"}}));
    let first = autosave_status(&mut s);
    let path = first["latestPath"].as_str().unwrap();
    assert!(std::fs::read_to_string(path).unwrap().contains("Durable"));
    let summary = call_json(&mut s, "get_project", json!({}));
    assert_eq!(summary["autosave"]["latestPath"], path);
    assert_eq!(summary["path"], Value::Null);
    assert_eq!(summary["dirty"], true);
    assert_eq!(autosave_status(&mut s)["latestPath"], path);
    assert!(s.backend().session().unwrap().config.is_none());
    drop(s); // Abrupt process loss has no EOF cleanup; the checkpoint already exists.
    let mut restarted = server().with_autosave(&dir).unwrap();
    let init = rpc(&mut restarted, 1, "initialize", json!({}));
    // The instructions carry the auto-save state as JSON, where a Windows path's backslashes
    // are escaped.
    assert!(init["instructions"].as_str().unwrap().contains(&Value::from(path).to_string()));
    assert_eq!(init["_meta"]["effectcraftAutoSave"]["previousSessions"][0]["autosave"], path);
    call_json(&mut restarted, "open_project", json!({"path": path}));
    assert_eq!(call_json(&mut restarted, "get_project", json!({}))["items"][0]["name"], "Durable");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn autosave_errors_preserve_tool_success_retry_and_fail_shutdown() {
    let dir = autosave_test_dir("write-error");
    let mut s = server().with_autosave(&dir).unwrap();
    let folder = std::path::PathBuf::from(autosave_status(&mut s)["folder"].as_str().unwrap());
    std::fs::remove_dir(&folder).unwrap();
    std::fs::write(&folder, "blocked by file").unwrap();
    let (content, failed) = call(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "Still in memory"}}));
    assert!(!failed, "the edit succeeded even though its checkpoint failed");
    assert!(content.iter().any(|c| c["text"].as_str().unwrap_or("").contains("Warning:")));
    assert!(s.backend().session().unwrap().is_dirty());
    assert!(autosave_status(&mut s)["error"].as_str().unwrap().contains("failed"));
    let error = s.serve(&b""[..], Vec::new()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    assert!(error.to_string().contains("MCP auto-save"));
    std::fs::remove_file(&folder).unwrap();
    // Even a query retries the outstanding dirty revision.
    let summary = call_json(&mut s, "get_project", json!({}));
    assert_eq!(summary["autosave"]["error"], Value::Null);
    let path = summary["autosave"]["latestPath"].as_str().unwrap();
    assert!(std::fs::read_to_string(path).unwrap().contains("Still in memory"));
    std::fs::remove_dir_all(dir).unwrap();
}

/// Big checkpoints are throttled (a 1 GB project must not be rewritten after every agent edit):
/// the change waits, `pending` says so, and the session's end writes it.
#[test]
fn autosave_throttles_big_checkpoints_and_flushes_at_the_end() {
    let dir = autosave_test_dir("throttle");
    let mut s = server().with_autosave(&dir).unwrap();
    s.throttle_autosave(1, std::time::Duration::from_secs(3600));
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "First"}}));
    let path = autosave_status(&mut s)["latestPath"].as_str().unwrap().to_string();
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "Second"}}));
    assert!(!std::fs::read_to_string(&path).unwrap().contains("Second"), "throttled: not written yet");
    assert_eq!(autosave_status(&mut s)["pending"], true);
    s.serve(&b""[..], Vec::new()).unwrap();
    let latest = s.backend().session().unwrap().autosave.last_path.clone().unwrap();
    assert!(std::fs::read_to_string(latest).unwrap().contains("Second"), "the end of the session flushes the pending change");
    std::fs::remove_dir_all(dir).unwrap();
}

/// Recovery copies clean themselves up: saving the project normally empties the session's folder,
/// and a session that ends saved leaves nothing behind.
#[test]
fn autosave_folder_is_removed_once_the_project_is_saved() {
    let dir = autosave_test_dir("cleanup");
    let mut s = server().with_autosave(&dir).unwrap();
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "Saved later"}}));
    let status = autosave_status(&mut s);
    let folder = std::path::PathBuf::from(status["folder"].as_str().unwrap());
    assert!(std::path::Path::new(status["latestPath"].as_str().unwrap()).is_file());
    call_json(&mut s, "save_project", json!({"path": dir.join("Saved.ecproj")}));
    assert!(!folder.exists(), "saved normally: the recovery copies are gone");
    assert_eq!(autosave_status(&mut s)["latestPath"], Value::Null);
    // A later change checkpoints again into a recreated folder.
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "After save"}}));
    assert!(std::fs::read_to_string(autosave_status(&mut s)["latestPath"].as_str().unwrap()).unwrap().contains("After save"));
    call_json(&mut s, "save_project", json!({}));
    s.serve(&b""[..], Vec::new()).unwrap();
    assert!(!folder.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

/// A new session prunes folders of finished, saved sessions and any folder untouched for 30 days;
/// recent unsaved work and folders it didn't make stay.
#[test]
fn autosave_prunes_finished_and_abandoned_sessions() {
    let dir = autosave_test_dir("prune");
    let base = dir.join(effectcraft_engine::autosave::AUTOSAVE_FOLDER).join("MCP");
    let session = |name: &str, record: Value| {
        let f = base.join(name);
        std::fs::create_dir_all(&f).unwrap();
        std::fs::write(f.join("session.json"), record.to_string()).unwrap();
        f
    };
    let saved = session("1-100-0", json!({"dirty": false, "ended": true}));
    let unsaved = session("2-200-0", json!({"dirty": true, "ended": false, "autosave": null}));
    let other = session("notes", json!({"dirty": false, "ended": true}));
    let _s = server().with_autosave(&dir).unwrap();
    assert!(!saved.exists() && unsaved.exists() && other.exists());
    crate::autosave::prune(&base, std::time::SystemTime::now() + crate::autosave::ABANDONED_AFTER);
    assert!(!unsaved.exists() && other.exists(), "untouched for 30 days: removed; foreign folders kept");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn autosave_keeps_sessions_and_desktop_recovery_separate() {
    let dir = autosave_test_dir("isolated");
    let desktop_save = dir.join(effectcraft_engine::autosave::AUTOSAVE_FOLDER).join("Untitled Project auto-save 1.ecproj");
    std::fs::create_dir_all(desktop_save.parent().unwrap()).unwrap();
    std::fs::write(&desktop_save, "desktop backup").unwrap();
    std::fs::write(dir.join("session.lock"), "desktop sentinel").unwrap();
    let mut a = server().with_autosave(&dir).unwrap();
    let mut b = server().with_autosave(&dir).unwrap();
    assert_ne!(autosave_status(&mut a)["folder"], autosave_status(&mut b)["folder"]);
    for index in 0..7 {
        for (s, name) in [(&mut a, "A"), (&mut b, "B")] {
            call_json(s, "execute_command", json!({"command": "comp.new", "params": {"name": format!("{name}{index}")}}));
        }
    }
    for (s, mine, other) in [(&mut a, "A6", "B6"), (&mut b, "B6", "A6")] {
        let status = autosave_status(s);
        let json = std::fs::read_to_string(status["latestPath"].as_str().unwrap()).unwrap();
        assert!(json.contains(mine));
        assert!(!json.contains(other));
        let folder = std::path::Path::new(status["folder"].as_str().unwrap());
        assert_eq!(effectcraft_engine::autosave::existing(folder, "Untitled Project").len(), 5);
    }
    assert_eq!(std::fs::read_to_string(desktop_save).unwrap(), "desktop backup");
    assert_eq!(std::fs::read_to_string(dir.join("session.lock")).unwrap(), "desktop sentinel");
    assert!(!dir.join("prefs.json").exists());
    assert!(!dir.join("shortcuts.json").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn autosave_marks_saved_or_undone_work_clean_without_losing_its_path() {
    let dir = autosave_test_dir("clean");
    let mut s = server().with_autosave(&dir).unwrap();
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "Undo me"}}));
    let status = autosave_status(&mut s);
    let folder = std::path::PathBuf::from(status["folder"].as_str().unwrap());
    // Undone back to the saved state: nothing is unsaved, so the recovery copies go.
    call_json(&mut s, "undo", json!({}));
    assert!(!folder.exists());
    assert_eq!(autosave_status(&mut s)["latestPath"], Value::Null);
    // Redone: unsaved again, checkpointed again.
    call_json(&mut s, "redo", json!({}));
    let path = autosave_status(&mut s)["latestPath"].as_str().unwrap().to_string();
    assert!(std::fs::read_to_string(&path).unwrap().contains("Undo me"));
    call_json(&mut s, "save_project", json!({"path": dir.join("Saved.ecproj")}));
    assert_eq!(autosave_status(&mut s)["latestPath"], Value::Null, "saved normally: no recovery file is kept");
    let mut next = server().with_autosave(&dir).unwrap();
    assert!(autosave_status(&mut next)["previousSessions"].as_array().unwrap().is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

/// A reply that cannot be written ends `serve` with the write error, even while stdin stays open
/// (the client is gone but never closed its end): the reader thread must not be waited for.
#[test]
fn broken_output_returns_while_stdin_stays_open() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let (in_r, mut in_w) = std::io::pipe().unwrap();
    writeln!(in_w, "{}", json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})).unwrap();
    let (done_tx, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(server().serve(BufReader::new(in_r), Broken));
    });
    let result = done.recv_timeout(std::time::Duration::from_secs(10)).expect("serve must return while stdin is still open");
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
    drop(in_w);
}

#[test]
fn autosave_flushes_on_read_errors_and_broken_output() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let dir = autosave_test_dir("transports");
    for invalid_input in [true, false] {
        let mut s = server().with_autosave(&dir).unwrap();
        // Simulate an edit whose client went away before it received a tool response.
        s.backend().exec("comp.new", json!({"name": "Transport failure"})).unwrap();
        let error = if invalid_input {
            s.serve(&b"\xff\n"[..], Vec::new()).unwrap_err()
        } else {
            s.serve(&b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n"[..], Broken).unwrap_err()
        };
        assert_eq!(error.kind(), if invalid_input { std::io::ErrorKind::InvalidData } else { std::io::ErrorKind::BrokenPipe });
        let status = autosave_status(&mut s);
        assert!(std::fs::read_to_string(status["latestPath"].as_str().unwrap()).unwrap().contains("Transport failure"));
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// A fake app's reply to one control request: `engine.execute` from a real session, and canned
/// `render.frame` / `ui.screenshot` replies.
fn fake_reply(session: &mut Session, method: &str, p: &Value) -> Value {
    match method {
        "engine.execute" => match session.execute(p["command"].as_str().unwrap(), p["params"].clone()) {
            Ok(v) => json!({"ok": true, "result": v}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        },
        "render.frame" => {
            let png = crate::encode_png(4, 2, vec![255; 32], 0).unwrap();
            json!({"ok": true, "result": {"comp": 1, "time": 0.5, "width": 4, "height": 2, "png": base64::encode(&png)}})
        }
        "ui.screenshot" => {
            let png = crate::encode_png(8, 8, vec![128; 256], 0).unwrap();
            std::fs::write(p["path"].as_str().unwrap(), png).unwrap();
            json!({"ok": true, "result": {"path": p["path"]}})
        }
        "ui.key" | "ui.click" => json!({"ok": true, "result": null}),
        m => json!({"ok": false, "error": format!("unknown method `{m}`")}),
    }
}

/// A fake desktop control channel over TCP (see [`fake_reply`]).
fn fake_app() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (stream, _) = l.accept().unwrap();
        let mut out = stream.try_clone().unwrap();
        let mut session = Session::default();
        for line in BufReader::new(stream).lines() {
            let msg: Value = serde_json::from_str(&line.unwrap()).unwrap();
            let mut reply = fake_reply(&mut session, msg["method"].as_str().unwrap(), &msg["params"]);
            reply["id"] = msg["id"].clone();
            writeln!(out, "{reply}").unwrap();
        }
    });
    port
}

/// An in-process control transport (as `mcp --ui`'s offscreen UI), recording the methods called.
struct FakeTransport {
    session: Session,
    calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl crate::ControlTransport for FakeTransport {
    fn call(&mut self, method: &str, params: Value) -> crate::Result<Value> {
        self.calls.lock().unwrap().push(method.to_string());
        crate::bridge::reply_result(fake_reply(&mut self.session, method, &params))
    }
    fn mode(&self) -> &'static str {
        "headless-ui"
    }
}

#[test]
fn ui_tools_run_through_any_control_transport() {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut s = McpServer::new(Backend::transport(FakeTransport { session: Session::default(), calls: calls.clone() }));
    let r = rpc(&mut s, 1, "initialize", json!({}));
    assert!(r["serverInfo"]["title"].as_str().unwrap().contains("headless-ui"), "{r}");
    let names: Vec<String> =
        rpc(&mut s, 2, "tools/list", json!({}))["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    for want in ["screenshot", "ui_click", "ui_elements", "control", "command_run"] {
        assert!(names.iter().any(|n| n == want), "{want} listed");
    }
    // Engine commands, inspection and renders go through the transport too.
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "T", "width": 64, "height": 64}}));
    assert_eq!(call_json(&mut s, "get_comp", json!({}))["name"], "T");
    let (c, err) = call(&mut s, "render_frame", json!({}));
    assert!(!err, "{c:?}");
    assert_eq!(png_size(&base64::decode(c[0]["data"].as_str().unwrap()).unwrap()), Some((4, 2)));
    let (c, err) = call(&mut s, "screenshot", json!({"panel": "Timeline", "max_side": 4}));
    assert!(!err, "{c:?}");
    assert_eq!(png_size(&base64::decode(c[0]["data"].as_str().unwrap()).unwrap()), Some((4, 4)));
    assert!(!call(&mut s, "ui_click", json!({"id": "tools.Selection"})).1);
    let (c, err) = call(&mut s, "control", json!({"method": "ui.nope"}));
    assert!(err && c[0]["text"].as_str().unwrap().contains("unknown method"));
    let calls = calls.lock().unwrap().clone();
    for want in ["engine.execute", "render.frame", "ui.screenshot", "ui.click", "ui.nope"] {
        assert!(calls.iter().any(|c| c == want), "{want} in {calls:?}");
    }
}

#[test]
fn bridge_forwards_to_control_channel() {
    let port = fake_app();
    let mut s = McpServer::new(Backend::bridge(&port.to_string()).unwrap());
    let r = rpc(&mut s, 1, "initialize", json!({}));
    assert!(r["serverInfo"]["title"].as_str().unwrap().contains("bridge"));
    let names: Vec<String> =
        rpc(&mut s, 2, "tools/list", json!({}))["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    assert!(names.iter().any(|n| n == "screenshot") && names.iter().any(|n| n == "ui_click"));

    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "B", "width": 64, "height": 64}}));
    assert_eq!(call_json(&mut s, "get_comp", json!({}))["name"], "B");

    // Bridged agents run under their own name from `initialize` on (AN4), and a named agent
    // renders through the engine command on its own cursor (the app's session: comp B).
    assert!(s.backend().client().is_some());
    let (c, err) = call(&mut s, "render_frame", json!({}));
    assert!(!err, "{c:?}");
    assert_eq!(png_size(&base64::decode(c[0]["data"].as_str().unwrap()).unwrap()), Some((64, 64)));
    // As the user (`"client": ""`), the render goes to the app's own `render.frame` method.
    let (c, err) = call(&mut s, "render_frame", json!({"client": ""}));
    assert!(!err, "{c:?}");
    assert_eq!(png_size(&base64::decode(c[0]["data"].as_str().unwrap()).unwrap()), Some((4, 2)));

    let (c, err) = call(&mut s, "screenshot", json!({"max_side": 4}));
    assert!(!err, "{c:?}");
    assert_eq!(png_size(&base64::decode(c[0]["data"].as_str().unwrap()).unwrap()), Some((4, 4)));

    assert!(!call(&mut s, "ui_key", json!({"key": "Space"})).1);
    let (c, err) = call(&mut s, "control", json!({"method": "ui.nope"}));
    assert!(err && c[0]["text"].as_str().unwrap().contains("unknown method"));
}

#[test]
fn bridge_rejects_non_loopback() {
    assert!(Backend::bridge("10.0.0.1:9877").is_err());
    assert!(Backend::bridge("localhost:9877").is_ok());
}

/// The app applies the first request but loses its reply. The next connection can be either
/// an automatic retry or an explicit follow-up; the returned counts distinguish them.
fn lost_reply_server(first_reply: Option<&'static str>) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let worker = std::thread::spawn(move || {
        let mut session = Session::default();
        let mut mutations = 0;
        for attempt in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut output = stream.try_clone().unwrap();
            let mut input = String::new();
            let mut reader = BufReader::new(stream);
            reader.read_line(&mut input).unwrap();
            let handshake: Value = serde_json::from_str(&input).unwrap();
            assert_eq!(handshake["params"]["command"], "app.capabilities");
            writeln!(output, "{}", json!({"id": handshake["id"], "ok": true, "result": {}})).unwrap();
            input.clear();
            reader.read_line(&mut input).unwrap();
            let request: Value = serde_json::from_str(&input).unwrap();
            let result = if request["method"] == "engine.execute" {
                mutations += 1;
                session.execute(request["params"]["command"].as_str().unwrap(), request["params"]["params"].clone()).unwrap()
            } else if request["method"] == "ui.inspect" || (request["method"] == "ui.playback" && request["params"]["action"] == "status") {
                json!({"mutations": mutations, "requests": attempt + 1, "comps": session.project.comps().count()})
            } else {
                mutations += 1;
                json!({"mutations": mutations})
            };
            if attempt == 1 {
                writeln!(output, "{}", json!({"id": request["id"], "ok": true, "result": result})).unwrap();
            } else if let Some(reply) = first_reply {
                writeln!(output, "{reply}").unwrap();
            }
        }
    });
    (addr, worker)
}

#[test]
fn bridge_does_not_replay_a_mutation_when_its_reply_is_lost() {
    let (addr, worker) = lost_reply_server(None);
    let mut backend = Backend::bridge(&addr).unwrap();
    let error = backend.exec("comp.new", json!({"width": 64, "height": 48, "duration": 1})).unwrap_err();
    assert!(error.to_string().contains("outcome is unknown"), "{error}");
    let state = backend.control("ui.inspect", json!({})).unwrap();
    assert_eq!(state["mutations"], 1, "the first command was applied exactly once");
    assert_eq!(state["comps"], 1);
    worker.join().unwrap();
}

#[test]
fn bridge_retries_read_only_queries_after_a_lost_reply() {
    let (addr, worker) = lost_reply_server(None);
    let mut backend = Backend::bridge(&addr).unwrap();
    let state = backend.control("ui.inspect", json!({})).unwrap();
    assert_eq!(state["requests"], 2);
    assert_eq!(state["mutations"], 0);
    worker.join().unwrap();
}

#[test]
fn bridge_reports_unknown_outcome_after_a_malformed_mutation_reply() {
    let (addr, worker) = lost_reply_server(Some("{invalid reply"));
    let mut backend = Backend::bridge(&addr).unwrap();
    let error = backend.exec("comp.new", json!({"width": 64, "height": 48, "duration": 1})).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("outcome is unknown") && message.contains("bad reply"), "{message}");
    assert_eq!(backend.control("ui.inspect", json!({})).unwrap()["comps"], 1);
    worker.join().unwrap();
}

#[test]
fn bridge_does_not_assume_an_unknown_method_is_read_only() {
    let (addr, worker) = lost_reply_server(None);
    let mut backend = Backend::bridge(&addr).unwrap();
    let error = backend.control("future.customOperation", json!({})).unwrap_err();
    assert!(error.to_string().contains("outcome is unknown"), "{error}");
    assert_eq!(backend.control("ui.inspect", json!({})).unwrap()["mutations"], 1);
    worker.join().unwrap();
}

#[test]
fn bridge_playback_edits_and_default_toggle_are_not_replayed() {
    for params in [json!({}), json!({"action": "toggle"}), json!({"action": "play"}), json!({"action": "stop"})] {
        let (addr, worker) = lost_reply_server(None);
        let mut backend = Backend::bridge(&addr).unwrap();
        let error = backend.control("ui.playback", params).unwrap_err();
        assert!(error.to_string().contains("outcome is unknown"), "{error}");
        assert_eq!(backend.control("ui.inspect", json!({})).unwrap()["mutations"], 1);
        worker.join().unwrap();
    }
}

#[test]
fn bridge_playback_status_can_be_retried() {
    let (addr, worker) = lost_reply_server(None);
    let mut backend = Backend::bridge(&addr).unwrap();
    let state = backend.control("ui.playback", json!({"action": "status"})).unwrap();
    assert_eq!(state["requests"], 2);
    assert_eq!(state["mutations"], 0);
    worker.join().unwrap();
}

#[test]
fn bridge_connect_failure_can_be_retried_when_the_app_starts() {
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = reserved.local_addr().unwrap();
    drop(reserved);
    let mut backend = Backend::bridge(&addr.to_string()).unwrap();
    let error = backend.control("ui.inspect", json!({})).unwrap_err();
    assert!(error.to_string().contains("cannot connect"), "{error}");
    assert!(!error.to_string().contains("outcome is unknown"), "nothing was sent");
    let listener = TcpListener::bind(addr).unwrap();
    let worker = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut output = stream.try_clone().unwrap();
        let mut input = String::new();
        let mut reader = BufReader::new(stream);
        reader.read_line(&mut input).unwrap();
        let handshake: Value = serde_json::from_str(&input).unwrap();
        writeln!(output, "{}", json!({"id": handshake["id"], "ok": true, "result": {}})).unwrap();
        input.clear();
        reader.read_line(&mut input).unwrap();
        let request: Value = serde_json::from_str(&input).unwrap();
        writeln!(output, "{}", json!({"id": request["id"], "ok": true, "result": {"ready": true}})).unwrap();
    });
    assert_eq!(backend.control("ui.inspect", json!({})).unwrap()["ready"], true);
    worker.join().unwrap();
}

#[test]
fn script_ui_and_history_tools() {
    let mut sess = Session::default();
    effectcraft_script::install(&mut sess);
    let mut s = McpServer::new(Backend::headless(sess));
    let code = "var d = new Window('dialog', 'Ask'); var g = d.add('group'); g.add('button', undefined, 'OK', {name: 'ok'}); \
                if (d.show() == 1) app.project.items.addComp('Answered', 64, 64, 1, 1, 24); 'done'";
    let r = call_json(&mut s, "run_script", json!({"code": code}));
    assert_eq!(r["waiting"], json!(true), "{r}");
    let list = call_json(&mut s, "script_ui", json!({}));
    assert_eq!(list[0]["title"], "Ask");
    let tree = call_json(&mut s, "script_ui", json!({"action": "get"}));
    assert_eq!(tree["root"]["children"][0]["children"][0]["name"], "ok");
    let r = call_json(&mut s, "script_ui", json!({"action": "click", "widget": "ok"}));
    assert_eq!(r["result"], json!("done"), "{r}");
    // History: undo the comp, make another, and jump back to the first on its branch.
    call_json(&mut s, "undo", json!({}));
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "Other", "width": 64, "height": 64}}));
    let h = call_json(&mut s, "history", json!({}));
    let branch = h["states"].as_array().unwrap().iter().find(|n| n["depth"] == 1).unwrap()["index"].clone();
    let h = call_json(&mut s, "history", json!({"goto": branch}));
    let cur = h["current"].as_u64().unwrap() as usize;
    assert_eq!(h["states"][cur]["depth"], 0, "the jumped-to state is now the working line");
    assert!(call_json(&mut s, "get_project", json!({})).to_string().contains("Answered"));
}

#[test]
fn run_script_round_trip() {
    let mut sess = Session::default();
    effectcraft_script::install(&mut sess);
    let mut s = McpServer::new(Backend::headless(sess));
    let r = call_json(
        &mut s,
        "run_script",
        json!({"code": "var c = app.project.items.addComp('Scripted', 320, 180, 1, 2, 24);\nc.layers.addText('Hi');\nwriteLn('made ' + c.name);\nc.numLayers"}),
    );
    assert_eq!(r["ok"], json!(true), "{r}");
    assert_eq!(r["result"], json!(1));
    assert_eq!(r["output"], json!("made Scripted"));
    // The edit is visible to the other tools.
    let p = call_json(&mut s, "get_project", json!({}));
    assert!(p.to_string().contains("Scripted"), "{p}");
    // Errors come back with their line.
    let (content, error) = call(&mut s, "run_script", json!({"code": "var a = 1;\nundefinedFn();", "name": "bad.jsx"}));
    assert!(error);
    let r: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(r["ok"], json!(false));
    assert_eq!(r["error"]["line"], json!(2));
    assert_eq!(r["error"]["file"], json!("bad.jsx"));
    // Without a scripting engine the command explains itself.
    let mut bare = server();
    let (c, err) = call(&mut bare, "run_script", json!({"code": "1"}));
    assert!(err && c[0]["text"].as_str().unwrap().contains("not available"), "{c:?}");
}

#[test]
fn puppet_rigging_over_mcp() {
    let mut s = server();
    let ex = |s: &mut McpServer, command: &str, params: Value| call_json(s, "execute_command", json!({"command": command, "params": params}));
    ex(&mut s, "comp.new", json!({"name": "Rig", "width": 200, "height": 100, "duration": 2}));
    let id = ex(&mut s, "layer.newSolid", json!({"width": 100, "height": 60, "color": "#3060c0"}))["layer"].clone();
    let lead = ex(&mut s, "puppet.addPin", json!({"layer": id, "position": [10, 30]}))["pin"].clone();
    let tail = ex(&mut s, "puppet.addPin", json!({"layer": id, "position": [90, 30]}))["pin"].clone();
    let bend = ex(&mut s, "puppet.addPin", json!({"layer": id, "kind": "bend", "position": [50, 30]}))["pin"].clone();
    // Turn and scale the Bend pin, select pins, make the tail trail the leader, rig the leader.
    ex(&mut s, "puppet.setPin", json!({"layer": id, "pin": bend, "rotation": 45, "scale": 120}));
    assert_eq!(ex(&mut s, "puppet.selectPins", json!({"layer": id, "pins": [lead, tail]}))["pins"], json!([lead, tail]));
    assert_eq!(ex(&mut s, "puppet.follow", json!({"delay": 0.25}))["pins"], json!([{"pin": tail, "delay": 0.25}]));
    let r = ex(&mut s, "paths.pointsFollowNulls", json!({"layer": id, "pins": [lead]}));
    assert_eq!(r["nulls"].as_array().unwrap().len(), 1);
    // The new commands are listed for agents.
    let (c, _) = call(&mut s, "list_commands", json!({"filter": "puppet."}));
    let text = c[0]["text"].as_str().unwrap();
    assert!(text.contains("puppet.follow") && text.contains("puppet.selectPins"), "{text}");
}

#[test]
fn round_corners_over_mcp() {
    let mut s = server();
    let run = |s: &mut McpServer, id: &str, params: Value| call_json(s, "command_run", json!({"id": id, "params": params}));
    run(&mut s, "comp.new", json!({"name": "Round", "width": 200, "height": 100, "duration": 1}));
    let id = run(&mut s, "layer.newSolid", json!({"width": 120, "height": 60, "color": "#c03030"}))["layer"].clone();
    let d = call_json(&mut s, "describe_command", json!({"command": "layer.roundCorners"}));
    assert!(d["schema"]["properties"]["roundness"].is_object(), "{d}");
    let r = run(&mut s, "layer.roundCorners", json!({"layer": id, "roundness": 16}));
    assert_eq!(r["converted"], json!([id]), "{r}");
    let l = call_json(&mut s, "get_layer", json!({"layer": id, "full": true}));
    assert!(l.to_string().contains("Roundness"), "{l}");
    let p = call_json(&mut s, "get_property", json!({"layer": id, "path": "contents/group/contents/rect/roundness"}));
    assert_eq!(p["value"], 16.0, "{p}");
    // A shape layer takes it on its rectangles; one undo step each.
    run(&mut s, "layer.roundCorners", json!({"layer": id, "roundness": 4}));
    call_json(&mut s, "undo", json!({}));
    let p = call_json(&mut s, "get_property", json!({"layer": id, "path": "contents/group/contents/rect/roundness"}));
    assert_eq!(p["value"], 16.0, "{p}");
}

/// The agent-interface audit (MCP side): every registered engine command is listed by
/// `command_list` with a label, params doc and JSON Schema, described by `describe_command`,
/// and routed by `command_run` (an unknown parameter is rejected by that very command
/// without running it).
#[test]
fn every_engine_command_is_reachable_over_mcp() {
    let mut s = server();
    let tools = rpc(&mut s, 1, "tools/list", json!({}))["tools"].as_array().unwrap().clone();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for want in ["command_list", "describe_command", "command_run", "add_effect", "list_effects", "get_state", "render_preview", "get_layer", "set_property"] {
        assert!(names.contains(&want), "{want}");
    }
    let listed = call_json(&mut s, "command_list", json!({"schemas": true,"full":true}));
    let listed: std::collections::HashMap<&str, &Value> = listed.as_array().unwrap().iter().map(|c| (c["id"].as_str().unwrap(), c)).collect();
    let specs = effectcraft_engine::command_specs();
    assert_eq!(listed.len(), specs.len());
    for spec in specs {
        let c = listed.get(spec.id).unwrap_or_else(|| panic!("{} not listed", spec.id));
        assert!(!spec.label.trim().is_empty(), "{}: no label", spec.id);
        assert!(spec.params.trim_start().starts_with('{'), "{}: params doc {:?}", spec.id, spec.params);
        assert_eq!(c["schema"]["type"], "object", "{}", spec.id);
        assert!(c["schema"]["properties"].is_object(), "{}", spec.id);
        let d = call_json(&mut s, "describe_command", json!({"command": spec.id}));
        assert_eq!(d["schema"], c["schema"], "{}", spec.id);
        let (content, err) = call(&mut s, "command_run", json!({"id": spec.id, "params": {"__audit": 1}}));
        let text = content[0]["text"].as_str().unwrap_or_default();
        assert!(err && text.contains("unknown parameter") && text.contains(spec.id), "{}: {text}", spec.id);
    }
}

#[test]
fn high_level_tools_add_effect_and_report_state() {
    let mut s = server();
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "A", "width": 64, "height": 48, "duration": 1}}));
    let l = call_json(&mut s, "execute_command", json!({"command": "layer.newSolid", "params": {"color": "#808080"}}))["layer"].clone();
    let fx = call_json(&mut s, "list_effects", json!({"filter": "gaussian"}));
    assert!(fx.as_array().unwrap().iter().any(|e| e["id"] == "ec.blur.gaussian" && e["gpu"].is_boolean()), "{fx}");
    let r = call_json(&mut s, "add_effect", json!({"layer": l, "effect": "Gaussian Blur", "values": {"blurriness": 7}}));
    assert_eq!(r["path"], "effects/#1", "{r}");
    assert!(r["params"].as_array().unwrap().iter().any(|p| p["path"] == "effects/#1/blurriness" && p["value"] == 7.0), "{r}");
    // A second effect lands at #2.
    let r = call_json(&mut s, "add_effect", json!({"layer": l, "effect": "ec.color.levels"}));
    assert_eq!(r["path"], "effects/#2", "{r}");
    let p = call_json(&mut s, "get_property", json!({"layer": l, "path": "effects/#1/blurriness"}));
    assert_eq!(p["value"], 7.0, "{p}");
    let st = call_json(&mut s, "get_state", json!({}));
    assert!(st["state"].is_object() && st["app"]["commands"].as_u64().unwrap() > 500, "{st}");
    assert!(st["app"]["effects"]["total"].as_u64().unwrap() > 200, "{st}");
    assert!(st["app"]["parity"]["summary"].as_str().unwrap().contains("parity"), "{st}");
    // Unknown effects fail cleanly.
    let (c, err) = call(&mut s, "add_effect", json!({"layer": l, "effect": "No Such Effect"}));
    assert!(err && c[0]["text"].as_str().unwrap().contains("unknown effect"), "{c:?}");
}

/// add_effect is one undo step, and a call that fails leaves the layer and the history as they
/// were (#175).
#[test]
fn add_effect_is_one_undo_step_and_all_or_nothing() {
    let mut s = server();
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "A", "width": 64, "height": 64}}));
    call_json(&mut s, "execute_command", json!({"command": "layer.newSolid", "params": {"name": "S", "color": "#ff0000"}}));
    let undo = |s: &mut McpServer| call_json(s, "get_project", json!({}))["undo"].as_array().unwrap().clone();
    let effects = |s: &mut McpServer| {
        let l = call_json(s, "get_layer", json!({"layer": "S", "flat": true}));
        l["properties"].as_array().unwrap().iter().filter(|p| p["path"].as_str().unwrap().starts_with("effects/")).count()
    };
    let before = undo(&mut s);
    let (c, err) = call(&mut s, "add_effect", json!({"layer": "S", "effect": "Gaussian Blur", "values": {"blurriness": 8, "repeatEdgePixels": true}}));
    assert!(err && c[0]["text"].as_str().unwrap().contains("effects/#1/repeatEdgePixels"), "{c:?}");
    assert_eq!((undo(&mut s), effects(&mut s)), (before.clone(), 0), "nothing applied");
    // The corrected retry lands at #1, in one undo step named like effect.apply's.
    let r = call_json(&mut s, "add_effect", json!({"layer": "S", "effect": "Gaussian Blur", "values": {"blurriness": 8, "repeatEdge": true}}));
    assert_eq!((&r["path"], &r["set"]), (&json!("effects/#1"), &json!(["effects/#1/blurriness", "effects/#1/repeatEdge"])), "{r}");
    let after = undo(&mut s);
    assert_eq!(after.len(), before.len() + 1, "{after:?}");
    assert_eq!(after.last(), Some(&json!("Apply Gaussian Blur")));
    call_json(&mut s, "undo", json!({}));
    assert_eq!((undo(&mut s), effects(&mut s)), (before, 0), "one undo removes the effect and its values");
}

#[test]
fn list_fonts_lists_families_with_styles() {
    let mut s = server();
    let all = call_json(&mut s, "list_fonts", json!({"full":true}));
    let fams = all["families"].as_array().unwrap();
    assert!(fams.iter().any(|f| f["family"] == "Inter" && f["origin"] == "bundled"), "{all}");
    // The query filters by substring: the bundled JetBrains Mono, and any installed family whose
    // name contains it (JetBrainsMono Nerd Font…) (#149).
    let one = call_json(&mut s, "list_fonts", json!({"query": "jetbrains"}));
    let matched = one["families"].as_array().unwrap();
    assert_eq!(one["count"].as_u64(), Some(matched.len() as u64), "{one}");
    assert!(matched.iter().all(|f| f["family"].as_str().unwrap().to_lowercase().contains("jetbrains")), "{one}");
    assert!(matched.len() < fams.len(), "the query filters: {one}");
    let mono = matched.iter().find(|f| f["family"] == "JetBrains Mono").unwrap_or_else(|| panic!("{one}"));
    assert!(mono["styles"].as_array().unwrap().contains(&json!("Regular")), "{one}");
    assert_eq!(mono["variable"], false, "{one}");
    // `variable` keeps only the variable fonts (the font menu's filter); the static bundled
    // families drop out.
    let var = call_json(&mut s, "list_fonts", json!({"variable": true}));
    let listed = var["families"].as_array().unwrap();
    assert!(listed.iter().all(|f| f["variable"] == true && f["family"] != "Inter"), "{var}");
}

/// Trained models over MCP (Roto Brush and face tracking): listed with their authors and
/// licence, chosen, refused when unknown.
#[test]
fn trained_models_over_mcp() {
    let mut s = server();
    for (prefix, id) in [("roto", "mobilesam"), ("face", "mediapipe-face")] {
        let list = call_json(&mut s, "execute_command", json!({"command": format!("{prefix}.models"), "params": {}}));
        let models = list["models"].as_array().unwrap();
        assert!(models.iter().any(|m| m["id"] == id && m["licence"] == "Apache-2.0" && m["authors"].is_string() && m["installed"] == false), "{list}");
        let r = call_json(&mut s, "execute_command", json!({"command": format!("{prefix}.model.select"), "params": {"id": id}}));
        assert_eq!(r["model"], id);
        let (c, err) = call(&mut s, "execute_command", json!({"command": format!("{prefix}.model.select"), "params": {"id": "nope"}}));
        assert!(err && c[0]["text"].as_str().unwrap().contains("unknown model"), "{c:?}");
    }
}

/// docs/agents.md's Tools table lists every tool, and an entry that lists arguments lists every
/// argument the tool's input schema accepts (#137).
#[test]
fn agents_doc_tools_table_is_current() {
    // (Checkouts may have CRLF line endings.)
    let doc = include_str!("../../../docs/agents.md").replace("\r\n", "\n");
    let table: String = doc.split("### Tools").nth(1).and_then(|t| t.split("\n\n").nth(1)).unwrap_or_default().to_string();
    assert!(table.starts_with("| Tool |"), "no Tools table");
    let mut missing = vec![];
    for t in crate::tools::catalogue() {
        // `name` (no arguments) or `name {a, b?, …}`.
        // (the first match may be a longer name: `render_frame` in `render_frames`)
        let Some(at) = table.match_indices(&format!("`{}", t.name)).map(|(i, _)| i).find(|i| {
            let rest = &table[i + 1 + t.name.len()..];
            rest.starts_with('`') || rest.starts_with(" {")
        }) else {
            missing.push(format!("{}: no entry", t.name));
            continue;
        };
        let rest = &table[at + 1 + t.name.len()..];
        if !rest.starts_with(" {") {
            continue;
        }
        let args = rest.split("}`").next().unwrap_or_default();
        let schema = t.input_schema();
        for prop in schema["properties"].as_object().into_iter().flat_map(|p| p.keys()) {
            // These common options are documented once above the table.
            if ["full", "offset", "limit", "max_chars", "expectRevision"].contains(&prop.as_str()) {
                continue;
            }
            if !args.split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|w| w == prop) {
                missing.push(format!("{}: argument `{prop}`", t.name));
            }
        }
    }
    assert!(missing.is_empty(), "docs/agents.md's Tools table is missing: {missing:#?}");
}

#[test]
fn autosave_checkpoints_batch_and_partially_failed_tools() {
    let dir = autosave_test_dir("partial");
    let mut s = server().with_autosave(&dir).unwrap();
    let msgs = json!([
        {"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"execute_command","arguments":{"command":"comp.new","params":{"name":"Batch"}}}},
        {"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"execute_command","arguments":{"command":"layer.newText","params":{"text":"Batch title"}}}}
    ]);
    let reply = s.handle(&msgs).unwrap();
    assert_eq!(reply.as_array().unwrap().len(), 2);
    let (_, failed) = call(
        &mut s,
        "command_batch",
        json!({"steps":[{"id":"prop.addKey","params":{"layer":"#1","path":"transform/opacity","time":0,"value":37}},{"id":"invalid.command"}]}),
    );
    assert!(failed, "a non-atomic command sequence keeps its successful first step");
    let status = autosave_status(&mut s);
    let mut recovered = server();
    call_json(&mut recovered, "open_project", json!({"path":status["latestPath"]}));
    let prop = call_json(&mut recovered, "get_property", json!({"layer":"#1","path":"transform/opacity"}));
    assert_eq!(prop["keys"].as_array().unwrap().len(), 1);
    assert_eq!(prop["keys"][0]["value"], 37.0);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn autosave_isolation_also_applies_to_engine_autosaves_and_preference_changes() {
    let dir = autosave_test_dir("engine-save");
    let desktop = dir.join("DesktopCustom");
    std::fs::create_dir_all(&desktop).unwrap();
    std::fs::write(desktop.join("Untitled Project auto-save 1.ecproj"), "desktop backup").unwrap();
    let mut session = Session::default();
    session.prefs.auto_save.location = "custom".into();
    session.prefs.auto_save.folder = desktop.to_string_lossy().into_owned();
    let mut s = McpServer::new(Backend::headless(session)).with_autosave(&dir).unwrap();
    call_json(&mut s, "execute_command", json!({"command":"comp.new","params":{"name":"Isolated"}}));
    let status = autosave_status(&mut s);
    let explicit = call_json(&mut s, "execute_command", json!({"command":"file.autoSave"}));
    assert!(std::path::Path::new(explicit["path"].as_str().unwrap()).starts_with(status["folder"].as_str().unwrap()));
    assert_eq!(autosave_status(&mut s)["latestPath"], explicit["path"]);
    // The host override survives preference edits even within a single batch tool call.
    call_json(
        &mut s,
        "batch",
        json!({"atomic":false,"steps":[
            {"command":"prefs.set","params":{"values":{"autoSave.location":"custom","autoSave.folder":desktop.to_string_lossy()}}},
            {"command":"file.autoSave","params":{}}
        ]}),
    );
    call_json(&mut s, "save_project", json!({"path":dir.join("Named.ecproj")}));
    call_json(&mut s, "execute_command", json!({"command":"layer.newText","params":{"text":"Latest isolated edit"}}));
    let current = autosave_status(&mut s);
    assert!(std::path::Path::new(current["latestPath"].as_str().unwrap()).starts_with(status["folder"].as_str().unwrap()));
    assert!(std::fs::read_to_string(current["latestPath"].as_str().unwrap()).unwrap().contains("Latest isolated edit"));
    let recovery = call_json(&mut s, "execute_command", json!({"command":"file.recoveryInfo"}));
    assert_eq!(recovery["folder"], status["folder"]);
    assert_eq!(std::fs::read_to_string(desktop.join("Untitled Project auto-save 1.ecproj")).unwrap(), "desktop backup");
    assert_eq!(std::fs::read_dir(&desktop).unwrap().count(), 2, "only the original desktop file and the MCP subfolder");
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn autosave_non_utf8_path_returns_an_error_instead_of_panicking() {
    use std::os::unix::ffi::OsStringExt;
    let root = std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/ec-mcp-\xff".to_vec()));
    let result = server().with_autosave(&root);
    assert!(matches!(result, Err(crate::Error::Other(message)) if message.contains("UTF-8")));
}

#[test]
fn core_tools_annotations_and_strict_keys() {
    let mut s = server();
    let tools = rpc(&mut s, 1, "tools/list", json!({}));
    let tools = tools["tools"].as_array().unwrap();
    for name in ["command_list", "command_run", "command_batch", "doc_inspect", "render_preview"] {
        assert!(tools.iter().any(|t| t["name"] == name), "missing {name}");
    }
    for t in tools {
        assert!(t["title"].as_str().is_some_and(|s| !s.is_empty()));
        for hint in ["readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint"] {
            assert!(t["annotations"][hint].is_boolean(), "{t}");
        }
        let msg = json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":t["name"], "arguments":{"typo_key":1}}});
        let reply = s.handle(&msg).unwrap();
        assert_eq!(reply["error"]["code"], -32602, "{reply}");
        assert!(reply["error"]["message"].as_str().unwrap().contains("typo_key"));
    }
    let r = call_json(&mut s, "command_run", json!({"id":"comp.new", "params":{"width":32,"height":32}}));
    assert!(r.is_object());
    let (content, err) = call(&mut s, "command_batch", json!({"steps":[{"id":"no.such.command"},{"id":"layer.newNull"}]}));
    assert!(err);
    let result: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["completed"], 0);
    assert_eq!(result["failed"], 1);
    assert_eq!(result["results"].as_array().unwrap().len(), 1);
    let (content, err) = call(&mut s, "command_batch", json!({"stop_on_error":false,"steps":[{"id":"no.such.command"},{"id":"layer.newNull"}]}));
    assert!(err);
    let result: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["completed"], 1);
    assert_eq!(result["failed"], 1);
    assert!(call_json(&mut s, "doc_inspect", json!({}))["activeComp"].is_object());
    assert!(!call(&mut s, "render_preview", json!({"max_side":16})).1);
}

#[test]
fn resources_match_tools_and_recover_after_parse_error() {
    let mut s = server();
    let bad: Value = serde_json::from_str(&s.handle_line("{broken").unwrap()).unwrap();
    assert_eq!(bad["error"]["code"], -32700);
    assert!(bad["id"].is_null());
    assert_eq!(rpc(&mut s, 1, "ping", json!({})), json!({}));
    let list = rpc(&mut s, 2, "resources/list", json!({}));
    for (uri, tool) in [("effectcraft://document", "doc_inspect"), ("effectcraft://commands", "command_list")] {
        assert!(list["resources"].as_array().unwrap().iter().any(|r| r["uri"] == uri));
        let r = rpc(&mut s, 3, "resources/read", json!({"uri":format!("{uri}?full=true")}));
        assert_eq!(r["contents"][0]["mimeType"], "application/json");
        let value: Value = serde_json::from_str(r["contents"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(value, call_json(&mut s, tool, json!({"full":true})));
    }
}

/// Rendering is a direct tool call, outside Session::execute's existing panic guard.
#[test]
fn render_panic_is_an_error_and_session_remains_usable() {
    use effectcraft_engine::{
        color::Label,
        project::{Footage, FootageKind, ItemId, ItemKind},
        raster::Image,
        render::FootageSource,
        time::Tick,
    };
    use std::sync::{Arc, Mutex};
    struct BrokenFootage(Mutex<bool>);
    impl FootageSource for BrokenFootage {
        fn frame(&self, _: ItemId, _: &Footage, _: Tick) -> Option<Arc<Image>> {
            let mut first = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if *first {
                *first = false;
                panic!("synthetic decoder failure");
            }
            Some(Arc::new(Image::new(16, 16)))
        }
    }
    let mut session = Session { footage: Arc::new(BrokenFootage(Mutex::new(true))), ..Default::default() };
    session.execute("comp.new", json!({"width":16,"height":16})).unwrap();
    let item = Arc::make_mut(&mut session.project).add_item(
        "synthetic",
        Label::Aqua,
        None,
        ItemKind::Footage(Footage { path: "synthetic.png".into(), kind: FootageKind::Still, width: 16, height: 16, has_video: true, ..Default::default() }),
    );
    session.execute("layer.addItem", json!({"item":item.0})).unwrap();
    let mut s = McpServer::new(Backend::headless(session));
    let (content, err) = call(&mut s, "render_frame", json!({}));
    assert!(err);
    assert!(content[0]["text"].as_str().unwrap().contains("synthetic decoder failure"));
    assert_eq!(rpc(&mut s, 2, "ping", json!({})), json!({}));
    assert!(!call(&mut s, "render_frame", json!({})).1);
}

/// MCP 2026-07-28 ("modern") clients declare their revision per request in `_meta` and reject
/// list/read results without `resultType`, `ttlMs` and `cacheScope`; legacy sessions (revision
/// negotiated through `initialize`) keep the old result shape.
#[test]
fn modern_clients_get_result_type() {
    let mut s = server();
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    for (id, method, params) in [
        (1, "tools/list", json!({"_meta": meta})),
        (2, "resources/list", json!({"_meta": meta})),
        (3, "resources/templates/list", json!({"_meta": meta})),
        (4, "resources/read", json!({"_meta": meta, "uri": "effectcraft://document"})),
        (5, "tools/call", json!({"_meta": meta, "name": "command_list", "arguments": {}})),
    ] {
        let r = rpc(&mut s, id, method, params);
        assert_eq!(r["resultType"], "complete", "{method}: {r}");
        if method != "tools/call" {
            assert!(r["ttlMs"].is_u64(), "{method}: {r}");
            assert!(matches!(r["cacheScope"].as_str(), Some("public" | "private")), "{method}: {r}");
        }
    }
    let mut s = server();
    rpc(&mut s, 1, "initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    for (id, method, params) in
        [(2, "tools/list", json!({})), (3, "resources/list", json!({})), (4, "resources/read", json!({"uri": "effectcraft://document"}))]
    {
        let r = rpc(&mut s, id, method, params);
        let res = r.as_object().unwrap();
        assert!(!res.contains_key("resultType") && !res.contains_key("ttlMs") && !res.contains_key("cacheScope"), "{method}: {r}");
    }
}

#[test]
fn render_job_state_recovers_a_poisoned_lock() {
    let shared = std::sync::Arc::new(effectcraft_engine::render_queue::JobShared::default());
    let worker = shared.clone();
    let _ = std::thread::spawn(move || {
        let mut state = worker.state.lock().unwrap();
        state.done = 3;
        panic!("synthetic worker panic while holding the progress lock");
    })
    .join();
    assert_eq!(shared.snapshot().done, 3);
}

/// A render that takes ~2 ms per frame and writes its file first (so a cancel leaves a partial
/// one to clean up).
struct SlowExporter;

impl effectcraft_engine::Exporter for SlowExporter {
    fn formats(&self) -> Vec<effectcraft_engine::project::render_queue::OutputFormat> {
        effectcraft_engine::project::render_queue::OutputFormat::ALL.to_vec()
    }
    fn export(&self, job: &effectcraft_engine::ExportJob, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<effectcraft_engine::ExportResult, String> {
        std::fs::write(job.path, b"partial").map_err(|e| e.to_string())?;
        let n = 2000;
        for k in 0..=n {
            if !progress(k, n) {
                std::fs::remove_file(job.path).map_err(|e| e.to_string())?;
                return Err(effectcraft_engine::render_queue::CANCELLED.into());
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Ok(effectcraft_engine::ExportResult { path: job.path.to_string(), frames: n, ..Default::default() })
    }
}

/// docs/mcp.md "Requests during a render": while a blocking render runs, every way of starting a
/// second render is refused (blocking, `wait: false`, inside a batch), and edits are applied to
/// the project without changing the running job, which renders the snapshot it started with.
#[test]
fn second_render_is_refused_and_edits_apply_during_a_render() {
    let dir = std::env::temp_dir().join(format!("effectcraft-mcp-busy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (in_r, mut in_w) = std::io::pipe().unwrap();
    let (out_r, out_w) = std::io::pipe().unwrap();
    let server = std::thread::spawn(move || {
        let session = Session { exporter: Some(std::sync::Arc::new(SlowExporter)), ..Default::default() };
        McpServer::new(Backend::headless(session)).serve(BufReader::new(in_r), out_w).unwrap();
    });
    let mut lines = BufReader::new(out_r).lines();
    let mut next = || -> Value { serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap() };
    let mut send = |v: Value| writeln!(in_w, "{v}").unwrap();
    let call =
        |id: u64, name: &str, arguments: Value| json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": arguments}});
    send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}));
    next();
    send(call(2, "command_run", json!({"id": "comp.new", "params": {"width": 64, "height": 64, "duration": 1}})));
    assert_eq!(next()["result"]["isError"], false);
    let a = dir.join("a.mp4").to_string_lossy().to_string();
    send(call(3, "command_run", json!({"id": "renderQueue.add", "params": {"output": a}})));
    assert_eq!(next()["result"]["isError"], false);
    send(call(4, "command_run", json!({"id": "renderQueue.render"})));
    send(call(5, "command_run", json!({"id": "renderQueue.render", "params": {"wait": false}})));
    send(call(6, "command_run", json!({"id": "renderQueue.render"})));
    send(call(7, "command_batch", json!({"steps": [{"id": "renderQueue.render", "params": {"wait": false}}]})));
    send(call(8, "command_run", json!({"id": "comp.new", "params": {"name": "Added while rendering", "width": 32, "height": 32, "duration": 1}})));
    let mut replies = std::collections::HashMap::new();
    while !replies.contains_key(&4) {
        let m = next();
        replies.insert(m["id"].as_u64().unwrap(), m);
    }
    for id in [5, 6, 7] {
        let m = &replies[&id];
        assert_eq!(m["result"]["isError"], true, "a second render is refused: {m}");
        let text = m["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("already in progress") || text.contains("is rendering"), "{m}");
    }
    assert_eq!(replies[&8]["result"]["isError"], false, "edits are applied during a render");
    let done = &replies[&4];
    assert_eq!(done["result"]["isError"], false, "{done}");
    let v: Value = serde_json::from_str(done["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(v["items"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["items"][0]["statusLabel"], "Done", "{v}");
    send(call(9, "doc_inspect", json!({})));
    let doc = next();
    assert!(doc.to_string().contains("Added while rendering"), "{doc}");
    drop(in_w);
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// docs/mcp.md "Progress and cancellation" over the stdio loop: a blocking `renderQueue.render`
/// reports `notifications/progress` for its token, other requests are answered while it renders,
/// and `notifications/cancelled` stops it, deletes the partial file and sends no response.
#[test]
fn render_progress_and_cancel() {
    let dir = std::env::temp_dir().join(format!("effectcraft-mcp-progress-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (in_r, mut in_w) = std::io::pipe().unwrap();
    let (out_r, out_w) = std::io::pipe().unwrap();
    let server = std::thread::spawn(move || {
        let session = Session { exporter: Some(std::sync::Arc::new(SlowExporter)), ..Default::default() };
        McpServer::new(Backend::headless(session)).serve(BufReader::new(in_r), out_w).unwrap();
    });
    let mut lines = BufReader::new(out_r).lines();
    let mut next = || -> Value { serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap() };
    let mut send = |v: Value| writeln!(in_w, "{v}").unwrap();
    let run = |id: u64, cmd: &str, params: Value, token: Option<Value>| {
        let mut p = json!({"name": "command_run", "arguments": {"id": cmd, "params": params}});
        if let Some(t) = token {
            p["_meta"] = json!({"progressToken": t, "io.modelcontextprotocol/protocolVersion":"2026-07-28"});
        }
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": p})
    };
    send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}));
    next();
    send(run(2, "comp.new", json!({"width": 64, "height": 64, "duration": 1}), None));
    assert_eq!(next()["result"]["isError"], false);
    let a = dir.join("a.mp4").to_string_lossy().to_string();
    send(run(3, "renderQueue.add", json!({"output": a}), None));
    assert_eq!(next()["result"]["isError"], false);
    // progress for the token, strictly increasing; doc_inspect answered while rendering
    send(run(4, "renderQueue.render", json!({}), Some(json!("tok"))));
    let (mut last, mut notes, mut asked, mut answered) = (-1.0, 0, false, false);
    let done = loop {
        let m = next();
        if m["method"] == "notifications/progress" {
            assert_eq!(m["params"]["progressToken"], "tok", "{m}");
            let p = m["params"]["progress"].as_f64().unwrap();
            assert!(p > last && p <= m["params"]["total"].as_f64().unwrap(), "{m}");
            (last, notes) = (p, notes + 1);
            if !asked && p > 0.0 {
                asked = true;
                send(json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "doc_inspect", "arguments": {}}}));
            }
        } else if m["id"] == 5 {
            answered = true;
            assert_eq!(m["result"]["isError"], false, "{m}");
        } else {
            assert_eq!(m["id"], 4, "{m}");
            break m;
        }
    };
    assert_eq!(done["result"]["isError"], false, "{done}");
    assert_eq!(done["result"]["resultType"], "complete");
    assert_eq!(last, 1.0, "final progress reaches the total");
    let v: Value = serde_json::from_str(done["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(v["items"][0]["statusLabel"], "Done", "{v}");
    assert!(notes >= 2 && answered, "{notes} notifications; doc_inspect answered during the render: {answered}");
    // cancel after the first notification: no response, the partial file is gone
    let b = dir.join("b.mp4");
    send(run(6, "renderQueue.add", json!({"output": b.to_string_lossy()}), None));
    assert_eq!(next()["result"]["isError"], false);
    send(run(7, "renderQueue.render", json!({}), Some(json!(7))));
    loop {
        let m = next();
        assert_ne!(m["id"], 7, "{m}");
        if m["method"] == "notifications/progress" && m["params"]["progress"].as_f64().unwrap() > 0.0 {
            break;
        }
    }
    assert!(b.exists(), "the render started writing");
    send(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 7, "reason": "test"}}));
    send(json!({"jsonrpc": "2.0", "id": 8, "method": "ping"}));
    let m = loop {
        let m = next();
        if m["method"] != "notifications/progress" {
            break m;
        }
    };
    assert_eq!(m["id"], 8, "no response for the cancelled request: {m}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let v = loop {
        send(run(9, "renderQueue.list", json!({}), None));
        let v: Value = serde_json::from_str(next()["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        // the queue stops rendering a moment before the stopped item's status is updated
        if v["rendering"] == false && v["items"][1]["statusLabel"] != "Rendering" {
            break v;
        }
        assert!(std::time::Instant::now() < deadline, "cancel did not stop the job: {v}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(!b.exists(), "partial output deleted");
    assert!(std::path::Path::new(&a).exists(), "completed earlier queue item is retained");
    assert_eq!(v["items"][1]["statusLabel"], "User Stopped", "{v}");
    drop(in_w);
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn render_without_token_finishes_after_stdin_closes() {
    let dir = std::env::temp_dir().join(format!("ec-mcp-eof-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("complete.mp4");
    let mut s = McpServer::new(Backend::headless(Session { exporter: Some(std::sync::Arc::new(SlowExporter)), ..Default::default() }));
    let mut input = String::new();
    for (id, command, params) in
        [(1, "comp.new", json!({"width":16,"height":16})), (2, "renderQueue.add", json!({"output":path})), (3, "renderQueue.render", json!({}))]
    {
        input.push_str(&format!(
            "{}\n",
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"command_run","arguments":{"id":command,"params":params}}})
        ));
    }
    let mut out = Vec::new();
    s.serve(std::io::Cursor::new(input), &mut out).unwrap();
    let replies: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(replies.len(), 3, "no token means no notifications: {replies:?}");
    assert_eq!(replies[2]["result"]["isError"], false);
    assert!(path.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn panicked_render_worker_finishes_with_an_error() {
    struct BrokenExporter;
    impl effectcraft_engine::Exporter for BrokenExporter {
        fn formats(&self) -> Vec<effectcraft_engine::project::render_queue::OutputFormat> {
            effectcraft_engine::project::render_queue::OutputFormat::ALL.to_vec()
        }
        fn export(&self, _: &effectcraft_engine::ExportJob, _: &mut dyn FnMut(u64, u64) -> bool) -> Result<effectcraft_engine::ExportResult, String> {
            panic!("synthetic exporter failure");
        }
    }
    let mut session = Session { exporter: Some(std::sync::Arc::new(BrokenExporter)), ..Default::default() };
    session.execute("comp.new", json!({"width":16,"height":16})).unwrap();
    session.execute("renderQueue.add", json!({"output":"unused.mp4"})).unwrap();
    session.execute("renderQueue.render", json!({"wait":false})).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while session.is_rendering() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!session.is_rendering(), "a panicked worker must not leave an unfinishable job");
    session.poll_render();
    let queue = session.execute("renderQueue.list", json!({})).unwrap();
    assert!(queue.to_string().contains("synthetic exporter failure"), "{queue}");
    session.execute("renderQueue.add", json!({"output":"unused-again.mp4"})).unwrap();
    let mut server = McpServer::new(Backend::headless(session));
    let input =
        json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"command_run","arguments":{"id":"renderQueue.render"}}}).to_string() + "\n";
    let mut out = Vec::new();
    server.serve(std::io::Cursor::new(input), &mut out).unwrap();
    let reply: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(reply["result"]["isError"], true, "{reply}");
}

/// `id` is an accepted alias of `command` (#365 made unknown keys an error).
#[test]
fn command_tools_accept_id_as_an_alias() {
    let mut s = server();
    let d = call_json(&mut s, "describe_command", json!({"id": "comp.new"}));
    assert_eq!(d["id"], "comp.new", "{d}");
    call_json(&mut s, "execute_command", json!({"id": "comp.new", "params": {"name": "Alias", "width": 64, "height": 36}}));
    let (_, err) = call(&mut s, "execute_command", json!({}));
    assert!(err, "neither key is an error");
}

/// A headless server with a 320×180 comp, a red 100×100 solid at the centre ("Red") keyed to
/// move, and a green 40×40 solid at (40, 40) ("Green").
fn render_server() -> McpServer {
    let mut s = server();
    call_json(
        &mut s,
        "execute_command",
        json!({"command": "comp.new", "params": {"name": "Main", "width": 320, "height": 180, "duration": 4, "frameRate": 30}}),
    );
    call_json(&mut s, "execute_command", json!({"command": "layer.newSolid", "params": {"color": "#ff0000", "width": 100, "height": 100, "name": "Red"}}));
    call_json(&mut s, "execute_command", json!({"command": "layer.newSolid", "params": {"color": "#00ff00", "width": 40, "height": 40, "name": "Green"}}));
    call_json(&mut s, "set_property", json!({"layer": "Green", "path": "transform/position", "value": [40, 40]}));
    call_json(
        &mut s,
        "add_keyframe",
        json!({"layer": "Red", "path": "transform/position", "keys": [{"time": 0, "value": [60, 90]}, {"time": 3, "value": [260, 90]}]}),
    );
    s
}

/// (png size, info) of an image reply; panics if the tool failed.
fn call_image(s: &mut McpServer, name: &str, args: Value) -> ((u32, u32), Value) {
    let (c, err) = call(s, name, args.clone());
    assert!(!err, "{name} {args}: {c:?}");
    assert_eq!(c[0]["type"], "image");
    let png = base64::decode(c[0]["data"].as_str().unwrap()).unwrap();
    (png_size(&png).unwrap(), serde_json::from_str(c[1]["text"].as_str().unwrap()).unwrap())
}

fn tool_error(s: &mut McpServer, name: &str, args: Value) -> String {
    let (c, err) = call(s, name, args.clone());
    assert!(err, "{name} {args} should fail: {c:?}");
    c[0]["text"].as_str().unwrap().to_string()
}

#[test]
fn render_frame_region_solo_item_and_overlays() {
    let mut s = render_server();
    // Region at full resolution; max_side applies to the crop.
    let (size, info) = call_image(&mut s, "render_frame", json!({"region": [100, 60, 80, 40], "max_side": 0}));
    assert_eq!(size, (80, 40));
    assert_eq!(info["region"], json!([100.0, 60.0, 80.0, 40.0]));
    let (size, _) = call_image(&mut s, "render_preview", json!({"region": [0, 0, 160, 90], "max_side": 40}));
    assert_eq!(size, (40, 23));
    // Solo layer and a solid item on its own.
    let (_, info) = call_image(&mut s, "render_frame", json!({"layer": "Green", "max_side": 64}));
    assert!(info["layer"].is_u64());
    let proj = call_json(&mut s, "get_project", json!({}));
    let solid = proj["items"].as_array().unwrap().iter().find(|i| i["name"].as_str().is_some_and(|n| n.contains("Red"))).unwrap()["id"].clone();
    let (size, info) = call_image(&mut s, "render_frame", json!({"item": solid, "max_side": 0}));
    assert_eq!(size, (100, 100));
    assert_eq!(info["item"], solid);
    // Overlays report each drawn layer's comp-space bounds.
    let (_, info) = call_image(&mut s, "render_frame", json!({"time": 1.5, "overlays": true, "max_side": 320}));
    let red = info["layers"].as_array().unwrap().iter().find(|l| l["name"] == "Red").unwrap().clone();
    assert_eq!(red["bounds"], json!([110.0, 40.0, 100.0, 100.0]));
    let (_, info) = call_image(&mut s, "render_preview", json!({"overlays": ["motionPath", "safeMargins", "grid"]}));
    assert_eq!(info["overlays"], json!(["motionPath", "safeMargins", "grid"]));
    // Bad input is an error, never a crash; the session keeps serving.
    assert!(tool_error(&mut s, "render_frame", json!({"region": [500, 500, 10, 10]})).contains("outside"));
    assert!(tool_error(&mut s, "render_frame", json!({"region": [0, 0, 0, 10]})).contains("positive"));
    assert!(tool_error(&mut s, "render_frame", json!({"region": [0, 0]})).contains("region"));
    assert!(tool_error(&mut s, "render_frame", json!({"layer": "Nope"})).contains("no layer"));
    assert!(tool_error(&mut s, "render_frame", json!({"item": 9999})).contains("no project item"));
    assert!(tool_error(&mut s, "render_frame", json!({"overlays": ["sparkles"]})).contains("unknown overlay"));
    assert!(tool_error(&mut s, "render_frame", json!({"max_side": -5})).contains("max_side"));
    let (size, _) = call_image(&mut s, "render_frame", json!({"max_side": u64::MAX}));
    assert_eq!(size, (320, 180), "a huge max_side is clamped to the comp size");
}

#[test]
fn render_frames_contact_sheet() {
    let mut s = render_server();
    let (size, info) = call_image(&mut s, "render_frames", json!({"count": 4, "max_side": 800}));
    assert_eq!(info["frames"], json!([0, 40, 79, 119]));
    assert_eq!(size, (info["width"].as_u64().unwrap() as u32, info["height"].as_u64().unwrap() as u32));
    assert!(size.0 <= 800 && size.1 <= 800);
    assert!(info.get("png").is_none(), "the PNG travels as the image, not in the JSON");
    let (_, info) = call_image(&mut s, "render_frames", json!({"times": [0, 1, 2], "layer": "Red", "overlays": ["motionPath"], "cols": 3, "labels": false}));
    assert_eq!((info["cols"].as_u64(), info["rows"].as_u64()), (Some(3), Some(1)));
    // Huge counts are capped, not trusted.
    let (_, info) = call_image(&mut s, "render_frames", json!({"count": 1_000_000_000u64, "max_side": 256}));
    assert_eq!(info["count"], 64);
    assert_eq!(info["requested"], 1_000_000_000u64);
    assert!(tool_error(&mut s, "render_frames", json!({"count": 0})).contains("at least 1"));
    assert!(tool_error(&mut s, "render_frames", json!({"times": []})).contains("empty"));
    assert!(tool_error(&mut s, "render_frames", json!({"start": 3, "end": 1})).contains("before"));
    assert!(tool_error(&mut s, "render_frames", json!({"region": [400, 0, 10, 10]})).contains("outside"));
    // Files: the sheet and an animated GIF.
    let dir = std::env::temp_dir().join(format!("ec-mcp-frames-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (png, gif) = (dir.join("sheet.png"), dir.join("anim.gif"));
    let info =
        call_json(&mut s, "render_frames", json!({"count": 3, "max_side": 300, "path": png.to_string_lossy(), "gif": gif.to_string_lossy(), "inline": false}));
    assert_eq!(info["gif"], json!(gif.to_string_lossy()));
    assert!(std::fs::read(&png).unwrap().starts_with(b"\x89PNG"));
    assert!(std::fs::read(&gif).unwrap().starts_with(b"GIF89a"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn bridge_render_frames_and_render_params() {
    let port = fake_app();
    let mut s = McpServer::new(Backend::bridge(&port.to_string()).unwrap());
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "B", "width": 64, "height": 32}}));
    // The contact sheet runs the engine command in the app.
    let (size, info) = call_image(&mut s, "render_frames", json!({"count": 2, "max_side": 200}));
    assert_eq!(info["count"], 2);
    assert_eq!(size.0 as u64, info["width"].as_u64().unwrap());
    // render_frame forwards the new arguments to the app's render.frame (the fake answers a 4×2 PNG).
    let (size, _) = call_image(&mut s, "render_frame", json!({"region": [0, 0, 8, 8], "overlays": true}));
    assert_eq!(size, (4, 2));
}

/// `get_layout`: comp-space geometry of layers, headless and bridged.
#[test]
fn get_layout_reports_layer_geometry() {
    for bridge in [false, true] {
        let mut s = if bridge { McpServer::new(Backend::bridge(&fake_app().to_string()).unwrap()) } else { server() };
        call_json(&mut s, "command_run", json!({"id": "comp.new", "params": {"width": 400, "height": 300}}));
        let l = call_json(&mut s, "command_run", json!({"id": "layer.newSolid", "params": {"width": 100, "height": 50}}))["layer"].clone();
        call_json(&mut s, "set_property", json!({"layer": l, "path": "transform/position", "value": [0, 150]}));
        let r = call_json(&mut s, "get_layout", json!({"layers": [l], "overlaps": true}));
        let g = &r["layers"][0];
        assert_eq!(g["bounds"], json!([-50.0, 125.0, 100.0, 50.0]), "{r}");
        assert_eq!(g["onScreen"], 0.5);
        assert_eq!(g["visible"], true);
        assert_eq!(r["overlaps"], json!([]));
        // A single layer reference works too; bad references are tool errors.
        assert_eq!(call_json(&mut s, "get_layout", json!({"layers": "#1"}))["layers"].as_array().unwrap().len(), 1);
        let (c, err) = call(&mut s, "get_layout", json!({"layers": ["Nope"]}));
        assert!(err && c[0]["text"].as_str().unwrap().contains("no layer"), "{c:?}");
    }
}

/// `lint_frame`: design problems as structured JSON, headless and bridged.
#[test]
fn lint_frame_reports_design_problems() {
    for bridge in [false, true] {
        let mut s = if bridge { McpServer::new(Backend::bridge(&fake_app().to_string()).unwrap()) } else { server() };
        call_json(&mut s, "command_run", json!({"id": "comp.new", "params": {"width": 320, "height": 180, "frameRate": 25, "duration": 2}}));
        let l = call_json(&mut s, "command_run", json!({"id": "layer.newSolid", "params": {"width": 40, "height": 40}}))["layer"].clone();
        call_json(&mut s, "set_property", json!({"layer": l, "path": "transform/position", "value": [0, 90]}));
        let r = call_json(&mut s, "lint_frame", json!({"time": 0, "rules": ["cut-off"]}));
        assert_eq!(r["problems"].as_array().unwrap().len(), 1, "{r}");
        let p = &r["problems"][0];
        assert_eq!((p["rule"].as_str(), p["severity"].as_str(), &p["layer"]), (Some("cut-off"), Some("info"), &l), "{r}");
        assert_eq!(r["clean"], true);
        assert_eq!(r["counts"], json!({"error": 0, "warning": 0, "info": 1}));
        // A range and snake_case thresholds; bad rules are tool errors.
        let r = call_json(&mut s, "lint_frame", json!({"start": 0, "end": 1, "step": 0.5, "min_text_px": 10, "rules": "cut-off"}));
        assert_eq!(r["times"], json!([0.0, 0.5, 1.0]), "{r}");
        assert_eq!((r["problems"][0]["time"].as_f64(), r["problems"][0]["until"].as_f64()), (Some(0.0), Some(1.0)), "{r}");
        let (c, err) = call(&mut s, "lint_frame", json!({"rules": ["nope"]}));
        assert!(err && c[0]["text"].as_str().unwrap().contains("unknown rule"), "{c:?}");
    }
}

/// `get_motion`: a motion summary of keyframed layers, headless and bridged.
#[test]
fn get_motion_describes_layer_motion() {
    for bridge in [false, true] {
        let mut s = if bridge { McpServer::new(Backend::bridge(&fake_app().to_string()).unwrap()) } else { server() };
        call_json(&mut s, "command_run", json!({"id": "comp.new", "params": {"width": 320, "height": 180, "frameRate": 24, "duration": 2}}));
        let l = call_json(&mut s, "command_run", json!({"id": "layer.newSolid", "params": {"name": "Logo", "width": 80, "height": 44}}))["layer"].clone();
        call_json(
            &mut s,
            "add_keyframe",
            json!({"layer": l, "path": "transform/position", "keys": [{"time": 0, "value": [-60, 90]}, {"time": 1, "value": [160, 90]}]}),
        );
        let r = call_json(&mut s, "get_motion", json!({}));
        assert_eq!(r["truncated"], false, "{r}");
        let m = &r["layers"][0];
        assert_eq!(m["segments"][0]["kind"], "enter", "{r}");
        assert_eq!(m["segments"][0]["edge"], "left");
        assert!(m["summary"].as_str().unwrap().contains("from the left"), "{r}");
        // `layer` routes to layer.motion; bad references are tool errors.
        let r = call_json(&mut s, "get_motion", json!({"layer": "Logo", "start": 0.5}));
        assert_eq!((r["layers"].as_array().unwrap().len(), r["start"].clone()), (1, json!(0.5)));
        let (c, err) = call(&mut s, "get_motion", json!({"layers": ["Nope"]}));
        assert!(err && c[0]["text"].as_str().unwrap().contains("no layer"), "{c:?}");
    }
    assert!(crate::tools::output_schema("get_motion").is_some());
}

/// `analyze_audio`: levels of a Tone, a waveform image, no-audio comps.
#[test]
fn analyze_audio_levels_and_waveform() {
    let mut s = server();
    call_json(&mut s, "command_run", json!({"id": "comp.new", "params": {"width": 64, "height": 36, "duration": 1}}));
    let r = call_json(&mut s, "analyze_audio", json!({}));
    assert_eq!(r["hasAudio"], false, "{r}");
    let l = call_json(&mut s, "command_run", json!({"id": "layer.newSolid", "params": {}}))["layer"].clone();
    let values = json!({"frequency1": 1000, "frequency2": 0, "frequency3": 0, "frequency4": 0, "frequency5": 0, "level": 25});
    call_json(&mut s, "add_effect", json!({"layer": l, "effect": "Tone", "values": values}));
    let r = call_json(&mut s, "analyze_audio", json!({}));
    assert!((r["peakDbfs"].as_f64().unwrap() + 12.04).abs() < 0.05, "{r}");
    assert!((r["integratedLufs"].as_f64().unwrap() + 12.04).abs() < 0.2, "{r}");
    let (c, err) = call(&mut s, "analyze_audio", json!({"waveform": true, "width": 120, "height": 50}));
    assert!(!err, "{c:?}");
    assert_eq!(png_size(&base64::decode(c[0]["data"].as_str().unwrap()).unwrap()), Some((120, 50)));
    let info: Value = serde_json::from_str(c[1]["text"].as_str().unwrap()).unwrap();
    assert!(info["waveform"].get("columns").is_none(), "columns become the image");
    let (_, err) = call(&mut s, "analyze_audio", json!({"start": 0.5, "end": 0.5}));
    assert!(err);
}

/// Send a request that must fail; returns the JSON-RPC error code.
fn rpc_err(s: &mut McpServer, method: &str, params: Value) -> i64 {
    let line = json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}).to_string();
    let reply: Value = serde_json::from_str(&s.handle_line(&line).expect("reply")).unwrap();
    assert!(reply.get("result").is_none(), "{method} {params}: {reply}");
    reply["error"]["code"].as_i64().unwrap()
}

/// Resource templates: comp, layer, frame (a PNG blob) and command; names are percent-decoded,
/// and unknown or malformed URIs are errors, never panics.
#[test]
fn resource_templates_read_and_reject_bad_uris() {
    use crate::resources::encode;
    let mut s = server();
    rpc(&mut s, 1, "initialize", json!({"protocolVersion": "2025-06-18"}));
    let t = rpc(&mut s, 2, "resources/templates/list", json!({}));
    let uris: Vec<&str> = t["resourceTemplates"].as_array().unwrap().iter().map(|t| t["uriTemplate"].as_str().unwrap()).collect();
    assert_eq!(
        uris,
        [
            "effectcraft://comp/{comp}",
            "effectcraft://layer/{comp}/{layer}",
            "effectcraft://frame/{comp}/{time}",
            "effectcraft://command/{id}",
            "effectcraft://result/{result_id}{?pointer,mode,offset,limit,max_chars}"
        ]
    );
    assert!(t["resourceTemplates"][0]["title"].is_string());

    call_json(&mut s, "command_run", json!({"id": "comp.new", "params": {"name": "Main / Ünïcode", "width": 64, "height": 32, "duration": 2}}));
    call_json(&mut s, "command_run", json!({"id": "layer.newSolid", "params": {"name": "Red bar", "color": "#ff0000"}}));
    let comp = format!("effectcraft://comp/{}", encode("Main / Ünïcode"));
    assert!(comp.contains("%2F"), "{comp}");
    let read = |s: &mut McpServer, uri: &str| -> Value {
        let r = rpc(s, 3, "resources/read", json!({"uri": uri}));
        assert_eq!(r["contents"][0]["uri"], uri);
        r["contents"][0].clone()
    };
    let json_of = |c: &Value| -> Value { serde_json::from_str(c["text"].as_str().unwrap()).unwrap() };
    let c = read(&mut s, &comp);
    assert_eq!(c["mimeType"], "application/json");
    assert_eq!(json_of(&c), call_json(&mut s, "get_comp", json!({"comp": "Main / Ünïcode"})));
    let id = json_of(&c)["id"].as_u64().unwrap();
    assert_eq!(json_of(&read(&mut s, &format!("effectcraft://comp/{id}")))["id"], id, "numeric segment = item id");

    let tree = json_of(&read(&mut s, &format!("{}/{}", comp.replace("/comp/", "/layer/"), encode("Red bar"))));
    assert_eq!(tree["name"], "Red bar");
    assert!(tree["properties"].is_object());
    let flat = json_of(&read(&mut s, &format!("effectcraft://layer/{id}/%231?flat=true")));
    assert!(flat["properties"].as_array().unwrap().iter().any(|p| p["path"] == "transform/opacity"), "{flat}");

    let frame = read(&mut s, &format!("effectcraft://frame/{id}/0.5"));
    assert_eq!(frame["mimeType"], "image/png");
    assert!(frame.get("text").is_none());
    assert_eq!(png_size(&base64::decode(frame["blob"].as_str().unwrap()).unwrap()), Some((64, 32)));
    let small = read(&mut s, &format!("effectcraft://frame/{id}/0?max_side=16"));
    assert_eq!(png_size(&base64::decode(small["blob"].as_str().unwrap()).unwrap()), Some((16, 8)));

    let cmd = json_of(&read(&mut s, "effectcraft://command/layer.newText"));
    assert_eq!(cmd["id"], "layer.newText");

    for (uri, code) in [
        ("effectcraft://nope/1", -32002),
        ("https://example.com/", -32002),
        ("effectcraft://comp/No%20such%20comp", -32002),
        ("effectcraft://command/no.such", -32002),
        (&format!("effectcraft://layer/{id}/Nobody") as &str, -32002),
        ("effectcraft://comp/", -32602),
        ("effectcraft://comp/a/b", -32602),
        ("effectcraft://comp/%zz", -32602),
        ("effectcraft://comp/%E2%28", -32602),
        ("effectcraft://comp/%", -32602),
        ("effectcraft://layer/1", -32602),
        (&format!("effectcraft://frame/{id}/soon") as &str, -32602),
        (&format!("effectcraft://frame/{id}/NaN") as &str, -32602),
        (&format!("effectcraft://frame/{id}/1?max_side=-3") as &str, -32602),
        (&format!("effectcraft://frame/{id}/1?bogus=1") as &str, -32602),
        (&format!("effectcraft://layer/{id}/%231?flat=maybe") as &str, -32602),
    ] {
        assert_eq!(rpc_err(&mut s, "resources/read", json!({"uri": uri})), code, "{uri}");
    }
    assert_eq!(rpc_err(&mut s, "resources/read", json!({})), -32602);
    assert_eq!(rpc(&mut s, 4, "ping", json!({})), json!({}));
}

/// Workflow prompts: listed with their arguments, rendered with the tool flow, and argument
/// mistakes are invalid params.
#[test]
fn prompts_list_and_get() {
    let mut s = server();
    let init = rpc(&mut s, 1, "initialize", json!({"protocolVersion": "2025-06-18"}));
    assert!(init["capabilities"]["prompts"].is_object());
    let list = rpc(&mut s, 2, "prompts/list", json!({}));
    let prompts = list["prompts"].as_array().unwrap();
    let names: Vec<&str> = prompts.iter().map(|p| p["name"].as_str().unwrap()).collect();
    for want in ["title_card", "lower_third", "port_ae_script", "check_against_reference", "animate_layer"] {
        assert!(names.contains(&want), "missing {want}");
    }
    for p in prompts {
        assert!(p["title"].is_string() && p["description"].is_string(), "{p}");
        assert!(p["arguments"].as_array().unwrap().iter().any(|a| a["required"] == true), "{p}");
    }
    let r = rpc(&mut s, 3, "prompts/get", json!({"name": "title_card", "arguments": {"text": "Hello World"}}));
    assert_eq!(r["messages"][0]["role"], "user");
    let text = r["messages"][0]["content"]["text"].as_str().unwrap();
    for needle in ["Hello World", "command_run comp.new", "layer.newText", "add_keyframe", "render_preview", "layer time"] {
        assert!(text.contains(needle), "{needle}: {text}");
    }
    let r = rpc(&mut s, 4, "prompts/get", json!({"name": "check_against_reference", "arguments": {"reference_path": "/tmp/ref.png", "time": "1.5"}}));
    let text = r["messages"][0]["content"]["text"].as_str().unwrap();
    assert!(text.contains("render_compare") && text.contains("/tmp/ref.png") && text.contains("1.5 s"), "{text}");
    let r = rpc(&mut s, 5, "prompts/get", json!({"name": "port_ae_script", "arguments": {"script": "app.project.items.addComp('X', 10, 10, 1, 1, 24);"}}));
    assert!(r["messages"][0]["content"]["text"].as_str().unwrap().contains("run_script"));
    for params in [
        json!({"name": "lower_third", "arguments": {"name": "Ada"}}),
        json!({"name": "lower_third", "arguments": {"name": "Ada", "title": "   "}}),
        json!({"name": "title_card"}),
        json!({"name": "title_card", "arguments": {"text": "x", "colour": "red"}}),
        json!({"name": "title_card", "arguments": {"text": 5}}),
        json!({"name": "title_card", "arguments": "text"}),
        json!({"name": "no_such_prompt"}),
        json!({}),
    ] {
        assert_eq!(rpc_err(&mut s, "prompts/get", params.clone()), -32602, "{params}");
    }
    // Older revisions don't get `title`.
    let mut s = server();
    rpc(&mut s, 1, "initialize", json!({"protocolVersion": "2025-03-26"}));
    assert!(rpc(&mut s, 2, "prompts/list", json!({}))["prompts"][0].get("title").is_none());
}

/// The older duplicate tool names are left out of tools/list (unless `with_all_tools`), stay
/// callable with their own arguments, and the canonical tools take everything they did.
#[test]
fn hidden_aliases_are_callable_but_not_listed() {
    let names = |s: &mut McpServer| -> Vec<String> {
        rpc(s, 1, "tools/list", json!({}))["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
    };
    let mut s = server();
    let listed = names(&mut s);
    for (alias, canonical) in crate::tools::HIDDEN_ALIASES {
        assert!(!listed.iter().any(|n| n == alias), "{alias} is listed");
        assert!(listed.iter().any(|n| n == canonical), "{canonical} is missing");
        // The canonical tool accepts every argument of the alias (except execute_command's `command`).
        let schema = |n: &str| crate::tools::find(n).unwrap().input_schema();
        let (a, c) = (schema(alias), schema(canonical));
        for k in a["properties"].as_object().unwrap().keys().filter(|k| *k != "command") {
            assert!(c["properties"].get(k).is_some(), "{canonical} lacks `{k}` of {alias}");
        }
    }
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "Alias", "width": 32, "height": 16}}));
    assert!(call_json(&mut s, "list_commands", json!({"filter": "comp.new"})).as_array().unwrap().iter().any(|c| c["id"] == "comp.new"));
    assert_eq!(call_json(&mut s, "get_project", json!({}))["items"], call_json(&mut s, "doc_inspect", json!({}))["project"]["items"]);
    let (c, err) = call(&mut s, "render_frame", json!({"max_side": 16}));
    assert!(!err && c[0]["type"] == "image", "{c:?}");
    // render_preview took over render_frame's `path` and `inline`.
    let png = std::env::temp_dir().join(format!("ec-alias-{}.png", std::process::id()));
    let info = call_json(&mut s, "render_preview", json!({"max_side": 16, "inline": false, "path": png.to_string_lossy()}));
    assert_eq!((info["width"].as_u64(), info["height"].as_u64()), (Some(16), Some(8)));
    assert_eq!(png_size(&std::fs::read(&png).unwrap()), Some((16, 8)));
    let _ = std::fs::remove_file(png);

    let mut s = server().with_all_tools(true);
    let tools = rpc(&mut s, 1, "tools/list", json!({}))["tools"].as_array().unwrap().clone();
    for (alias, canonical) in crate::tools::HIDDEN_ALIASES {
        let t = tools.iter().find(|t| t["name"] == *alias).unwrap_or_else(|| panic!("{alias} not listed with --all-tools"));
        assert!(t["description"].as_str().unwrap().starts_with(&format!("Alias of {canonical}")), "{t}");
    }
    // The server's instructions name only canonical tools.
    let init = rpc(&mut s, 2, "initialize", json!({"protocolVersion": "2025-06-18"}));
    let text = init["instructions"].as_str().unwrap();
    for (alias, _) in crate::tools::HIDDEN_ALIASES {
        assert!(!text.contains(alias), "instructions mention {alias}");
    }
}

/// The `uri`s of queued `notifications/resources/updated`.
fn updated(s: &mut McpServer) -> Vec<String> {
    s.take_notifications()
        .iter()
        .filter(|n| n["method"] == "notifications/resources/updated")
        .map(|n| n["params"]["uri"].as_str().unwrap().to_string())
        .collect()
}

/// Subscribed project resources are reported updated after an edit (and undo), never after a
/// query; command resources don't follow the project; `serve` writes them after the reply.
#[test]
fn subscriptions_notify_after_edits_not_queries() {
    let mut s = server();
    let init = rpc(&mut s, 1, "initialize", json!({"protocolVersion": "2025-06-18"}));
    assert_eq!(init["capabilities"]["resources"]["subscribe"], true);
    assert!(init["capabilities"]["logging"].is_object());
    call_json(&mut s, "command_run", json!({"id": "comp.new", "params": {"name": "Sub", "width": 32, "height": 32}}));
    s.take_notifications();
    for uri in ["effectcraft://document", "effectcraft://comp/Sub", "effectcraft://commands", "effectcraft://command/comp.new"] {
        assert_eq!(rpc(&mut s, 2, "resources/subscribe", json!({"uri": uri})), json!({}));
    }
    assert_eq!(rpc_err(&mut s, "resources/subscribe", json!({"uri": "effectcraft://nope"})), -32002);
    assert_eq!(rpc_err(&mut s, "resources/subscribe", json!({"uri": "effectcraft://comp/%zz"})), -32602);
    assert_eq!(rpc_err(&mut s, "resources/subscribe", json!({})), -32602);

    call_json(&mut s, "get_comp", json!({"comp": "Sub"}));
    call_json(&mut s, "doc_inspect", json!({}));
    rpc(&mut s, 3, "resources/read", json!({"uri": "effectcraft://document"}));
    assert!(updated(&mut s).is_empty(), "queries change nothing");

    call_json(&mut s, "command_run", json!({"id": "layer.newNull"}));
    assert_eq!(updated(&mut s), ["effectcraft://comp/Sub", "effectcraft://document"]);
    call_json(&mut s, "undo", json!({}));
    assert_eq!(updated(&mut s).len(), 2, "undo changes the project too");
    // A value set to what it already is isn't an edit.
    call_json(&mut s, "command_run", json!({"id": "comp.settings", "params": {"comp": "Sub", "width": 32}}));
    assert!(updated(&mut s).is_empty());

    rpc(&mut s, 4, "resources/unsubscribe", json!({"uri": "effectcraft://document"}));
    rpc(&mut s, 5, "resources/unsubscribe", json!({"uri": "effectcraft://comp/Sub"}));
    rpc(&mut s, 6, "resources/unsubscribe", json!({"uri": "effectcraft://never-subscribed"}));
    call_json(&mut s, "command_run", json!({"id": "layer.newNull"}));
    assert!(updated(&mut s).is_empty());

    // Over stdio the notification follows the reply.
    let mut s = server();
    let input = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "resources/subscribe", "params": {"uri": "effectcraft://document"}}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "command_run", "arguments": {"id": "comp.new"}}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "ping"}),
    ]
    .iter()
    .map(|v| v.to_string() + "\n")
    .collect::<String>();
    let mut out = Vec::new();
    s.serve(std::io::Cursor::new(input), &mut out).unwrap();
    let lines: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 5, "{lines:?}");
    assert_eq!(lines[2]["id"], 3);
    assert_eq!(lines[3], json!({"jsonrpc": "2.0", "method": "notifications/resources/updated", "params": {"uri": "effectcraft://document"}}));
    assert_eq!(lines[4]["id"], 4);
}

/// A retried tool call with the same request id returns the first result and doesn't run again
/// (no duplicate comp, no second change notification); `request_id` is not an unknown argument.
#[test]
fn a_repeated_request_id_runs_once() {
    let mut s = server();
    let revision = |s: &mut McpServer| s.backend().session().unwrap().revision;
    rpc(&mut s, 1, "resources/subscribe", json!({"uri": "effectcraft://document"}));
    let call = json!({"name": "command_run", "arguments": {"id": "comp.new", "params": {"name": "Dup"}}, "_meta": {"requestId": "agent-1"}});
    let first = rpc(&mut s, 2, "tools/call", call.clone());
    assert_eq!(first["isError"], false, "{first}");
    assert_eq!(updated(&mut s), ["effectcraft://document"]);
    let rev = revision(&mut s);
    assert_eq!(rpc(&mut s, 3, "tools/call", call), first);
    assert_eq!(revision(&mut s), rev, "the retry must not run");
    assert!(updated(&mut s).is_empty());

    let call = json!({"name": "command_run", "arguments": {"id": "comp.new", "params": {}, "request_id": "agent-2"}});
    let first = rpc(&mut s, 4, "tools/call", call.clone());
    assert_eq!(first["isError"], false, "{first}");
    let rev = revision(&mut s);
    assert_eq!(rpc(&mut s, 5, "tools/call", call), first);
    assert_eq!(revision(&mut s), rev);
    // Another id runs.
    rpc(&mut s, 6, "tools/call", json!({"name": "command_run", "arguments": {"id": "comp.new", "params": {}, "request_id": "agent-3"}}));
    assert!(revision(&mut s) > rev);
}

/// `notifications/message`: new expression errors after an edit and missing footage after
/// open_project are warnings, toasts are info; `logging/setLevel` filters them.
#[test]
fn logging_levels_filter_warnings() {
    let messages = |s: &mut McpServer| -> Vec<Value> {
        s.take_notifications().into_iter().filter(|n| n["method"] == "notifications/message").map(|n| n["params"].clone()).collect()
    };
    let mut s = McpServer::new(Backend::headless(effectcraft_host::session()));
    rpc(&mut s, 1, "initialize", json!({"protocolVersion": "2025-03-26"}));
    call_json(&mut s, "command_run", json!({"id": "comp.new", "params": {"name": "L", "width": 64, "height": 64}}));
    call_json(&mut s, "command_run", json!({"id": "layer.newSolid", "params": {"name": "S", "color": "#ff0000"}}));
    assert!(messages(&mut s).is_empty());
    assert_eq!(rpc_err(&mut s, "logging/setLevel", json!({"level": "loud"})), -32602);
    assert_eq!(rpc_err(&mut s, "logging/setLevel", json!({})), -32602);

    assert_eq!(rpc(&mut s, 2, "logging/setLevel", json!({"level": "error"})), json!({}));
    call_json(&mut s, "set_property", json!({"layer": "S", "path": "transform/rotation", "expression": "nope*2"}));
    assert!(messages(&mut s).is_empty(), "warnings are below `error`");

    rpc(&mut s, 3, "logging/setLevel", json!({"level": "warning"}));
    call_json(&mut s, "set_property", json!({"layer": "S", "path": "transform/opacity", "value": 50}));
    let m = messages(&mut s);
    assert_eq!(m.len(), 1, "{m:?}");
    assert_eq!((&m[0]["level"], &m[0]["logger"]), (&json!("warning"), &json!("effectcraft")));
    assert!(m[0]["data"]["errors"][0].as_str().unwrap().contains("nope"), "{m:?}");
    call_json(&mut s, "set_property", json!({"layer": "S", "path": "transform/opacity", "value": 40}));
    assert!(messages(&mut s).is_empty(), "the same error isn't repeated");
    call_json(&mut s, "get_property", json!({"layer": "S", "path": "transform/opacity"}));
    assert!(messages(&mut s).is_empty(), "queries log nothing");

    // Missing footage after open_project.
    let session = s.backend().session().unwrap();
    let footage = effectcraft_engine::project::Footage { path: "/nonexistent/ec/gone.png".into(), width: 8, height: 8, has_video: true, ..Default::default() };
    std::sync::Arc::make_mut(&mut session.project).add_item(
        "gone",
        effectcraft_engine::color::Label::None,
        None,
        effectcraft_engine::project::ItemKind::Footage(footage),
    );
    let path = std::env::temp_dir().join(format!("ec-mcp-log-{}.ecproj", std::process::id())).to_string_lossy().into_owned();
    call_json(&mut s, "save_project", json!({"path": path}));
    call_json(&mut s, "open_project", json!({"path": path}));
    let m = messages(&mut s);
    assert!(m.iter().any(|m| m["level"] == "warning" && m["data"]["missing"] == 1), "{m:?}");
    // Toasts are info.
    rpc(&mut s, 4, "logging/setLevel", json!({"level": "info"}));
    call_json(&mut s, "command_run", json!({"id": "footage.check", "params": {"wait": true}}));
    let m = messages(&mut s);
    assert!(m.iter().any(|m| m["level"] == "info" && m["data"]["message"].as_str().unwrap().contains("missing")), "{m:?}");
    rpc(&mut s, 5, "logging/setLevel", json!({"level": "emergency"}));
    call_json(&mut s, "open_project", json!({"path": path}));
    assert!(messages(&mut s).is_empty());
    let _ = std::fs::remove_file(path);
}

/// A minimal output-schema check: type, anyOf, const, required, properties and items.
fn validate(schema: &Value, v: &Value, at: &str) -> Result<(), String> {
    if let Some(t) = schema.get("type") {
        let types: Vec<&str> = match t {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        let ok = types.iter().any(|t| match *t {
            "object" => v.is_object(),
            "array" => v.is_array(),
            "string" => v.is_string(),
            "integer" => v.is_i64() || v.is_u64(),
            "number" => v.is_number(),
            "boolean" => v.is_boolean(),
            "null" => v.is_null(),
            _ => false,
        });
        if !ok {
            return Err(format!("{at}: {v} is not {t}"));
        }
    }
    if let Some(variants) = schema.get("anyOf").and_then(Value::as_array)
        && !variants.iter().any(|variant| validate(variant, v, at).is_ok())
    {
        return Err(format!("{at}: does not match any anyOf variant"));
    }
    if let Some(expected) = schema.get("const")
        && v != expected
    {
        return Err(format!("{at}: expected constant {expected}, got {v}"));
    }
    for r in schema.get("required").and_then(Value::as_array).into_iter().flatten() {
        if v.get(r.as_str().unwrap()).is_none() {
            return Err(format!("{at}: missing `{r}` in {v}"));
        }
    }
    if let (Some(props), Some(o)) = (schema.get("properties").and_then(Value::as_object), v.as_object()) {
        for (k, s) in props {
            if let Some(x) = o.get(k) {
                validate(s, x, &format!("{at}.{k}"))?;
            }
        }
    }
    if let (Some(items), Some(a)) = (schema.get("items"), v.as_array()) {
        for (i, x) in a.iter().enumerate() {
            validate(items, x, &format!("{at}[{i}]"))?;
        }
    }
    Ok(())
}

/// MCP 2025-06-18: JSON results also come as `structuredContent` (always an object) and the
/// stable ones declare an `outputSchema` it validates against; older revisions get neither, and
/// keep the text content either way.
#[test]
fn structured_content_is_version_gated_and_matches_output_schemas() {
    for (version, structured) in [("2025-06-18", true), ("2025-03-26", false), ("2024-11-05", false)] {
        let mut s = server();
        rpc(&mut s, 1, "initialize", json!({"protocolVersion": version}));
        let tools = rpc(&mut s, 2, "tools/list", json!({}))["tools"].as_array().unwrap().clone();
        let declared = tools.iter().filter(|t| t.get("outputSchema").is_some()).count();
        assert_eq!(declared > 0, structured, "{version}");
        let r = rpc(&mut s, 3, "tools/call", json!({"name": "command_run", "arguments": {"id": "comp.new", "params": {"width": 32, "height": 32}}}));
        assert_eq!(r.get("structuredContent").is_some(), structured, "{version}: {r}");
        assert_eq!(r["content"][0]["type"], "text", "{version}: text content stays");
    }
    // A modern request (revision in `_meta`, no initialize) gets them too.
    let mut s = server();
    let meta = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28"});
    let r = rpc(&mut s, 1, "tools/call", json!({"_meta": meta, "name": "get_state", "arguments": {}}));
    assert!(r["structuredContent"]["state"].is_object(), "{r}");

    let mut s = server();
    rpc(&mut s, 1, "initialize", json!({"protocolVersion": "2025-06-18"}));
    let tools = rpc(&mut s, 2, "tools/list", json!({}))["tools"].as_array().unwrap().clone();
    let schema = |n: &str| tools.iter().find(|t| t["name"] == n).and_then(|t| t.get("outputSchema")).cloned().unwrap_or_else(|| panic!("{n}: no outputSchema"));
    for t in &tools {
        if let Some(o) = t.get("outputSchema") {
            assert_eq!(o["type"], "object", "{t}");
        }
    }
    let path = std::env::temp_dir().join(format!("ec-structured-{}.ecproj", std::process::id())).to_string_lossy().into_owned();
    let png = std::env::temp_dir().join(format!("ec-structured-{}.png", std::process::id())).to_string_lossy().into_owned();
    let calls = [
        ("command_run", json!({"id": "comp.new", "params": {"name": "S", "width": 64, "height": 36}})),
        ("command_batch", json!({"steps": [{"id": "layer.newNull"}, {"id": "no.such"}], "stop_on_error": false})),
        ("batch", json!({"steps": [{"command": "layer.newSolid", "params": {"color": "#ff0000"}}]})),
        ("get_state", json!({})),
        ("doc_inspect", json!({})),
        ("render_preview", json!({"max_side": 16})),
        ("render_preview", json!({"max_side": 16, "inline": false, "path": png})),
        ("render_snapshot", json!({"name": "s"})),
        ("undo", json!({})),
        // Something changed: the heatmap image, with the metrics as structuredContent.
        ("render_compare", json!({"a": "s", "b": {}})),
        ("redo", json!({})),
        ("render_compare", json!({"a": "s", "b": {}, "region": [0, 0, 32, 18], "min_psnr": 30})),
        ("history", json!({})),
        ("save_project", json!({"path": path})),
        ("command_list", json!({"filter": "comp.new"})),
    ];
    for (name, args) in calls {
        let r = rpc(&mut s, 3, "tools/call", json!({"name": name, "arguments": args}));
        let sc = &r["structuredContent"];
        assert!(sc.is_object(), "{name}: {r}");
        if r["content"][0]["type"] == "text" {
            let text: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
            assert!(sc == &text || sc["items"] == text, "{name}: structuredContent mirrors the text");
        }
        if name != "command_list" && name != "command_run" {
            validate(&schema(name), sc, name).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(png);
}

/// Strict MCP clients validate bounded replies against the schema advertised in tools/list.
#[test]
fn bounded_structured_layout_matches_its_advertised_output_schema() {
    let mut s = server();
    rpc(&mut s, 1, "initialize", json!({"protocolVersion": "2025-06-18"}));
    let tools = rpc(&mut s, 2, "tools/list", json!({}))["tools"].as_array().unwrap().clone();
    let schema = &tools.iter().find(|tool| tool["name"] == "get_layout").unwrap()["outputSchema"];
    assert_eq!(schema["type"], "object");
    assert!(schema.get("properties").is_none(), "legacy restrictions must stay inside their variant");
    call_json(&mut s, "command_run", json!({"id": "comp.new", "params": {"name": "Schema", "width": 400, "height": 300}}));
    let layer = call_json(
        &mut s,
        "command_run",
        json!({"id": "layer.newSolid", "params": {"name": "Large layer name ".repeat(5000), "width": 100, "height": 50}}),
    )["layer"]
        .clone();
    let reply = rpc(&mut s, 3, "tools/call", json!({"name": "get_layout", "arguments": {"layers": [layer]}}));
    assert_eq!(reply["isError"], false, "{reply}");
    let structured = &reply["structuredContent"];
    assert!(structured["result_id"].is_string(), "{structured}");
    assert_eq!(structured["layers"][0]["name"]["$omitted"], true, "{structured}");
    assert_eq!(structured["layers"][0]["name"]["fetch"], "get_result");
    assert!(structured.to_string().len() <= 7000);
    assert!(validate(&schema["anyOf"][0], structured, "legacy layout").is_err(), "regression must exercise the formerly invalid legacy shape");
    validate(schema, structured, "retained layout").unwrap();
}

#[test]
fn retained_and_retention_failure_output_schema_variants_are_explicit() {
    let mut store = crate::result_store::ResultStore::default();
    let mut deep = json!(1);
    for _ in 0..200 {
        deep = json!({"child": deep});
    }
    let failure = store.finish("get_layout", &json!({}), json!({"ok": true, "waiting": true, "layers": deep}), None).unwrap();
    assert_eq!(failure["operation_completed"], false);
    assert_eq!(failure["result_retained"], false);
    let reply = crate::server::call_result_with(Ok(crate::tools::Reply::Json(failure)), true);
    assert_eq!(reply["isError"], true);
    for tool in crate::tools::catalogue() {
        if let Some(schema) = crate::tools::output_schema(tool.name) {
            validate(&schema, &reply["structuredContent"], tool.name).unwrap();
            validate(&schema, &json!({"result_id": "opaque", "header": {"$omitted": true}}), tool.name).unwrap();
        }
    }
    let schema = crate::tools::output_schema("command_batch").unwrap();
    assert!(validate(&schema, &json!({"results": [{"$omitted": true}]}), "missing retained id").is_err());
    assert!(validate(&schema, &json!({"result_id": 3}), "wrong retained id type").is_err());
    assert!(validate(&schema, &json!({"result_retained": true, "operation_completed": false}), "not a retention failure").is_err());
    assert!(validate(&schema, &json!({"result_retained": false, "operation_completed": "false"}), "wrong completion type").is_err());
}

/// AG5: render_snapshot → edit → render_compare, with the heatmap inline only when it shows something.
#[test]
fn render_snapshot_and_compare_tools() {
    let mut s = server();
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "C", "width": 320, "height": 180}}));
    call_json(&mut s, "execute_command", json!({"command": "layer.newSolid", "params": {"name": "Box", "color": "#ffffff", "width": 40, "height": 40}}));
    let snap = call_json(&mut s, "render_snapshot", json!({"name": "before"}));
    assert_eq!((snap["width"].as_u64(), snap["snapshots"].clone()), (Some(320), json!(["before"])));
    // Nothing changed: JSON only, a pass with infinite PSNR.
    let same = call_json(&mut s, "render_compare", json!({"a": "before", "b": {}}));
    assert_eq!((same["pass"].as_bool(), same["psnr_db"].as_str()), (Some(true), Some("inf")), "{same}");
    assert!(same.get("diff_png").is_none());
    // Move the box 10 px right: metrics plus the heatmap image.
    call_json(&mut s, "set_property", json!({"layer": "#1", "path": "transform/position", "value": [170, 90]}));
    let (c, err) = call(&mut s, "render_compare", json!({"a": {"snapshot": "before"}, "b": {"time": 0}}));
    assert!(!err, "{c:?}");
    assert_eq!(c[0]["type"], "image");
    let png = base64::decode(c[0]["data"].as_str().unwrap()).unwrap();
    assert_eq!(png_size(&png), Some((320, 180)));
    let info: Value = serde_json::from_str(c[1]["text"].as_str().unwrap()).unwrap();
    assert_eq!((info["pass"].as_bool(), info["changed_pixels"].as_u64()), (Some(false), Some(800)), "{info}");
    // The 40×40 box (x 140..180) moved to x 150..190: the changed pixels span x 140..190.
    assert_eq!(info["diffBounds"], json!([140, 70, 50, 40]), "{info}");
    assert_eq!(same["diffBounds"], json!(null), "{same}");
    // `region` restricts the comparison (and the heatmap) to one rectangle.
    let (c, err) = call(&mut s, "render_compare", json!({"a": "before", "b": {}, "region": [100, 50, 45, 80]}));
    assert!(!err, "{c:?}");
    assert_eq!(png_size(&base64::decode(c[0]["data"].as_str().unwrap()).unwrap()), Some((45, 80)));
    let info: Value = serde_json::from_str(c[1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        (info["region"].clone(), info["diffBounds"].clone(), info["changed_pixels"].as_u64()),
        (json!([100, 50, 45, 80]), json!([140, 70, 5, 40]), Some(200)),
        "{info}"
    );
    let (c, err) = call(&mut s, "render_compare", json!({"a": "before", "b": {}, "region": [400, 0, 10, 10]}));
    assert!(err && c[0]["text"].as_str().unwrap().contains("outside"), "{c:?}");
    assert!(info.get("diff_png").is_none(), "base64 moved into the image content");
    // inline: false keeps the reply to JSON; bad references are in-band errors.
    let r = call_json(&mut s, "render_compare", json!({"a": "before", "b": {}, "inline": false}));
    assert_eq!(r["pass"], false);
    let (c, err) = call(&mut s, "render_compare", json!({"a": "missing", "b": {}}));
    assert!(err && c[0]["text"].as_str().unwrap().contains("no snapshot `missing`"));
    let (_, err) = call(&mut s, "render_compare", json!({"a": "before"}));
    assert!(err);
}

/// Every command in the long-call table exists and takes `wait` (a typo would silently make it
/// synchronous again).
#[test]
fn long_call_table_names_real_waiting_commands() {
    let mut s = Session::default();
    for id in crate::long_job::spec_commands() {
        let d = s.execute("command.describe", json!({"command": id})).unwrap_or_else(|e| panic!("{id}: {e}"));
        if id != "jobs.wait" {
            assert!(d["schema"]["properties"].get("wait").is_some(), "{id} takes no `wait`: {d}");
        }
    }
}

/// A stdio MCP session on its own thread.
struct Stdio {
    input: Option<std::io::PipeWriter>,
    lines: std::io::Lines<BufReader<std::io::PipeReader>>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl Stdio {
    fn new(session: Session) -> Self {
        let (in_r, in_w) = std::io::pipe().unwrap();
        let (out_r, out_w) = std::io::pipe().unwrap();
        let server = std::thread::spawn(move || McpServer::new(Backend::headless(session)).serve(BufReader::new(in_r), out_w).unwrap());
        let mut io = Stdio { input: Some(in_w), lines: BufReader::new(out_r).lines(), server: Some(server) };
        io.send(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}));
        io.next();
        io
    }
    fn send(&mut self, v: Value) {
        writeln!(self.input.as_mut().unwrap(), "{v}").unwrap();
    }
    fn next(&mut self) -> Value {
        serde_json::from_str(&self.lines.next().unwrap().unwrap()).unwrap()
    }
    /// The next message that is not a progress notification.
    fn reply(&mut self) -> Value {
        loop {
            let m = self.next();
            if m["method"] != "notifications/progress" {
                return m;
            }
        }
    }
    fn call(&mut self, id: u64, tool: &str, args: Value, token: Option<Value>) {
        let mut p = json!({"name": tool, "arguments": args});
        if let Some(t) = token {
            p["_meta"] = json!({"progressToken": t});
        }
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": p}));
    }
    fn cancel(&mut self, id: u64) {
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": id, "reason": "test"}}));
    }
    /// A tool result's JSON text.
    fn json(m: &Value) -> Value {
        assert_eq!(m["result"]["isError"], false, "{m}");
        serde_json::from_str(m["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }
}

impl Drop for Stdio {
    fn drop(&mut self) {
        drop(self.input.take());
        if let Some(s) = self.server.take() {
            let _ = s.join();
        }
    }
}

/// A clip whose frames take ~2 ms each to decode (a textured image), with a tracker on it, for
/// analyses that run long enough to watch.
fn slow_clip_session(seconds: f64) -> Session {
    use effectcraft_engine::{
        color::Label,
        project::{Footage, FootageKind, ItemId, ItemKind},
        raster::Image,
        render::FootageSource,
        time::{FrameRate, Tick},
    };
    use std::sync::Arc;
    struct Slow;
    impl FootageSource for Slow {
        fn frame(&self, _: ItemId, _: &Footage, _: Tick) -> Option<Arc<Image>> {
            std::thread::sleep(std::time::Duration::from_millis(2));
            let mut img = Image::new(64, 64);
            for y in 0..64 {
                for x in 0..64 {
                    let v = (((x * 7 + y * 13) % 17) as f32 / 17.0 + ((x * y) % 5) as f32 * 0.05).min(1.0);
                    img.set(x, y, [v, v, v, 1.0]);
                }
            }
            Some(Arc::new(img))
        }
    }
    let mut s = Session { footage: Arc::new(Slow), ..Default::default() };
    effectcraft_script::install(&mut s);
    s.execute("comp.new", json!({"width": 64, "height": 64, "frameRate": 25, "duration": seconds})).unwrap();
    let footage = Footage {
        path: "slow.mov".into(),
        kind: FootageKind::Video,
        width: 64,
        height: 64,
        pixel_aspect: 1.0,
        frame_rate: FrameRate::new(25, 1),
        duration: Tick::from_seconds_f64(seconds),
        has_video: true,
        ..Default::default()
    };
    let item = Arc::make_mut(&mut s.project).add_item("slow", Label::Aqua, None, ItemKind::Footage(footage));
    let layer = s.execute("layer.addItem", json!({"item": item.0})).unwrap()["layer"].as_u64().unwrap();
    s.execute("layer.select", json!({"layers": [layer]})).unwrap();
    s.execute("track.motion", json!({})).unwrap();
    s.execute("track.setPoint", json!({"point": 1, "center": [32, 32], "featureSize": [16, 16], "searchSize": [32, 32]})).unwrap();
    s.execute("time.set", json!({"time": 0})).unwrap();
    s
}

/// docs/mcp.md "Progress and cancellation" for an analysis: `track.analyze {wait: true}` runs
/// on its background job, reports progress for its token, lets requests through meanwhile and
/// replies like the blocking command once done.
#[test]
fn analysis_reports_progress_and_serves_requests() {
    let mut io = Stdio::new(slow_clip_session(2.0));
    io.call(1, "command_run", json!({"id": "track.analyze", "params": {"direction": "forward", "wait": true}}), Some(json!("trk")));
    let (mut last, mut notes, mut pinged, mut answered) = (-1.0, 0, false, false);
    let done = loop {
        let m = io.next();
        if m["method"] == "notifications/progress" {
            let p = &m["params"];
            assert_eq!(p["progressToken"], "trk", "{m}");
            let v = p["progress"].as_f64().unwrap();
            assert!(v > last, "strictly increasing: {m}");
            if let Some(t) = p["total"].as_f64() {
                assert!(v <= t, "{m}");
            }
            assert!(p["message"].as_str().is_some_and(|s| !s.is_empty()), "{m}");
            (last, notes) = (v, notes + 1);
            if !pinged {
                pinged = true;
                io.send(json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}));
                io.call(3, "get_project", json!({}), None);
            }
        } else if m["id"] == 2 || m["id"] == 3 {
            answered |= m["id"] == 3;
        } else {
            assert_eq!(m["id"], 1, "{m}");
            break m;
        }
    };
    let v = Stdio::json(&done);
    assert_eq!(v["running"], false, "{v}");
    assert_eq!(v["frames"], 49, "{v}");
    assert!(notes >= 2 && answered, "{notes} notifications; requests answered during the analysis: {answered}");
    // the results are in the project, as one undo step
    io.call(4, "get_project", json!({}), None);
    let p = Stdio::json(&io.reply());
    assert!(p["undo"].to_string().contains("Analyze Track"), "{p}");
    // footage.check {wait: true} runs as a task: its reply is the task's result
    io.call(5, "command_run", json!({"id": "footage.check", "params": {"wait": true}}), Some(json!(5)));
    let r = Stdio::json(&io.reply());
    assert_eq!(r["checked"], 1, "{r}");
}

/// `notifications/cancelled` stops a running analysis: no response for it, `ping` is answered,
/// unknown and completed ids are ignored, and the project stays consistent (the tracker keeps
/// the frames it analysed, as one undo step) and usable.
#[test]
fn analysis_cancel_stops_the_job_without_a_reply() {
    let mut io = Stdio::new(slow_clip_session(40.0));
    io.call(1, "command_run", json!({"id": "track.analyze", "params": {"direction": "forward", "wait": true}}), Some(json!(1)));
    let first = io.next();
    assert_eq!(first["method"], "notifications/progress", "{first}");
    io.cancel(77); // unknown id: ignored
    io.cancel(1);
    let started = std::time::Instant::now();
    io.send(json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}));
    assert_eq!(io.reply()["id"], 2);
    io.call(3, "command_run", json!({"id": "jobs.list"}), None);
    let m = io.reply();
    assert_eq!(m["id"], 3, "no response for the cancelled request: {m}");
    // the job winds down at its next frame
    let mut jobs = Stdio::json(&m);
    while jobs["running"] != json!([]) {
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "cancel did not stop the job: {jobs}");
        std::thread::sleep(std::time::Duration::from_millis(20));
        io.call(3, "command_run", json!({"id": "jobs.list"}), None);
        let again = io.reply();
        assert_eq!(again["id"], 3, "{again}");
        jobs = Stdio::json(&again);
    }
    // partial results, one undo step; a new analysis can start
    io.call(4, "get_project", json!({}), None);
    let p = Stdio::json(&io.reply());
    assert!(p["undo"].to_string().contains("Analyze Track"), "{p}");
    io.call(5, "command_run", json!({"id": "track.analyze", "params": {"direction": "frameForward", "wait": true}}), None);
    let r = Stdio::json(&io.reply());
    assert_eq!(r["frames"], 1, "{r}");
    io.cancel(1); // completed id: ignored
    io.send(json!({"jsonrpc": "2.0", "id": 6, "method": "ping"}));
    assert_eq!(io.reply()["id"], 6);
}

/// `run_script` over stdio: `ping` is answered while the script runs, other requests wait for
/// it, and `notifications/cancelled` stops even `while (true) {}` inside `try` quickly, with no
/// response.
#[test]
fn script_cancel_stops_an_endless_loop() {
    let mut sess = Session::default();
    effectcraft_script::install(&mut sess);
    let mut io = Stdio::new(sess);
    io.call(
        1,
        "run_script",
        json!({"code": "app.project.items.addComp('Kept', 64, 64, 1, 1, 25); try { while (true) {} } catch (e) {} 'caught'", "timeout": 0}),
        None,
    );
    io.send(json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}));
    assert_eq!(io.reply()["id"], 2, "ping answered while the script runs");
    io.call(3, "get_project", json!({}), None); // waits for the script
    io.call(4, "get_project", json!({}), None);
    io.cancel(4); // a queued request that is cancelled is dropped
    std::thread::sleep(std::time::Duration::from_millis(300));
    let started = std::time::Instant::now();
    io.cancel(1);
    let m = io.reply();
    assert!(started.elapsed() < std::time::Duration::from_secs(2), "cancel took {:?}", started.elapsed());
    assert_eq!(m["id"], 3, "no response for the cancelled script or the cancelled queued request: {m}");
    // a stopped script undoes its edits (rollback, the default)
    assert!(!Stdio::json(&m).to_string().contains("Kept"), "{m}");
    io.call(5, "run_script", json!({"code": "1 + 1"}), None);
    let m = io.reply();
    assert_eq!(m["id"], 5, "{m}");
    assert_eq!(Stdio::json(&m)["result"], 2);
}

/// `run_script {timeout}` ends a runaway script with an error; an undo group it left open is
/// rolled back, edits outside one are kept, and either way the output says so.
#[test]
fn script_timeout_rolls_back_an_open_undo_group() {
    let mut sess = Session::default();
    effectcraft_script::install(&mut sess);
    let mut s = McpServer::new(Backend::headless(sess));
    let started = std::time::Instant::now();
    let (content, error) = call(
        &mut s,
        "run_script",
        json!({"code": "app.beginUndoGroup('Build'); app.project.items.addComp('Doomed', 64, 64, 1, 1, 25); while (true) {}", "timeout": 0.3}),
    );
    assert!(error);
    let r: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(3), "timeout took {:?}", started.elapsed());
    assert_eq!(r["ok"], false, "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("time limit"), "{r}");
    assert!(r["output"].as_str().unwrap().contains("rolled back"), "{r}");
    assert!(!call_json(&mut s, "get_project", json!({})).to_string().contains("Doomed"));
    // an endless loop calling the object model, outside an undo group
    let (content, error) = call(
        &mut s,
        "run_script",
        json!({"code": "app.project.items.addComp('Kept', 64, 64, 1, 1, 25); for (;;) { app.project.numItems; }", "timeout": 0.3, "rollback": false}),
    );
    assert!(error);
    let r: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(r["ok"], false, "{r}");
    assert!(r["output"].as_str().unwrap().contains("kept"), "{r}");
    assert!(call_json(&mut s, "get_project", json!({})).to_string().contains("Kept"));
    // a script that finishes in time is unaffected
    let r = call_json(&mut s, "run_script", json!({"code": "var n = 0; for (var i = 0; i < 1000; i++) n += i; n", "timeout": 5}));
    assert_eq!(r["result"], 499500, "{r}");
}

/// Agent-facing ergonomics: clamped values come back as `warnings`, errors name the argument and
/// suggest a fix, and the catalogues have a cheap `brief` mode.
#[test]
fn warnings_hints_and_brief_listings() {
    let mut s = server();
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"width": 100, "height": 100}}));
    let l = call_json(&mut s, "execute_command", json!({"command": "layer.newSolid"}))["layer"].clone();
    let r = call_json(&mut s, "add_effect", json!({"layer": l, "effect": "Levels", "values": {"inBlack": 20}}));
    assert!(r["warnings"][0].as_str().unwrap().starts_with("inBlack: 20 is outside the valid range"), "{r}");
    let r = call_json(&mut s, "set_property", json!({"layer": l, "path": "transform/position", "value": 5}));
    assert_eq!(r["warnings"][0], "position: number 5 expanded to [5, 5, 5]", "{r}");
    let r = call_json(&mut s, "add_keyframe", json!({"layer": l, "path": "effects/#1/gamma", "time": 0, "value": 99}));
    assert!(r["warnings"][0].as_str().unwrap().contains("clamped to 10"), "{r}");
    let r = call_json(&mut s, "batch", json!({"steps": [{"command": "prop.set", "params": {"layer": l, "path": "effects/#1/inWhite", "value": 7}}]}));
    assert!(r["warnings"][0].as_str().unwrap().starts_with("step 1 (prop.set): inWhite"), "{r}");

    // add_effect errors name the value, not the batch step.
    let (c, err) = call(&mut s, "add_effect", json!({"layer": l, "effect": "Levels", "values": {"inputBlack": 0.1}}));
    let t = c[0]["text"].as_str().unwrap();
    assert!(err && t.starts_with("values.inputBlack: ") && t.contains("did you mean `inBlack`") && !t.contains("step "), "{t}");
    assert!(t.ends_with("(nothing was applied)"), "{t}");
    let (c, err) = call(&mut s, "add_effect", json!({"layer": l, "effect": "Gausian Blur"}));
    assert!(err && c[0]["text"].as_str().unwrap().contains("did you mean `Gaussian Blur`"), "{c:?}");
    let (c, err) = call(&mut s, "command_run", json!({"id": "layer.newSoild"}));
    assert!(err && c[0]["text"].as_str().unwrap().contains("did you mean `layer.newSolid`"), "{c:?}");

    // command_run takes `id`: the rejection says so.
    let line = json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": "command_run", "arguments": {"command": "comp.new"}}});
    let r: Value = serde_json::from_str(&s.handle_line(&line.to_string()).unwrap()).unwrap();
    assert_eq!(r["error"]["code"], -32602);
    assert!(r["error"]["message"].as_str().unwrap().contains("did you mean `id`?"), "{r}");

    // Brief listings: ids and labels / names only.
    let full = call(&mut s, "command_list", json!({"full":true})).0[0]["text"].as_str().unwrap().len();
    let brief = call(&mut s, "command_list", json!({"brief": true,"full":true})).0[0]["text"].as_str().unwrap().len();
    assert!(brief * 3 < full, "{brief} vs {full}");
    let fx = call_json(&mut s, "list_effects", json!({"brief": true,"full":true}));
    assert!(fx.as_array().unwrap().iter().all(|e| e.as_object().unwrap().len() == 2), "{fx}");
}

/// Every tool takes the agent's name as `client` (or the server runs as `mcp --client NAME`): the
/// engine commands it runs carry it, so each agent has its own selection, comp and time, renders
/// at its own time, and other agents' typed-tool edits to a comp it claimed are refused.
#[test]
fn tools_run_as_the_calling_client() {
    for bridge in [false, true] {
        let mut s = if bridge { McpServer::new(Backend::bridge(&fake_app().to_string()).unwrap()) } else { server() };
        call_json(&mut s, "command_run", json!({"id": "comp.new", "params": {"name": "Main", "width": 64, "height": 36, "frameRate": 24, "duration": 2}}));
        let a = call_json(&mut s, "command_run", json!({"id": "layer.newSolid", "params": {"name": "A", "width": 8, "height": 8}}))["layer"].clone();
        let b = call_json(&mut s, "command_run", json!({"id": "layer.newSolid", "params": {"name": "B", "width": 8, "height": 8}}))["layer"].clone();
        call_json(&mut s, "command_run", json!({"id": "layer.select", "params": {"layers": [a]}, "client": "agentA"}));
        call_json(&mut s, "command_run", json!({"id": "layer.select", "params": {"layers": [b]}, "client": "agentB"}));
        let st = call_json(&mut s, "get_state", json!({"client": "agentA"}));
        assert_eq!((st["state"]["client"].clone(), st["state"]["selected_layers"].clone()), (json!("agentA"), json!([a])), "bridge {bridge}: {st}");
        assert_eq!(call_json(&mut s, "get_state", json!({"client": "agentB"}))["state"]["selected_layers"], json!([b]));
        // A selection edit lands on the calling agent's own layer.
        call_json(&mut s, "command_run", json!({"id": "prop.set", "params": {"path": "transform/opacity", "value": 10}, "client": "agentA"}));
        let opacity = |s: &mut McpServer, l: &Value| call_json(s, "get_property", json!({"layer": l, "path": "transform/opacity"}))["value"].clone();
        assert_eq!((opacity(&mut s, &a), opacity(&mut s, &b)), (json!(10.0), json!(100.0)), "bridge {bridge}");
        // Renders default to the agent's own time (bridged too: through the engine command).
        call_json(&mut s, "command_run", json!({"id": "time.set", "params": {"time": 1}, "client": "agentA"}));
        let (c, err) = call(&mut s, "render_preview", json!({"client": "agentA", "max_side": 16}));
        assert!(!err, "{c:?}");
        let meta = c.iter().find(|x| x["type"] == "text").map(|x| serde_json::from_str::<Value>(x["text"].as_str().unwrap()).unwrap()).unwrap();
        assert_eq!(meta["time"], json!(1.0), "bridge {bridge}: {meta}");
        // A claim refuses the other agent's typed-tool edit, never the user's.
        call_json(&mut s, "command_run", json!({"id": "comp.claim", "params": {"comp": "Main"}, "client": "agentA"}));
        let (c, err) = call(&mut s, "set_property", json!({"layer": b, "path": "transform/opacity", "value": 50, "client": "agentB"}));
        assert!(err && c[0]["text"].as_str().unwrap().contains("claimed by agentA"), "bridge {bridge}: {c:?}");
        call_json(&mut s, "set_property", json!({"layer": b, "path": "transform/opacity", "value": 50}));
        call_json(&mut s, "set_property", json!({"layer": a, "path": "transform/opacity", "value": 20, "client": "agentA"}));
        let (c, err) = call(&mut s, "get_state", json!({"client": 5}));
        assert!(err && c[0]["text"].as_str().unwrap().contains("`client`"), "{c:?}");
    }
    // `mcp --client NAME`: every call runs as that agent; the user's cursor stays as it was.
    let mut s = server().with_client(Some("agentC".into()));
    call_json(&mut s, "command_run", json!({"id": "comp.new", "params": {"name": "Theirs", "width": 64, "height": 36}}));
    assert_eq!(call_json(&mut s, "get_state", json!({}))["state"]["client"], "agentC");
    assert_eq!(s.backend().session().unwrap().state.active_comp, None, "the user's view didn't move");
    assert_eq!(s.backend().client(), Some("agentC"));
}

/// Bridged to an app, an agent that gave no name runs as `<MCP client name>-<pid>` from
/// `initialize` on, so it never moves the user's selection; `"client": ""` runs one call as the
/// user. Headless servers (the agent is alone) and named servers keep their client.
#[test]
fn bridged_agents_get_a_name_by_default() {
    let init = json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "Claude Code", "version": "1"}});
    let mut s = McpServer::new(Backend::bridge(&fake_app().to_string()).unwrap());
    rpc(&mut s, 1, "initialize", init.clone());
    let me = format!("claude-code-{}", std::process::id());
    assert_eq!(s.backend().client(), Some(me.as_str()));
    call_json(&mut s, "command_run", json!({"id": "comp.new", "params": {"name": "Main", "width": 64, "height": 36}}));
    let a = call_json(&mut s, "command_run", json!({"id": "layer.newSolid", "params": {"name": "A", "width": 8, "height": 8}}))["layer"].clone();
    call_json(&mut s, "command_run", json!({"id": "layer.select", "params": {"layers": [a]}}));
    let mine = call_json(&mut s, "get_state", json!({}));
    assert_eq!((mine["state"]["client"].clone(), mine["state"]["selected_layers"].clone()), (json!(me), json!([a])), "{mine}");
    let user = call_json(&mut s, "get_state", json!({"client": ""}));
    assert!(user["state"]["client"].is_null() && user["state"]["selected_layers"] != json!([a]), "the user's selection didn't move: {user}");
    assert_eq!(s.backend().client(), Some(me.as_str()), "a per-call client is restored after the call");

    let mut named = McpServer::new(Backend::bridge(&fake_app().to_string()).unwrap()).with_client(Some("agentC".into()));
    rpc(&mut named, 1, "initialize", init.clone());
    assert_eq!(named.backend().client(), Some("agentC"));
    let mut alone = server();
    rpc(&mut alone, 1, "initialize", json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "!!!", "version": "1"}}));
    assert_eq!(alone.backend().client(), None);
    assert_eq!(crate::server::default_client(&json!({"clientInfo": {"name": "!!!"}})), format!("agent-{}", std::process::id()));
}

/// An agent hops between projects: `hosts` lists where projects are open, `join` (a port or a
/// project path) switches the server onto that host as a named agent, and leaving unsaved
/// headless work is refused.
#[test]
fn hosts_tool_lists_and_joins_a_running_app() {
    let mut s = server();
    let listed = call_json(&mut s, "hosts", json!({}));
    assert!(listed["hosts"].is_array(), "{listed}");
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "Unsaved"}}));
    let port = fake_app();
    let (c, err) = call(&mut s, "hosts", json!({"join": port}));
    assert!(err && c[0]["text"].as_str().unwrap().contains("unsaved"), "{c:?}");
    let mut clean = server();
    let joined = call_json(&mut clean, "hosts", json!({"join": port}));
    assert_eq!(joined["joined"], port);
    let me = format!("mcp-{}", std::process::id());
    assert_eq!(joined["client"], json!(me));
    assert_eq!(clean.backend().mode(), "bridge");
    assert_eq!(clean.backend().client(), Some(me.as_str()));
    let (c, err) = call(&mut clean, "hosts", json!({"join": "/nowhere/Nope.ecproj"}));
    assert!(err && c[0]["text"].as_str().unwrap().contains("candidates"), "{c:?}");
}

/// A windowless host: two agents join one in-memory project, each claims and edits its own comp,
/// renders reply without blocking the other agent, and `host.release` saves the project and stops
/// the host (in the host list while it runs).
#[test]
fn windowless_host_serves_several_agents_one_project() {
    use crate::host_server::{HostConfig, request, serve};
    let dir = autosave_test_dir("host");
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    let config = HostConfig { port: 0, config_dir: Some(dir.clone()), follow: None };
    let host = std::thread::spawn(move || serve(config, Box::new(|| Ok(Session::default())), move |p| port_tx.send(p).unwrap()));
    let port = port_rx.recv().unwrap();
    // Each agent keeps its connection open, as an MCP bridge does: a client's claims are released
    // when its last connection closes.
    let connect = || {
        let c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(60))).unwrap();
        std::cell::RefCell::new((std::io::BufReader::new(c.try_clone().unwrap()), c))
    };
    let conns = [("agentA", connect()), ("agentB", connect())];
    let exec = |client: &str, command: &str, params: Value| {
        use std::io::{BufRead, Write};
        let mut p = params;
        p["client"] = json!(client);
        let mut conn = conns.iter().find(|(c, _)| *c == client).unwrap().1.borrow_mut();
        writeln!(conn.1, "{}", json!({"id": 1, "method": "engine.execute", "params": {"command": command, "params": p}})).unwrap();
        let mut line = String::new();
        conn.0.read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    };
    let a = exec("agentA", "comp.new", json!({"name": "A", "width": 64, "height": 36}));
    let b = exec("agentB", "comp.new", json!({"name": "B", "width": 64, "height": 36}));
    assert_eq!((a["ok"].clone(), b["ok"].clone()), (json!(true), json!(true)), "{a} {b}");
    assert_eq!(exec("agentA", "comp.claim", json!({"comp": "A"}))["ok"], true);
    assert_eq!(exec("agentA", "layer.newSolid", json!({"comp": "A", "width": 8, "height": 8}))["ok"], true);
    let refused = exec("agentB", "layer.newSolid", json!({"comp": "A", "width": 8, "height": 8}));
    assert!(refused["error"].as_str().unwrap_or("").contains("claimed by agentA"), "{refused}");
    let frame = exec("agentB", "render.frame", json!({"comp": "B", "base64": true, "max_side": 16}));
    assert_eq!(frame["ok"], true, "{frame}");
    assert!(frame["result"]["png"].is_string());
    let info = request(port, "host.info", json!({})).unwrap();
    assert_eq!(info["result"]["dirty"], true);
    assert!(request(port, "ui.click", json!({})).unwrap()["error"].as_str().unwrap().contains("desktop app"));
    let listed = exec("agentA", "hosts.list", json!({}));
    assert!(listed["result"]["hosts"].as_array().unwrap().iter().any(|h| h["port"] == port && h["kind"] == "host"), "{listed}");
    let saved = dir.join("Shared.ecproj");
    let r = request(port, "host.release", json!({"path": saved.to_string_lossy()})).unwrap();
    assert_eq!(r["result"]["released"], true, "{r}");
    host.join().unwrap().unwrap();
    let text = std::fs::read_to_string(&saved).unwrap();
    assert!(text.contains("\"A\"") && text.contains("\"B\""), "both agents' comps are in the saved project");
    assert!(!dir.join(effectcraft_engine::hosts::HOSTS_FOLDER).join(format!("{}.json", std::process::id())).exists(), "left the host list");
    std::fs::remove_dir_all(dir).unwrap();
}

/// #500: `--automation-read-root` / `--automation-write-root` confine the files an agent reads
/// and writes: inside the roots works, outside (or relative, `..`, through a symbolic link, by
/// a batch step, by Save to the project's own file, from a script) is refused.
#[test]
fn automation_roots_confine_file_access() {
    use crate::confine::Confinement;
    let base = std::env::temp_dir().join(format!("ec-mcp-confine-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (read, write, outside) = (base.join("in"), base.join("out"), base.join("elsewhere"));
    for d in [&read, &write, &outside] {
        std::fs::create_dir_all(d).unwrap();
    }
    let p = |d: &std::path::Path, f: &str| d.join(f).to_string_lossy().into_owned();
    // Unconfined (the default): anywhere.
    let mut s = server();
    call_json(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "A", "width": 64, "height": 64}}));
    call_json(&mut s, "save_project", json!({"path": p(&outside, "free.ecproj")}));
    call_json(&mut s, "save_project", json!({"path": p(&read, "source.ecproj")}));

    let mut s = server();
    s.backend().confine(Confinement::new(std::slice::from_ref(&read), std::slice::from_ref(&write)).unwrap());
    let denied = |s: &mut McpServer, name: &str, args: Value| {
        let (c, err) = call(s, name, args.clone());
        assert!(err, "{name} {args} should be refused: {c:?}");
        c[0]["text"].as_str().unwrap_or_default().to_string()
    };
    // Reading: inside a read root, not outside.
    call_json(&mut s, "open_project", json!({"path": p(&read, "source.ecproj")}));
    let msg = denied(&mut s, "open_project", json!({"path": p(&outside, "free.ecproj")}));
    assert!(msg.contains("outside the automation roots"), "{msg}");
    // Writing: inside a write root only; not into a read root, not relative, not with `..`.
    call_json(&mut s, "save_project", json!({"path": p(&write, "saved.ecproj")}));
    assert!(write.join("saved.ecproj").is_file());
    denied(&mut s, "save_project", json!({"path": p(&read, "copy.ecproj")}));
    denied(&mut s, "save_project", json!({"path": "relative.ecproj"}));
    denied(&mut s, "save_project", json!({"path": p(&write, "../elsewhere/up.ecproj")}));
    denied(&mut s, "execute_command", json!({"command": "comp.saveFrameAs", "params": {"path": p(&outside, "f.png")}}));
    // The checking commands' outputs are writes too, even into a read root or relative.
    denied(&mut s, "execute_command", json!({"command": "render.contactSheet", "params": {"gif": p(&read, "sheet.gif")}}));
    denied(&mut s, "execute_command", json!({"command": "audio.analyze", "params": {"path": "wave.png"}}));
    denied(&mut s, "execute_command", json!({"command": "help.exportRepro", "params": {"path": p(&read, "repro.jsonl")}}));
    // Any other absolute path a command is given.
    denied(&mut s, "execute_command", json!({"command": "renderQueue.add", "params": {"output": p(&outside, "movie.mp4")}}));
    // A batch step.
    let steps = json!([{"command": "comp.new", "params": {"name": "B"}}, {"command": "file.saveAs", "params": {"path": p(&outside, "b.ecproj")}}]);
    denied(&mut s, "batch", json!({"steps": steps}));
    let steps = json!([{"command": "file.saveAs", "params": {"path": "$1.path"}}]);
    denied(&mut s, "batch", json!({"steps": steps}));
    // Save (and Increment and Save) write the open project's own file: it must be writable.
    call_json(&mut s, "open_project", json!({"path": p(&read, "source.ecproj")}));
    denied(&mut s, "save_project", json!({}));
    denied(&mut s, "execute_command", json!({"command": "file.incrementAndSave"}));
    // Clipboard asset writes are disabled while file access is confined.
    let msg = denied(&mut s, "execute_command", json!({"command": "edit.pasteFigCraft", "params": {"clipboard": "{}"}}));
    assert!(msg.contains("file access is confined"), "{msg}");
    // Rendering the queue writes its items' outputs (by default beside the project).
    call_json(&mut s, "execute_command", json!({"command": "renderQueue.add", "params": {"comp": "A"}}));
    let queue = call_json(&mut s, "execute_command", json!({"command": "renderQueue.list"}));
    assert!(queue["items"][0]["outputPath"].as_str().is_some_and(|o| o.starts_with(&*read.to_string_lossy())), "{queue}");
    let msg = denied(&mut s, "execute_command", json!({"command": "renderQueue.render"}));
    assert!(msg.contains("cannot write"), "{msg}");
    call_json(&mut s, "open_project", json!({"path": p(&write, "saved.ecproj"), "discard": true}));
    call_json(&mut s, "save_project", json!({}));
    // Through a symbolic link out of a root.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, write.join("link")).unwrap();
        denied(&mut s, "save_project", json!({"path": p(&write.join("link"), "escaped.ecproj")}));
        assert!(!outside.join("escaped.ecproj").exists());
    }
    // Scripts can read and write anything: refused.
    denied(&mut s, "run_script", json!({"code": "1 + 1"}));
    denied(&mut s, "execute_command", json!({"command": "file.runScript", "params": {"path": p(&read, "x.jsx")}}));
    // Writes to the files the agent named: none outside.
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1, "only the unconfined save");
    let _ = std::fs::remove_dir_all(&base);
}

/// A comp with `n` solids, built through the tools (`Big`, 1920×1080).
fn big_comp(n: usize) -> McpServer {
    let mut s = server();
    rpc(&mut s, 1, "initialize", json!({"protocolVersion": "2025-06-18"}));
    call_json(&mut s, "command_run", json!({"id": "comp.new", "params": {"name": "Big", "width": 1920, "height": 1080, "duration": 10}}));
    for i in 0..n {
        call_json(&mut s, "command_run", json!({"id": "layer.newSolid", "params": {"name": format!("Lower third background {i}"), "color": "#336699"}}));
    }
    s
}

#[test]
fn get_comp_is_compact_and_says_when_it_is_cut() {
    let mut s = big_comp(110);
    let full = call_json(&mut s, "get_comp", json!({"full": true}));
    let compact = call_json(&mut s, "get_comp", json!({"max_chars": 100000}));
    let (fl, cl) = (full.to_string().len(), compact.to_string().len());
    eprintln!("110 layers: full {fl} chars, compact {cl} chars ({} per layer)", cl / 110);
    // A larger requested budget can return all rows; default-budget traversal is tested below.
    assert_eq!((compact["layerCount"].clone(), compact["returned"].clone(), compact["next_offset"].clone()), (json!(110), json!(110), Value::Null));
    assert!(compact.get("truncated").is_none(), "nothing cut");
    assert!(cl < 110 * 130, "{cl} chars");
    assert!(fl > 3 * cl, "compact is several times smaller: {fl} vs {cl}");
    // Defaults are omitted: a fresh solid is just index, id, name, type, in, out.
    let row = &compact["layers"][0];
    let keys: Vec<&str> = row.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["id", "in", "index", "name", "out", "type"], "{row}");
    assert_eq!(row["id"], full["layers"][0]["id"]);
    assert!(compact["compact"].is_string(), "the reply says what compact rows leave out");

    // `full: true` is the engine's comp.info unchanged.
    assert_eq!(full["layers"].as_array().unwrap().len(), 110);
    assert!(full.get("layerCount").is_none() && full["layers"][0]["switches"].is_object());

    // Paging: a cut list says so in-band, with the offset to continue from.
    let p1 = call_json(&mut s, "get_comp", json!({"limit": 50}));
    assert_eq!((p1["returned"].clone(), p1["next_offset"].clone(), p1["layerCount"].clone()), (json!(50), json!(50), json!(110)));
    assert_eq!(p1["truncated"]["next_offset"], 50);
    assert_eq!(p1["truncated"]["total"], 110);
    assert!(p1["truncated"]["hint"].as_str().unwrap().contains("offset=50"));
    let p3 = call_json(&mut s, "get_comp", json!({"offset": 100, "limit": 50, "expectRevision": compact["revision"]}));
    assert_eq!((p3["returned"].clone(), p3["next_offset"].clone()), (json!(10), Value::Null));
    assert_eq!(p3["layers"][0]["index"], 101);
    assert!(p3.get("truncated").is_none());
    let past = call_json(&mut s, "get_comp", json!({"offset": 500, "expectRevision": compact["revision"]}));
    assert_eq!((past["returned"].clone(), past["next_offset"].clone()), (json!(0), Value::Null));
    // Paged full rows.
    let pf = call_json(&mut s, "get_comp", json!({"full": true, "limit": 2}));
    assert_eq!((pf["layers"].as_array().unwrap().len(), pf["next_offset"].clone()), (2, json!(2)));
    assert!(pf["layers"][0]["switches"].is_object());
    // Bad paging arguments are errors, not crashes.
    for bad in [json!({"limit": 0}), json!({"offset": -1}), json!({"limit": "many"}), json!({"offset": 1.5})] {
        assert!(call(&mut s, "get_comp", bad.clone()).1, "{bad}");
    }
    // doc_inspect's active comp is compact too.
    let doc = call_json(&mut s, "doc_inspect", json!({"max_chars": 100000}));
    assert_eq!(doc["activeComp"]["layerCount"], 110);
    assert_eq!(doc["activeComp"]["layers"][0], compact["layers"][0]);
}

#[test]
fn compact_rows_list_only_what_differs() {
    let mut s = big_comp(3);
    let run = |s: &mut McpServer, id: &str, params: Value| call_json(s, "command_run", json!({"id": id, "params": params}));
    run(&mut s, "layer.setSwitch", json!({"layers": ["#1"], "switch": "threeD", "value": true}));
    run(&mut s, "layer.setSwitch", json!({"layers": ["#1"], "switch": "video", "value": false}));
    run(&mut s, "layer.setSwitch", json!({"layers": ["#1"], "switch": "quality", "value": false}));
    run(&mut s, "layer.setBlendMode", json!({"layers": ["#1"], "mode": "Multiply"}));
    let parent = call_json(&mut s, "get_comp", json!({}))["layers"][2]["id"].clone();
    run(&mut s, "layer.setParent", json!({"layers": ["#1"], "parent": parent}));
    run(&mut s, "layer.timing", json!({"layer": "#2", "start": 1.0}));
    let c = call_json(&mut s, "get_comp", json!({}));
    let row = &c["layers"][0];
    assert_eq!(row["switches"], json!(["hidden", "3d", "quality:Draft"]), "{row}");
    assert_eq!(row["blendMode"], "Multiply");
    assert_eq!(row["parent"], parent);
    assert!(row.get("start").is_none() && row.get("label").is_none(), "{row}");
    assert!((c["layers"][1]["start"].as_f64().unwrap() - 1.0).abs() < 0.01, "{c}");
    assert!(c["layers"][2].get("switches").is_none() && c["layers"][2].get("blendMode").is_none());
    assert!(row.to_string().len() <= 200, "a busy row stays short: {row}");

    // Every engine switch is known (a new one would show as `key:value` until it is added).
    let full = call_json(&mut s, "get_comp", json!({"full": true}));
    let mut keys: Vec<&str> = full["layers"][0]["switches"].as_object().unwrap().keys().map(String::as_str).collect();
    let mut known = crate::compact::known_switches();
    keys.sort_unstable();
    known.sort_unstable();
    assert_eq!(keys, known);
}

#[test]
fn get_property_pages_keyframes_and_get_layer_pages_the_flat_list() {
    let mut s = big_comp(1);
    let keys: Vec<Value> = (0..257).map(|i| json!({"time": f64::from(i) / 30.0, "value": [f64::from(i) / 3.0, 540.0]})).collect();
    call_json(&mut s, "add_keyframe", json!({"layer": "#1", "path": "transform/position", "keys": keys}));
    let p = call_json(&mut s, "get_property", json!({"layer": "#1", "path": "transform/position", "max_chars": 100000}));
    assert_eq!((p["keyCount"].clone(), p["returned"].clone(), p["next_offset"].clone()), (json!(257), json!(100), json!(100)));
    assert_eq!(p["truncated"]["next_offset"], 100);
    assert!(p.to_string().len() < 100_000);
    let k = &p["keys"][1];
    assert!(k.get("layerTime").is_none() && k.get("compTime").is_none() && k.get("in").is_none(), "{k}");
    assert_eq!(k["value"][0], 1.0 / 3.0, "values stay exact: {k}");
    let last = call_json(&mut s, "get_property", json!({"layer": "#1", "path": "transform/position", "offset": 200, "expectRevision": p["revision"]}));
    assert_eq!((last["returned"].clone(), last["next_offset"].clone()), (json!(57), Value::Null));
    assert!(last.get("truncated").is_none());
    // `full: true`: every key, every field.
    let full = call_json(&mut s, "get_property", json!({"layer": "#1", "path": "transform/position", "full": true}));
    assert_eq!(full["keys"].as_array().unwrap().len(), 257);
    assert!(full["keys"][1]["layerTime"].is_number() && full["keys"][1]["in"].is_string());
    assert!(full.get("keyCount").is_none());

    let flat = call_json(&mut s, "get_layer", json!({"layer": "#1", "flat": true, "limit": 5}));
    let total = flat["total"].as_u64().unwrap();
    assert!(total > 5, "{flat}");
    assert_eq!((flat["returned"].clone(), flat["next_offset"].clone()), (json!(5), json!(5)));
    assert_eq!(flat["truncated"]["total"], total);
    let rest = call_json(&mut s, "get_layer", json!({"layer": "#1", "flat": true, "offset": 5, "max_chars": 100000, "expectRevision": flat["revision"]}));
    assert_eq!(rest["returned"].as_u64().unwrap() + 5, total);
    assert_eq!(rest["next_offset"], Value::Null);
    // The tree with 257 keys is big: it says how to get a smaller reply.
    let tree = call_json(&mut s, "get_layer", json!({"layer": "#1"}));
    assert!(tree.to_string().len() <= 7000);
    assert!(tree.to_string().contains("get_field"), "large fields have a recovery route");
}

#[test]
fn set_property_confirms_the_written_comp_time_in_both_bases() {
    use effectcraft_engine::project::LayerId;
    for stretch in [100.0, 200.0, -100.0] {
        for base in [None, Some("layer"), Some("comp")] {
            let mut session = Session::default();
            session.execute("comp.new", json!({"duration": 30, "frameRate": 30})).unwrap();
            let lid = session.execute("layer.newSolid", json!({})).unwrap()["layer"].as_u64().unwrap();
            session.execute("layer.timing", json!({"layer": lid, "start": 10, "in": 0, "out": 30})).unwrap();
            let cid = session.active_comp_id().unwrap();
            std::sync::Arc::make_mut(&mut session.project).comp_mut(cid).unwrap().layer_mut(LayerId(lid)).unwrap().stretch = stretch;
            let mut s = McpServer::new(Backend::headless(session));
            call_json(&mut s, "set_property", json!({"layer": lid, "path": "transform/opacity", "value": 20, "time": 0}));
            let requested_comp = 12.011;
            let requested = if base == Some("comp") { requested_comp } else { (requested_comp - 10.0) * 100.0 / stretch };
            let mut args = json!({"layer": lid, "path": "transform/opacity", "value": 80, "time": requested});
            if let Some(base) = base {
                args["timeBase"] = json!(base);
            }
            let got = call_json(&mut s, "set_property", args);
            assert_eq!(got["requestedTime"], requested);
            assert!((got["compTime"].as_f64().unwrap() - 12.0).abs() < 1e-8, "{stretch} {base:?}: {got}");
            assert!((got["layerTime"].as_f64().unwrap() - 2.0 * 100.0 / stretch).abs() < 1e-8, "{got}");
            assert_eq!(got["time"], got["compTime"]);
            assert_eq!(got["value"], 80.0);
            assert_eq!(got["appliedValue"], 80.0);
        }
    }
}

#[test]
fn bounded_pages_cover_every_layer_and_exact_field_chunks_round_trip() {
    let mut s = big_comp(110);
    let revision = call_json(&mut s, "get_state", json!({}))["state"]["revision"].clone();
    let mut offset = 0u64;
    let mut ids = std::collections::BTreeSet::new();
    loop {
        let (content, error) = call(&mut s, "get_comp", json!({"offset": offset,"expectRevision":revision}));
        assert!(!error, "{content:?}");
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.len() <= 7000, "{}", text.len());
        assert!(text.find("\"returned\"").unwrap() < text.find("\"layers\":").unwrap());
        let page: Value = serde_json::from_str(text).unwrap();
        assert_eq!(page["total"], 110);
        for row in page["layers"].as_array().unwrap() {
            assert!(ids.insert(row["id"].as_u64().unwrap()));
        }
        match page["next_offset"].as_u64() {
            Some(next) => {
                assert!(next > offset);
                offset = next;
            }
            None => break,
        }
    }
    assert_eq!(ids.len(), 110);
    let text = "A 🌱 \" ".repeat(2500);
    let layer = call_json(&mut s, "command_run", json!({"id": "layer.newText", "params": {"text": text}}))["layer"].clone();
    let small = call_json(&mut s, "get_property", json!({"layer": layer, "path": "text/sourceText"}));
    assert!(small.to_string().len() <= 7000);
    let omitted = &small["value"];
    assert_eq!(omitted["$omitted"], true, "{small}");
    let mut offset = 0;
    let mut exact = String::new();
    loop {
        let page = call_json(
            &mut s,
            "get_field",
            json!({"source": "get_property", "layer": layer, "comp": small["comp"], "time": small["time"], "path": "text/sourceText", "pointer": omitted["pointer"], "offset": offset,"expectRevision":small["revision"]}),
        );
        assert!(page.to_string().len() <= 7000);
        exact.push_str(page["text"].as_str().unwrap());
        match page["next_offset"].as_u64() {
            Some(next) => {
                assert!(next > offset);
                offset = next;
            }
            None => break,
        }
    }
    let full = call_json(&mut s, "get_property", json!({"layer": layer, "path": "text/sourceText", "full": true}));
    assert_eq!(serde_json::from_str::<Value>(&exact).unwrap(), full["value"]);
}

#[test]
fn compact_key_values_can_be_written_back_without_rounding() {
    let mut s = big_comp(1);
    call_json(
        &mut s,
        "add_keyframe",
        json!({"layer": "#1", "path": "transform/rotation", "keys": [{"time": 1,"value": 0.00001},{"time": 2,"value": 0.00002}]}),
    );
    let read = call_json(&mut s, "get_property", json!({"layer":"#1","path":"transform/rotation"}));
    assert_ne!(read["keys"][0]["value"], read["keys"][1]["value"]);
    for k in read["keys"].as_array().unwrap() {
        let written = call_json(&mut s, "set_property", json!({"layer":"#1","path":"transform/rotation","time":k["time"],"value":k["value"]}));
        assert_eq!(written["appliedValue"], k["value"]);
    }
}

#[test]
fn keyframe_edits_are_atomic_and_have_one_undo_step() {
    let mut s = big_comp(1);
    let before = call_json(&mut s, "get_project", json!({"full":true}));
    for args in [
        json!({"keys":[{"time":1,"value":20},{"value":80}]}),
        json!({"keys":[{"time":1,"value":20},{"time":2,"value":"bad"}]}),
        json!({"keys":[{"time":1,"value":20}],"interpolation":"wrong"}),
    ] {
        let mut a = args;
        a["layer"] = json!("#1");
        a["path"] = json!("transform/opacity");
        let (content, error) = call(&mut s, "add_keyframe", a);
        assert!(error, "{content:?}");
        let prop = call_json(&mut s, "get_property", json!({"layer":"#1","path":"transform/opacity"}));
        assert_eq!(prop["keyCount"], 0);
        assert_eq!(call_json(&mut s, "get_project", json!({"full":true}))["undo"], before["undo"]);
    }
    let out = call_json(
        &mut s,
        "add_keyframe",
        json!({"layer":"#1","path":"transform/opacity","keys":[{"time":1.011,"value":20},{"time":2.011,"value":80}],"interpolation":"hold"}),
    );
    assert_eq!(out["applied"], true);
    assert_eq!(out["rolled_back"], false);
    assert!(out["keys"].as_array().unwrap().iter().all(|k| k["out"] == "Hold"));
    let after = call_json(&mut s, "get_project", json!({"full":true}));
    assert_eq!(after["undo"].as_array().unwrap().len(), before["undo"].as_array().unwrap().len() + 1);
    call_json(&mut s, "undo", json!({}));
    assert_eq!(call_json(&mut s, "get_property", json!({"layer":"#1","path":"transform/opacity"}))["keyCount"], 0);
}

#[test]
fn revision_guards_reject_stale_writes_and_live_page_continuation() {
    let mut s = big_comp(3);
    let page = call_json(&mut s, "get_comp", json!({"limit":1}));
    call_json(&mut s, "set_property", json!({"layer":"#1","path":"transform/opacity","value":40}));
    for (name, args) in [
        ("set_property", json!({"layer":"#1","path":"transform/opacity","value":80,"expectRevision":page["revision"]})),
        ("add_keyframe", json!({"layer":"#1","path":"transform/opacity","time":1,"value":80,"expectRevision":page["revision"]})),
        ("add_effect", json!({"layer":"#1","effect":"Gaussian Blur","expectRevision":page["revision"]})),
        ("get_comp", json!({"offset":1,"limit":1,"expectRevision":page["revision"]})),
        ("get_comp", json!({"offset":1,"limit":1})),
    ] {
        let (content, error) = call(&mut s, name, args);
        assert!(error, "{name}: {content:?}");
    }
    let prop = call_json(&mut s, "get_property", json!({"layer":"#1","path":"transform/opacity"}));
    assert_eq!(prop["value"], 40.0);
    assert_eq!(prop["keyCount"], 0);
    let new = call_json(&mut s, "get_comp", json!({"limit":1}));
    assert_eq!(call_json(&mut s, "get_comp", json!({"offset":1,"limit":1,"expectRevision":new["revision"]}))["returned"], 1);
}

#[test]
fn inspection_transport_errors_are_not_empty_compositions() {
    struct BrokenComp;
    impl crate::ControlTransport for BrokenComp {
        fn call(&mut self, _: &str, params: Value) -> crate::Result<Value> {
            match params["command"].as_str() {
                Some("project.summary") => Ok(json!({"activeComp":1,"revision":2,"items":[]})),
                Some("comp.info") => Err(crate::Error::Bridge("deliberately dropped read".into())),
                _ => Ok(json!({})),
            }
        }
    }
    let mut s = McpServer::new(Backend::transport(BrokenComp));
    let (content, error) = call(&mut s, "doc_inspect", json!({}));
    assert!(error && content[0]["text"].as_str().unwrap().contains("dropped read"));
    let mut empty = server();
    assert!(call_json(&mut empty, "doc_inspect", json!({}))["activeComp"].is_null());
}

#[test]
fn open_project_preserves_provenance_and_missing_media_warning() {
    struct Opened;
    impl crate::ControlTransport for Opened {
        fn call(&mut self, _: &str, p: Value) -> crate::Result<Value> {
            Ok(match p["command"].as_str() {
                Some("file.open") => {
                    json!({"path":"/project.ecproj","repaired":["fixed width"],"savedBy":"999.0.0","handover":"saved previous host","openElsewhere":[{"pid":99}],"project":{"path":"/project.ecproj","revision":7,"activeComp":1,"items":[]}})
                }
                Some("footage.check") => json!({"missing":2,"checked":2}),
                _ => json!({"revision":7}),
            })
        }
    }
    let mut s = McpServer::new(Backend::transport(Opened));
    let result = call_json(&mut s, "open_project", json!({"path":"/project.ecproj"}));
    assert_eq!(result["repaired"], json!(["fixed width"]));
    assert_eq!(result["savedBy"], "999.0.0");
    assert_eq!(result["handover"], "saved previous host");
    assert_eq!(result["openElsewhere"][0]["pid"], 99);
    let kinds: Vec<_> = result["warnings"].as_array().unwrap().iter().map(|w| w["kind"].as_str().unwrap()).collect();
    for kind in ["repairs", "newerVersion", "missingMedia", "openElsewhere"] {
        assert!(kinds.contains(&kind));
    }
}

/// `open_project` must never silently drop unsaved work: not a headless server's own session,
/// and not a shared host it is bridged to (several agents/the desktop app on one running app) —
/// `b.session()` is `None` over a bridge, so the guard reads `project.summary`'s `dirty` flag
/// instead, which the fake app's real `Session::execute` keeps accurate (#open_project-dirty).
#[test]
fn open_project_refuses_to_discard_unsaved_work() {
    let mut headless = server();
    call_json(&mut headless, "execute_command", json!({"command": "comp.new", "params": {"name": "Unsaved"}}));
    let (c, err) = call(&mut headless, "open_project", json!({"new": true}));
    assert!(err && c[0]["text"].as_str().unwrap().contains("unsaved changes"), "{c:?}");
    let reopened = call_json(&mut headless, "open_project", json!({"new": true, "discard": true}));
    assert_eq!(reopened["dirty"], false);

    let mut bridged = McpServer::new(Backend::bridge(&fake_app().to_string()).unwrap());
    call_json(&mut bridged, "execute_command", json!({"command": "comp.new", "params": {"name": "Unsaved"}}));
    let (c, err) = call(&mut bridged, "open_project", json!({"new": true}));
    assert!(err && c[0]["text"].as_str().unwrap().contains("unsaved changes"), "{c:?}");
    let reopened = call_json(&mut bridged, "open_project", json!({"new": true, "discard": true}));
    assert_eq!(reopened["dirty"], false);
}

#[test]
fn retained_script_output_is_recovered_without_running_the_script_again() {
    let mut session = Session::default();
    effectcraft_script::install(&mut session);
    let mut s = McpServer::new(Backend::headless(session));
    let result = call_json(&mut s, "run_script", json!({"code":"app.project.items.addComp('Only once',64,64,1,1,30); 'x'.repeat(24000)"}));
    assert!(result.to_string().len() <= 7000);
    assert!(result["result_id"].is_string());
    let mut exact = String::new();
    let mut offset = 0;
    loop {
        let page = call_json(&mut s, "get_result", json!({"result_id":result["result_id"],"pointer":"/result","mode":"json","offset":offset}));
        assert!(page.to_string().len() <= 7000);
        exact.push_str(page["text"].as_str().unwrap());
        match page["next_offset"].as_u64() {
            Some(next) => {
                assert!(next > offset);
                offset = next;
            }
            None => break,
        }
    }
    assert_eq!(serde_json::from_str::<String>(&exact).unwrap(), "x".repeat(24000));
    let project = call_json(&mut s, "get_project", json!({"full":true}));
    assert_eq!(project["items"].as_array().unwrap().iter().filter(|item| item["type"] == "Composition").count(), 1);
}

#[test]
fn stale_host_discovery_does_not_replace_the_current_backend() {
    struct Discovery {
        port: u16,
        path: String,
    }
    impl crate::ControlTransport for Discovery {
        fn mode(&self) -> &'static str {
            "discovery"
        }
        fn call(&mut self, _: &str, p: Value) -> crate::Result<Value> {
            match p["command"].as_str() {
                Some("hosts.list") => Ok(json!({"hosts":[{"port":self.port,"project":self.path}]})),
                Some("project.summary") => Ok(json!({"revision":0,"activeComp":null,"path":null,"items":[]})),
                Some("expr.errors") => Ok(json!([])),
                other => Err(crate::Error::App(format!("unsupported discovery query: {other:?}"))),
            }
        }
    }
    let dir = autosave_test_dir("stale-host");
    let path = dir.join("expected.ecproj");
    std::fs::write(&path, "{}").unwrap();
    let port = fake_app(); // Its live session does not have expected.ecproj open.
    let mut s = McpServer::new(Backend::transport(Discovery { port, path: path.to_string_lossy().into_owned() }));
    let (content, error) = call(&mut s, "hosts", json!({"join":path}));
    assert!(error, "{content:?}");
    assert_eq!(s.backend().mode(), "discovery", "a stale discovery record must not switch sessions");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn oversized_errors_keep_failure_status_and_a_bounded_recovery_handle() {
    struct LongError;
    impl crate::ControlTransport for LongError {
        fn call(&mut self, _: &str, _: Value) -> crate::Result<Value> {
            Err(crate::Error::App("bad input ".repeat(5000)))
        }
    }
    let mut s = McpServer::new(Backend::transport(LongError));
    let (content, error) = call(&mut s, "get_comp", json!({}));
    assert!(error);
    let text = content[0]["text"].as_str().unwrap();
    assert!(text.len() <= 7000);
    let value: Value = serde_json::from_str(text).unwrap();
    assert_eq!(value["ok"], false);
    assert!(value["result_id"].is_string());
}
