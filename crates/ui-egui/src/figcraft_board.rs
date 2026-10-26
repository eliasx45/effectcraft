//! Board this → FigCraft (SX5): `script.board` builds a storyboard (one 16:9 frame per beat of the
//! picked script lines: its words, the frame, the stressed word) and the app sends it to the
//! FigCraft dev app over its control channel (newline-delimited JSON on 127.0.0.1, port
//! `FIGCRAFT_CONTROL_PORT`, default 7979; FigCraft docs/control-protocol.md) as one
//! `command.batch` of `node.create` steps: the board frame, then a frame and a text per beat.
//! Loopback only; FigCraft not running is a plain error (the board JSON is still in the result).
//!
//! TODO(batch1-verify): FigCraft's `node.create` params (`type`, `parent: {"$step": n}`, `name`,
//! `x`, `y`, `width`, `height`, `text`) were read from its mcp.md, not tried; "Approve & build"
//! (building the board back into the comp) is not wired: Claude builds it over MCP.

use serde_json::{Value, json};

const W: f64 = 480.0;
const H: f64 = 270.0;
const GAP: f64 = 40.0;

/// The FigCraft batch for a board (`script.board`'s result: `{name, frames: [{text, frame, peak?}]}`).
pub fn batch(board: &Value) -> Value {
    let name = board.get("name").and_then(Value::as_str).unwrap_or("Storyboard");
    let frames = board.get("frames").and_then(Value::as_array).cloned().unwrap_or_default();
    let n = frames.len().max(1) as f64;
    let mut cmds = vec![json!({"command": "node.create", "params": {
        "type": "frame", "name": format!("Storyboard · {name}"), "x": 0, "y": 0,
        "width": GAP + n * (W + GAP), "height": H + 2.0 * GAP,
    }})];
    for (i, f) in frames.iter().take(200).enumerate() {
        let text = f.get("text").and_then(Value::as_str).unwrap_or("");
        let at = f.get("frame").and_then(Value::as_i64).unwrap_or(0);
        let peak = f.get("peak").and_then(Value::as_str);
        let step = cmds.len();
        cmds.push(json!({"command": "node.create", "params": {
            "type": "frame", "parent": {"$step": 0}, "name": format!("f{at}"),
            "x": GAP + i as f64 * (W + GAP), "y": GAP, "width": W, "height": H,
        }}));
        let label = match peak {
            Some(p) => format!("{text}\n\npeak on {p} · f{at}"),
            None => format!("{text}\n\nf{at}"),
        };
        cmds.push(json!({"command": "node.create", "params": {"type": "text", "parent": {"$step": step}, "x": 24, "y": 24, "text": label}}));
    }
    json!({"label": format!("Storyboard · {name}"), "commands": cmds})
}

#[cfg(not(target_arch = "wasm32"))]
pub fn send(board: &Value) -> Result<Value, String> {
    use std::io::{BufRead, BufReader, Read, Write};
    let port: u16 = std::env::var("FIGCRAFT_CONTROL_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(7979);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2))
        .map_err(|e| format!("FigCraft isn't listening on port {port} (open the FigCraft dev app): {e}"))?;
    let timeout = Some(std::time::Duration::from_secs(20));
    let _ = s.set_read_timeout(timeout);
    let _ = s.set_write_timeout(timeout);
    let req = json!({"id": 1, "method": "engine.execute", "params": {"command": "command.batch", "params": batch(board)}});
    let mut line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
    line.push('\n');
    s.write_all(line.as_bytes()).map_err(|e| format!("sending to FigCraft: {e}"))?;
    let mut reply = String::new();
    // One reply line, at most 4 MiB.
    BufReader::new(&s).take(4 << 20).read_line(&mut reply).map_err(|e| format!("FigCraft didn't answer: {e}"))?;
    let v: Value = serde_json::from_str(reply.trim()).map_err(|e| format!("FigCraft's answer isn't JSON: {e}"))?;
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(json!({"sent": true, "port": port, "result": v.get("result").cloned().unwrap_or(Value::Null)}))
    } else {
        Err(format!("FigCraft: {}", v.get("error").and_then(Value::as_str).unwrap_or("refused the storyboard")))
    }
}

#[cfg(target_arch = "wasm32")]
pub fn send(_: &Value) -> Result<Value, String> {
    Err("Board this → FigCraft runs in the desktop app".into())
}
