//! Editing harness step 3 (taste check) over MCP: `taste_check` judges moves, `ease_graph`
//! returns the value + speed graph as an image, `lint_frame` knows `off-taste`.

use serde_json::{Value, json};

use crate::{Backend, McpServer};

fn result(s: &mut McpServer, name: &str, args: Value) -> Value {
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string();
    let reply: Value = serde_json::from_str(&s.handle_line(&line).expect("reply")).unwrap();
    assert!(reply.get("error").is_none(), "{name}: {reply}");
    assert_ne!(reply["result"]["isError"], true, "{name}: {reply}");
    reply["result"].clone()
}

fn json_of(r: &Value) -> Value {
    r["content"].as_array().unwrap().iter().find_map(|c| c["text"].as_str().and_then(|t| serde_json::from_str(t).ok())).unwrap()
}

fn cmd(s: &mut McpServer, c: &str, p: Value) -> Value {
    json_of(&result(s, "execute_command", json!({"command": c, "params": p})))
}

#[test]
fn taste_check_and_ease_graph() {
    let mut s = McpServer::new(Backend::headless(effectcraft_host::session()));
    cmd(&mut s, "comp.new", json!({"name": "Main", "width": 1920, "height": 1080, "frameRate": 30, "duration": 5}));
    let l = cmd(&mut s, "layer.newSolid", json!({"name": "Card"}))["layer"].clone();
    for (t, y) in [(1.0, 640.0), (1.6, 540.0)] {
        cmd(&mut s, "prop.addKey", json!({"layer": l, "path": "transform/position", "time": t, "value": [960.0, y], "timeBase": "comp"}));
    }
    let r = json_of(&result(&mut s, "taste_check", json!({"problems_only": true})));
    let m = &r["moves"][0];
    assert_eq!(m["verdict"], "off", "{r:#}");
    // The fix runs as given and makes it ✅.
    let fixed = cmd(&mut s, m["fix"]["command"].as_str().unwrap(), m["fix"]["params"].clone());
    assert_eq!(fixed["after"]["verdict"], "approved", "{fixed:#}");
    let g = result(&mut s, "ease_graph", json!({"layer": l, "property": "transform/position", "width": 480, "height": 270}));
    assert!(g["content"].as_array().unwrap().iter().any(|c| c["type"] == "image"), "{g}");
    let lint = json_of(&result(&mut s, "lint_frame", json!({"start": 0.9, "end": 1.7, "rules": ["off-taste"]})));
    assert_eq!(lint["clean"], true, "{lint:#}");
}
