//! The file gate's refusals say it's deliberate and how to allow a read (2 Nov 2026: an agent
//! running `effectcraft-cli script --empty` couldn't read an SVG outside the project and took it
//! for a bug). Tests not run yet.

use std::sync::Arc;

use effectcraft_engine::Session;
use serde_json::json;

use crate::run_code;

fn session() -> Session {
    let mut s = Session { expr: Some(Arc::new(effectcraft_expr::Expressions)), expr_check: Some(effectcraft_expr::check_syntax), ..Default::default() };
    crate::install(&mut s);
    s
}

#[test]
fn a_refused_read_says_why_and_how_to_allow_it() {
    let dir = std::env::temp_dir().join(format!("ec-gate-help-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("a.svg");
    std::fs::write(&f, "<svg/>").unwrap();
    let q = serde_json::to_string(&f.to_string_lossy()).unwrap();
    let mut s = session();
    // An unsaved project (the CLI's `--empty`) has no folder to read from.
    let o = run_code(&mut s, &format!("var f = new File({q}); f.open('r'); f.read()"), "gate.jsx");
    let e = o.error.expect("the read was allowed").message;
    for want in ["unsaved", "deliberate", "prefs.set", "scripting.allowScriptsWriteFiles", "file.runScript"] {
        assert!(e.contains(want), "{want} missing in: {e}");
    }
    // The user grants it (UI, CLI `run … prefs.set`, control channel): the same read works.
    s.execute("prefs.set", json!({"key": "scripting.allowScriptsWriteFiles", "value": true})).unwrap();
    let o = run_code(&mut s, &format!("var f = new File({q}); f.open('r'); var t = f.read(); f.close(); t"), "gate.jsx");
    assert!(o.error.is_none(), "{:?}", o.error);
    assert_eq!(o.result, json!("<svg/>"));
    let _ = std::fs::remove_dir_all(&dir);
}
