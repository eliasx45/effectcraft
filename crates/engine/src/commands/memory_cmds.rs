//! The live memory map (Window ▸ Memory): show the panel, read it, purge by comp / layer.
//! See [`crate::memory_map`] and plan/memory/memory-map.md.

use serde_json::{Value, json};

use super::{CommandSpec, always, bad, comp_id, str_p};
use crate::memory_map::{self, What};
use crate::{Result, Session, cmd, query};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "memory.show",
            "Memory",
            [],
            None,
            "{} — Window ▸ Memory: what the app's memory holds, by cache and by comp → layer → effect (also opened by clicking the RAM chip in the Timeline footer)",
            always,
            show
        ),
        query!(
            "memory.stats",
            "Memory Map",
            "{tree?: bool (default true: comp → layer → effect rows; false: totals only, cheaper)} — the process's memory against the cap (total − RAM reserved), bytes per cache (preview, layer, media, GPU, untracked) and per comp / layer / effect",
            stats
        ),
        cmd!(
            "memory.purge",
            "Purge Memory",
            [],
            None,
            "{comp?: id or name, layer?: id (needs comp), what?: all|layers|preview|media} — without comp: every cache; with comp: its cached layer buffers and RAM preview frames; with layer: that layer's buffers (decoded footage is shared: only a purge of everything empties it)",
            always,
            purge
        ),
    ]
}

fn show(s: &mut Session, _: &Value) -> Result<Value> {
    s.events.push(crate::Event::Frontend { command: "window.panel".into(), params: json!({"panel": "memory"}) });
    Ok(json!({"frontend": "window.panel", "panel": "memory"}))
}

fn stats(s: &mut Session, p: &Value) -> Result<Value> {
    let tree = p.get("tree").and_then(Value::as_bool).unwrap_or(true);
    // A reading for the next call (agents polling see it settle).
    s.memory_map.process.poll();
    Ok(serde_json::to_value(memory_map::stats(s, tree)).unwrap_or(Value::Null))
}

fn purge(s: &mut Session, p: &Value) -> Result<Value> {
    const ID: &str = "memory.purge";
    let what = match str_p(p, "what") {
        None => What::All,
        Some(w) => What::parse(w).ok_or_else(|| bad(ID, format!("unknown `what` {w:?}: all, layers, preview or media")))?,
    };
    // A comp by id may be gone from the project (its buffers still cached): purge it anyway.
    let comp = match p.get("comp") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => Some(n.as_u64().ok_or_else(|| bad(ID, "`comp` must be a comp id or name"))?),
        Some(_) => Some(comp_id(s, p)?.0),
    };
    let layer = match p.get("layer") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| bad(ID, "`layer` must be a layer id"))?),
    };
    if layer.is_some() && comp.is_none() {
        return Err(bad(ID, "`layer` needs `comp`"));
    }
    let freed = memory_map::purge(s, comp, layer, what);
    let total = freed.layer_bytes.saturating_add(freed.preview_bytes).saturating_add(freed.media_bytes);
    s.toast(format!("Freed {:.1} GB", total as f64 / f64::from(1u32 << 30)));
    Ok(serde_json::to_value(freed).unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Session;

    #[test]
    fn stats_and_purge_run_headless() {
        let mut s = Session::new();
        let st = s.execute("memory.stats", json!({})).unwrap();
        assert!(st["categories"]["layer"].is_u64());
        assert!(st["comps"].is_array());
        let st = s.execute("memory.stats", json!({"tree": false})).unwrap();
        assert_eq!(st["comps"], json!([]));
        let r = s.execute("memory.purge", json!({"comp": 12345, "layer": 1})).unwrap();
        assert_eq!(r["layerEntries"], json!(0));
        assert!(s.execute("memory.purge", json!({"layer": 1})).is_err());
        assert!(s.execute("memory.purge", json!({"what": "nonsense"})).is_err());
        let r = s.execute("memory.show", json!({})).unwrap();
        assert_eq!(r["panel"], json!("memory"));
    }
}
