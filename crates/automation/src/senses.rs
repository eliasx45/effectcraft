//! What an agent needs to see before it acts (editing harness step 2, "senses"):
//! - time arguments as the Timeline shows them: `"6:20"` (s:ff), `"0:00:06:20"`, `"200f"` or
//!   `{"frame": 200}` next to seconds ([`resolve_times`], [`widen_time_schema`]);
//! - a `notice` on replies when the user or another agent changed the document since the agent's
//!   last call, or another project was opened ([`attach_notices`], engine `agent.notices`);
//! - the `edit_feed` tool: who changed what, in words (engine `history.feed`).

use serde_json::{Value, json};

use crate::backend::Backend;
use crate::tools::{Reply, ToolDef};
use crate::{Error, Result};

/// Argument names whose schema says "seconds" and that also take timecode / frames.
const TIME_ARGS: &[&str] = &["time", "times", "start", "end", "duration"];

const TIME_FORMS: &str = " Also timecode as the Timeline shows it: \"6:20\" (6 s 20 frames), \"0:00:06:20\", or comp frames: {\"frame\": 200} / \"200f\".";

/// Tool arguments with timecode / frame times turned into seconds of their comp, so tools that
/// check numbers see seconds. Arguments without such forms are returned as they are (no call).
pub(crate) fn resolve_times(b: &mut Backend, args: Value) -> Result<Value> {
    if !effectcraft_engine::readout::has_time_forms(&args) {
        return Ok(args);
    }
    let comp = args.get("comp").cloned().unwrap_or(Value::Null);
    let r = b.exec("time.resolve", json!({"comp": comp, "params": args}))?;
    r.get("params").cloned().ok_or_else(|| Error::App("time.resolve returned no params".into()))
}

/// Let the `time`-like number arguments of a tool schema take timecode and frames too.
pub(crate) fn widen_time_schema(schema: &mut Value) {
    let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) else { return };
    for name in TIME_ARGS {
        let Some(p) = props.get_mut(*name).and_then(Value::as_object_mut) else { continue };
        let number = json!("number");
        if p.get("type") == Some(&number) {
            p.insert("type".into(), json!(["number", "string", "object"]));
        } else if p.get("type") == Some(&json!("array")) && p.get("items").and_then(|i| i.get("type")) == Some(&number) {
            p.insert("items".into(), json!({"type": ["number", "string", "object"]}));
        } else {
            continue;
        }
        let d = p.get("description").and_then(Value::as_str).unwrap_or("Seconds.").to_string();
        p.insert("description".into(), json!(format!("{d}{TIME_FORMS}")));
    }
}

/// Add `notice` (the user / other agents changed the document since this client's last call, or
/// another project was opened) to a reply. Nothing for the user's own calls or when nothing changed.
pub(crate) fn attach_notices(b: &mut Backend, name: &str, reply: &mut Reply) {
    if name == "edit_feed" {
        return;
    }
    let target = match reply {
        Reply::Json(v) => v,
        Reply::Image { info, .. } => info,
        #[allow(unreachable_patterns)]
        _ => return,
    };
    if !target.is_object() {
        return;
    }
    let Ok(n) = b.exec("agent.notices", json!({})) else { return };
    if let Some(text) = n.get("text").and_then(Value::as_str) {
        target["notice"] = json!(text);
        if let Some(p) = n.get("projectChanged") {
            target["projectChanged"] = p.clone();
        }
    }
}

fn edit_feed(b: &mut Backend, a: &Value) -> Result<Reply> {
    let mut p = json!({});
    for k in ["since", "limit", "lines", "others"] {
        if let Some(v) = a.get(k).filter(|v| !v.is_null()) {
            p[k] = v.clone();
        }
    }
    Ok(Reply::Json(b.exec("history.feed", p)?))
}

pub(crate) const EDIT_FEED: ToolDef = ToolDef {
    name: "edit_feed",
    description: "Who changed what in the open project, in words: each change with its revision, who made it (`by`: \"user\" = the person at the app, or the agent/script client; `who` is \"you\" for your own) and lines like \"Shape 1 › Position: key @ 6:20 [960, 540] → [900, 540]\" or \"ease @ 6:20: bezier 0/s 33% → bezier 0/s 75%\". Without `since`: the changes since you last read the feed (first call: the latest). Read it before editing after a pause; other tools' replies carry a short `notice` when someone else changed something since your last call.",
    bridge_only: false,
    schema: || {
        json!({"type": "object", "properties": {
            "since": {"type": "integer", "minimum": 0, "description": "Revision to list changes after (from a reply's `revision`)."},
            "limit": {"type": "integer", "minimum": 1, "description": "At most this many changes, the newest (default 20)."},
            "lines": {"type": "integer", "minimum": 1, "description": "Lines per change (default 6)."},
            "others": {"type": "boolean", "description": "Leave out your own changes."}
        }, "additionalProperties": false})
    },
    run: edit_feed,
};

fn appear_plan(b: &mut Backend, a: &Value) -> Result<Reply> {
    let mut p = json!({});
    for k in ["comp", "layers", "accentLead"] {
        if let Some(v) = a.get(k).filter(|v| !v.is_null()) {
            p[k] = v.clone();
        }
    }
    Ok(Reply::Json(b.exec("speech.appearPlan", p)?))
}

