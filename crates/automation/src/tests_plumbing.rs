//! Editing harness step 1 (plumbing): the MCP friction agents hit most (argument names, scripts
//! and their client, retained results, loud failures, short replies).

use serde_json::{Value, json};

use crate::{Backend, McpServer};

/// A headless server with scripting and expressions (as `effectcraft-cli mcp`).
fn server() -> McpServer {
    McpServer::new(Backend::headless(effectcraft_host::session()))
}

fn tmp(tag: &str) -> std::path::PathBuf {
    let d = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-out").join(format!("plumbing-{tag}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Call a tool; returns (reply JSON or error text, isError).
fn call(s: &mut McpServer, name: &str, args: Value) -> (Value, bool) {
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string();
    let reply: Value = serde_json::from_str(&s.handle_line(&line).expect("reply")).unwrap();
    if let Some(e) = reply.get("error") {
        return (e["message"].clone(), true);
    }
    let r = &reply["result"];
    let text = r["content"][0]["text"].as_str().unwrap_or_default();
    (serde_json::from_str(text).unwrap_or_else(|_| json!(text)), r["isError"] == true)
}

fn ok(s: &mut McpServer, name: &str, args: Value) -> Value {
    let (v, err) = call(s, name, args.clone());
    assert!(!err, "{name} {args}: {v}");
    v
}

#[test]
fn tools_accept_the_argument_names_of_their_siblings() {
    let mut s = server();
    // `query` (list_fonts' name) on list_effects and command_list, which take `filter`.
    let effects = ok(&mut s, "list_effects", json!({"query": "gaussian", "brief": true}));
    assert!(effects.to_string().to_lowercase().contains("gaussian"), "{effects}");
    let commands = ok(&mut s, "command_list", json!({"query": "layer.newText", "brief": true}));
    assert!(commands.to_string().contains("layer.newText"), "{commands}");
    // `source` / `script` for run_script's `code`.
    assert_eq!(ok(&mut s, "run_script", json!({"source": "1 + 1"}))["result"], 2);
    assert_eq!(ok(&mut s, "run_script", json!({"script": "2 + 2"}))["result"], 4);
    // camelCase for snake_case (and back).
    ok(&mut s, "command_run", json!({"id": "comp.new", "params": {"name": "Main", "width": 64, "height": 36}}));
    let sheet = ok(&mut s, "render_frames", json!({"count": 2, "maxSide": 128, "inline": false}));
    assert!(sheet["width"].as_u64().unwrap() <= 128, "{sheet}");
    ok(&mut s, "command_list", json!({"filter": "comp.new", "enabledOnly": true, "brief": true}));
    // The canonical name wins when both are given; a real unknown argument is still refused.
    let (e, err) = call(&mut s, "list_effects", json!({"bogus": 1}));
    assert!(err && e.as_str().unwrap().contains("unknown argument \"bogus\""), "{e}");
    // get_field's own `source` is not taken for `code`.
    let (e, err) = call(&mut s, "get_field", json!({"source": "get_comp", "pointer": "/name", "code": "x"}));
    assert!(err && e.as_str().unwrap().contains("unknown argument \"code\""), "{e}");
}

#[test]
fn run_script_runs_as_the_callers_client_and_includes_next_to_its_file() {
    let mut s = server();
    // comp.claim inside a script holds the claim for the caller's client.
    let r = ok(
        &mut s,
        "run_script",
        json!({"client": "builder-a", "code": "var c = app.project.items.addComp('C', 64, 36, 1, 1, 30); app.run('comp.claim', {comp: c.id}); c.id"}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let claims = ok(&mut s, "command_run", json!({"id": "comp.claims", "params": {}}));
    assert_eq!(claims["claims"][0]["client"], "builder-a", "{claims}");
    // `file`: the code comes from the file and #include resolves next to it.
    let dir = tmp("include");
    std::fs::write(dir.join("common.jsx"), "function twice(x) { return 2 * x; }").unwrap();
    std::fs::write(dir.join("build.jsx"), "#include \"common.jsx\"\ntwice(21)").unwrap();
    let main = dir.join("build.jsx").to_string_lossy().to_string();
    assert_eq!(ok(&mut s, "run_script", json!({"file": main}))["result"], 42);
    // `code` with `file`: the code runs, its includes still resolve next to the file.
    assert_eq!(ok(&mut s, "run_script", json!({"file": main, "code": "#include \"common.jsx\"\ntwice(5)"}))["result"], 10);
    let (e, err) = call(&mut s, "run_script", json!({}));
    assert!(err && e.to_string().contains("`file`"), "{e}");
}

#[test]
fn get_result_finds_a_named_clients_result_when_the_call_names_none() {
    let mut s = server();
    let big = ok(
        &mut s,
        "run_script",
        json!({"client": "builder-b", "code": "var a = []; for (var i = 0; i < 3000; i++) a.push('row ' + i); a.join('\\n')", "max_chars": 2000}),
    );
    let id = big["result_id"].as_str().unwrap_or_else(|| panic!("not retained: {big}")).to_string();
    // The same agent forgot `client` on get_result: the id alone finds it.
    let page = ok(&mut s, "get_result", json!({"result_id": id, "mode": "json", "pointer": "/result", "max_chars": 4000}));
    assert_eq!(page["result_id"], id.as_str());
    // Another named client is still refused, told whose it is.
    let (e, err) = call(&mut s, "get_result", json!({"result_id": id, "client": "builder-c"}));
    assert!(err && e.to_string().contains("builder-b"), "{e}");
    assert!(!call(&mut s, "get_result", json!({"result_id": id, "client": "builder-b"})).1);
}

#[test]
fn a_failing_script_rolls_back_and_a_broken_expression_is_reported() {
    let mut s = server();
    ok(&mut s, "command_run", json!({"id": "comp.new", "params": {"name": "Main", "width": 64, "height": 36}}));
    let comps = |s: &mut McpServer| ok(s, "doc_inspect", json!({}))["project"].to_string().matches("Half").count();
    // Edits, then throws: nothing is left behind.
    let (r, err) = call(
        &mut s,
        "run_script",
        json!({"code": "app.project.items.addComp('Half', 64, 36, 1, 1, 30); app.project.activeItem.layers.addNull(); throw new Error('boom')"}),
    );
    assert!(err);
    assert_eq!(r["ok"], false, "{r}");
    assert_eq!(r["rolledBack"], true, "{r}");
    assert_eq!(comps(&mut s), 0);
    // rollback: false keeps them (After Effects' behaviour).
    let (r, _) = call(&mut s, "run_script", json!({"rollback": false, "code": "app.project.items.addComp('Half', 64, 36, 1, 1, 30); throw new Error('boom')"}));
    assert!(r.get("rolledBack").is_none(), "{r}");
    assert_eq!(comps(&mut s), 1);
    // An expression reading a marker that isn't there is an error the reply names.
    let r = ok(
        &mut s,
        "run_script",
        json!({"code": "var l = app.project.activeItem.layers.addNull(); l.name = 'Reader'; l.property('ADBE Transform Group').property('ADBE Opacity').expression = 'thisComp.marker.key(\"NOPE\").time'; 1"}),
    );
    let errors = &r["expressionErrors"];
    assert_eq!(errors["count"], 1, "{r}");
    assert_eq!(errors["errors"][0]["layerName"], "Reader");
    assert!(errors["errors"][0]["message"].as_str().unwrap().contains("NOPE"), "{errors}");
    // Only new errors: an unrelated edit doesn't repeat it.
    let r = ok(&mut s, "run_script", json!({"code": "app.project.activeItem.layers.addNull(); 1"}));
    assert!(r.get("expressionErrors").is_none(), "{r}");
    // observedContext only on request.
    assert!(r.get("observedContext").is_none(), "{r}");
    assert!(ok(&mut s, "run_script", json!({"code": "1", "context": true})).get("observedContext").is_some());
}

#[test]
fn library_insert_reports_expressions_that_fail_after_applying() {
    let dir = tmp("library");
    // A card whose layer reads a comp marker the card doesn't have.
    let mut maker = server();
    ok(&mut maker, "command_run", json!({"id": "comp.new", "params": {"name": "Card", "width": 64, "height": 36}}));
    ok(
        &mut maker,
        "run_script",
        json!({"code": "var l = app.project.activeItem.layers.addNull(); l.property('ADBE Transform Group').property('ADBE Opacity').expression = 'thisComp.marker.key(\"CLIENTS START\").time'; 1"}),
    );
    let path = dir.join("card.ecproj").to_string_lossy().to_string();
    ok(&mut maker, "command_run", json!({"id": "file.saveAs", "params": {"path": path}}));
    let index = json!({"format": "effectcraft-library", "version": 1, "name": "Kit",
        "entries": [{"id": "card", "name": "Card", "kind": "comp", "file": "card.ecproj", "comp": "Card"}]});
    std::fs::write(dir.join("library.json"), index.to_string()).unwrap();

    let mut s = server();
    ok(&mut s, "command_run", json!({"id": "library.addFolder", "params": {"path": dir.to_string_lossy()}}));
    ok(&mut s, "command_run", json!({"id": "comp.new", "params": {"name": "Show", "width": 64, "height": 36}}));
    let r = ok(&mut s, "command_run", json!({"id": "library.insert", "params": {"id": "card"}}));
    assert_eq!(r["expressionErrors"]["count"], 1, "{r}");
    assert!(r["warnings"][0].as_str().unwrap().contains("CLIENTS START"), "{r}");
}

#[test]
fn command_batch_inlines_small_results_and_lists_failed_steps() {
    let mut s = server();
    let (r, failed) = call(
        &mut s,
        "command_batch",
        json!({"stop_on_error": false, "steps": [
            {"id": "comp.new", "params": {"name": "B", "width": 64, "height": 36}},
            {"id": "layer.newNull"},
            {"id": "no.such.command"},
            {"id": "command.list", "params": {"filter": "layer"}}
        ]}),
    );
    assert!(failed, "a failed step makes the call an error");
    assert_eq!((r["completed"].as_u64(), r["failed"].as_u64()), (Some(3), Some(1)), "{r}");
    // Small results stay inline (the new layer's id), big ones keep only their ids.
    assert!(r["results"][1]["result"]["layer"].is_number(), "{r}");
    assert_eq!(r["results"][3]["omitted"], true, "{r}");
    assert!(r.to_string().len() < 4000, "{} chars", r.to_string().len());
    assert_eq!(r["failedSteps"][0]["step"], 3);
    assert_eq!(r["failedSteps"][0]["id"], "no.such.command");
    // verbose: everything.
    let v = ok(&mut s, "command_batch", json!({"verbose": true, "steps": [{"id": "command.list", "params": {"filter": "layer"}}]}));
    assert!(v["results"][0]["result"].is_object() || v["result_id"].is_string(), "{v}");
}