pub(crate) const APPEAR_PLAN: ToolDef = ToolDef {
    name: "appear_plan",
    description: "When each layer should appear, from the word it shows (engine speech.appearPlan; needs the comp's speech map). For every text layer (and layers whose name has a spoken word, or the `layers` you list with the `words` each shows) it finds the word in the speech map and compares: an entrance's speed peak should land on the word's stressed syllable (1 frame before the vowel), a layer that cuts in should start on the word. Returns per layer {word, wordFrame, accentFrame, appearFrame, entrance.peakFrame, suggestedFrame, offBy (frames), fix {command, params}} and `fixes` for the ones more than 3 frames off (the off-word lint). Run it after placing layers and before handover; pass `layers: [{layer, words}]` for layers whose text doesn't say their word (a window on \"year\", a bar on \"eight hours\").",
    bridge_only: false,
    schema: || {
        json!({"type": "object", "properties": {
            "comp": {"description": "Comp id or name (default the active comp)."},
            "layers": {"type": "array", "items": {}, "description": "Layers to plan: id, name or \"#n\", or {layer, words? (what it shows), occurrence? (1-based)}. Default: every text layer and layers whose name has a spoken word."},
            "accentLead": {"type": "number", "minimum": 0, "description": "Frames before the stressed vowel the peak should land (default 1)."}
        }, "additionalProperties": false})
    },
    run: appear_plan,
};

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{Backend, McpServer, Session};

    fn rpc(s: &mut McpServer, method: &str, params: Value) -> Value {
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        let reply: Value = serde_json::from_str(&s.handle_line(&line).expect("reply")).unwrap();
        assert!(reply.get("error").is_none(), "{method}: {reply}");
        reply["result"].clone()
    }

    fn call(s: &mut McpServer, name: &str, args: Value) -> Value {
        let r = rpc(s, "tools/call", json!({"name": name, "arguments": args}));
        assert_eq!(r["isError"], false, "{name}: {r}");
        serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn timecode_args_notices_and_the_edit_feed() {
        let mut b = Backend::headless(Session::default());
        b.set_client(Some("bot".into()));
        let mut s = McpServer::new(b);
        rpc(&mut s, "initialize", json!({"protocolVersion": "2025-06-18"}));
        call(&mut s, "execute_command", json!({"command": "comp.new", "params": {"name": "Main", "frameRate": 30, "duration": 10}}));
        let l = call(&mut s, "execute_command", json!({"command": "layer.newSolid", "params": {"name": "Card"}}))["layer"].clone();

        // Timecode in, timecode out (add_keyframe checks its times are numbers).
        let p = call(
            &mut s,
            "add_keyframe",
            json!({"layer": l, "path": "transform/opacity", "keys": [{"time": "0:15", "value": 10}, {"time": {"frame": 30}, "value": 90}], "interpolation": "easyEase"}),
        );
        assert_eq!(p["keys"][0]["tc"], "0:15", "{p}");
        assert_eq!(p["keys"][1]["frame"], 30, "{p}");
        assert!(p["keys"][0]["ease"].as_str().unwrap().contains("bezier"), "compact keys carry the ease readout: {p}");
        assert!(p.get("notice").is_none(), "own edits are no news: {p}");

        // The user (no client) changes something: the agent's next reply says so, once.
        call(
            &mut s,
            "execute_command",
            json!({"command": "prop.set", "params": {"comp": "Main", "layer": l, "path": "transform/rotation", "value": 5, "client": null}}),
        );
        let r = call(&mut s, "get_property", json!({"layer": l, "path": "transform/rotation"}));
        assert!(r["notice"].as_str().unwrap().contains("user · Change Property: Card › Rotation: 0 → 5"), "{r}");
        let r = call(&mut s, "get_property", json!({"layer": l, "path": "transform/rotation"}));
        assert!(r.get("notice").is_none(), "{r}");

        let f = call(&mut s, "edit_feed", json!({"since": 0}));
        let by: Vec<&str> = f["entries"].as_array().unwrap().iter().filter_map(|e| e["who"].as_str()).collect();
        assert!(by.contains(&"you") && by.contains(&"user"), "{f}");

        let tools = rpc(&mut s, "tools/list", json!({}));
        let gp = tools["tools"].as_array().unwrap().iter().find(|t| t["name"] == "get_property").unwrap();
        assert_eq!(gp["inputSchema"]["properties"]["time"]["type"], json!(["number", "string", "object"]));
    }

    #[test]
    fn time_schema_widening_leaves_other_numbers_alone() {
        let mut schema =
            json!({"properties": {"time": {"type": "number"}, "times": {"type": "array", "items": {"type": "number"}}, "width": {"type": "number"}}});
        super::widen_time_schema(&mut schema);
        assert_eq!(schema["properties"]["time"]["type"], json!(["number", "string", "object"]));
        assert_eq!(schema["properties"]["times"]["items"]["type"], json!(["number", "string", "object"]));
        assert_eq!(schema["properties"]["width"]["type"], "number");
    }
}
