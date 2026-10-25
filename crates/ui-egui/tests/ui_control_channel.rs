//! Control channel robustness: cancelled (timed-out) requests never run, `events.subscribe`
//! change events, and `ui.screenshot`'s `max_side`.

use std::sync::mpsc::TryRecvError;

use effectcraft_engine::Session;
use effectcraft_ui_egui::control::{self, ControlEvents};
use effectcraft_ui_egui::{ControlRequest, EffectcraftApp};
use egui_kittest::Harness;
use serde_json::{Value, json};

/// Regression: the server replied "timeout" but the UI thread still ran the request later, so a
/// client's retry created a duplicate.
#[test]
fn a_cancelled_request_is_skipped() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe(|_| EffectcraftApp::new(Session::default()).with_control(rx));
    h.run_steps(2);
    let before = h.state().session.revision;
    let (req, reply) = ControlRequest::new("engine.execute", json!({"command": "comp.new", "params": {"name": "Late"}}));
    assert!(req.state.cancel(), "not started yet");
    tx.send(req).unwrap();
    h.run_steps(3);
    assert_eq!(h.state().session.revision, before, "a cancelled request must not run");
    assert_eq!(reply.try_recv().unwrap_err(), TryRecvError::Disconnected);
    // A live one runs, and can't be cancelled once started.
    let (req, reply) = ControlRequest::new("engine.execute", json!({"command": "comp.new", "params": {"name": "Now"}}));
    let state = req.state.clone();
    tx.send(req).unwrap();
    h.run_steps(2);
    assert_eq!(reply.try_recv().unwrap()["ok"], true);
    assert!(!state.cancel());
    assert!(h.state().session.revision > before);
}

fn next_event(rx: &std::sync::mpsc::Receiver<Value>, h: &mut Harness<'_, EffectcraftApp>) -> Value {
    for _ in 0..40 {
        if let Ok(v) = rx.try_recv() {
            return v;
        }
        h.step();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("no event");
}

#[test]
fn subscribers_hear_project_and_selection_changes() {
    let (tx, rx) = std::sync::mpsc::channel();
    let events = ControlEvents::default();
    let hub = events.clone();
    let mut h = Harness::builder()
        .with_size(egui::vec2(1280.0, 800.0))
        .build_eframe(move |_| EffectcraftApp::new(Session::default()).with_control(rx).with_control_events(hub));
    h.run_steps(2);
    let (sub, revision) = events.subscribe();
    assert_eq!(revision, h.state().session.revision);
    // Nothing changed: nothing is sent.
    h.run_steps(3);
    assert!(sub.try_recv().is_err());

    let (req, reply) = ControlRequest::new("engine.execute", json!({"command": "comp.new", "params": {"name": "E"}}));
    tx.send(req).unwrap();
    h.run_steps(2);
    assert_eq!(reply.try_recv().unwrap()["ok"], true);
    let ev = next_event(&sub, &mut h);
    assert_eq!(ev["event"], "project.changed", "{ev}");
    assert_eq!(ev["revision"], h.state().session.revision);

    // An edit from the app itself (not the control channel) is published too.
    let layer = h.state_mut().session.execute("layer.newSolid", json!({"color": "#336699"})).unwrap();
    let mut seen = vec![];
    while seen.len() < 4 {
        let ev = next_event(&sub, &mut h);
        let done = ev["event"] == "selection.changed";
        seen.push(ev);
        if done {
            break;
        }
    }
    assert!(seen.iter().any(|e| e["event"] == "project.changed"), "{seen:?}");
    let sel = seen.iter().find(|e| e["event"] == "selection.changed").expect("selection.changed");
    assert_eq!(sel["layers"], json!([layer["layer"].as_u64().or(layer.as_u64()).unwrap()]), "{sel} {layer}");

    // A dropped subscriber is forgotten.
    drop(sub);
    h.state_mut().session.execute("comp.new", json!({})).unwrap();
    h.run_steps(20);
    assert_eq!(events.subscribers(), 0);
}

/// A full 4K window was a 31 MB PNG: screenshots are downscaled to `max_side` (default 1600)
/// and compressed.
#[test]
fn screenshots_are_capped_and_compressed() {
    let (w, h) = (3840u32, 2036u32);
    let rgba: Vec<u8> = (0..w * h).flat_map(|i| [(i % 251) as u8, (i / w % 256) as u8, 40, 255]).collect();
    let (small, sw, sh) = control::shrink(rgba.clone(), w, h, control::SCREENSHOT_MAX_SIDE);
    assert_eq!((sw, sh), (1600, 848));
    assert_eq!(small.len(), (sw * sh * 4) as usize);
    let png = control::encode_png_fast(&small, sw, sh).unwrap();
    assert!(png.len() < 2 * 1024 * 1024, "{} bytes", png.len());
    assert_eq!(image::load_from_memory(&png).unwrap().width(), 1600);
    // 0 keeps the full size.
    let (_, fw, fh) = control::shrink(rgba, w, h, 0);
    assert_eq!((fw, fh), (w, h));
}

/// Run frames until `reply` arrives (at most 30 s: the job runs on another thread, and a loaded
/// machine can take a while to schedule it).
fn wait_reply(h: &mut Harness<'_, EffectcraftApp>, reply: &std::sync::mpsc::Receiver<Value>) -> Option<Value> {
    let t0 = std::time::Instant::now();
    while t0.elapsed() < std::time::Duration::from_secs(30) {
        if let Ok(v) = reply.try_recv() {
            return Some(v);
        }
        h.run_steps(1);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    None
}

/// A session whose project has one footage item (its file is missing), so `footage.check` runs a
/// background task (with no footage it returns at once, without a job).
fn session_with_footage() -> Session {
    use effectcraft_project::{Footage, ItemKind};
    let mut s = Session::default();
    let f = Footage { path: "/nonexistent/ec/control-channel.png".into(), width: 64, height: 36, has_video: true, ..Default::default() };
    std::sync::Arc::make_mut(&mut s.project).add_item("Gone", Default::default(), None, ItemKind::Footage(f));
    s
}

/// Multi-agent fairness: on desktop a `wait: true` job command no longer blocks the UI thread
/// (and with it every other client's requests). It starts the job, the reply waits for it, and
/// the reply is what the blocking command returns. `jobs.wait` waits the same way.
#[test]
fn waiting_job_commands_reply_when_the_job_ends_without_blocking() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe(|_| EffectcraftApp::new(session_with_footage()).with_control(rx));
    h.run_steps(2);
    let run = |h: &mut Harness<'_, EffectcraftApp>, command: &str, params: Value| {
        let (req, reply) = ControlRequest::new("engine.execute", json!({"command": command, "params": params}));
        tx.send(req).unwrap();
        h.run_steps(1);
        reply
    };
    run(&mut h, "comp.new", json!({"name": "Main", "width": 64, "height": 36})).recv().unwrap();
    run(&mut h, "layer.newSolid", json!({"name": "S", "width": 8, "height": 8})).recv().unwrap();
    let mut blocking = session_with_footage();
    blocking.execute_checked("comp.new", json!({"name": "Main", "width": 64, "height": 36})).unwrap();
    blocking.execute_checked("layer.newSolid", json!({"name": "S", "width": 8, "height": 8})).unwrap();
    let expected = blocking.execute_checked("footage.check", json!({"wait": true})).unwrap();
    assert_eq!(expected["checked"], 1, "{expected}");

    let waiting = run(&mut h, "footage.check", json!({"wait": true}));
    // Another client's request is answered while the job runs (or right after it).
    let other = run(&mut h, "comp.new", json!({"name": "Meanwhile"}));
    let reply = wait_reply(&mut h, &waiting).expect("the waiting command replied once its job ended");
    assert_eq!(reply, json!({"ok": true, "result": expected}));
    assert_eq!(other.recv().unwrap()["ok"], true);
    assert!(h.state().session.jobs().is_empty());

    // jobs.wait with a task running replies when it is done, with the finished jobs.
    run(&mut h, "footage.check", json!({})).recv().unwrap();
    let waited = run(&mut h, "jobs.wait", json!({}));
    let finished = wait_reply(&mut h, &waited).expect("jobs.wait replied");
    assert_eq!(finished["ok"], true, "{finished}");
    assert!(finished["result"]["finished"].as_array().is_some_and(|a| a.iter().any(|j| j["kind"] == "footageCheck")), "{finished}");
}

/// Renders through the control channel (the `render.frame` method and the engine commands, as a
/// bridged agent sends them) run off the UI thread on desktop and reply with what the blocking
/// render returns; a request sent meanwhile is answered too.
#[test]
fn renders_reply_without_blocking_the_ui_thread() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe(|_| EffectcraftApp::new(Session::default()).with_control(rx));
    h.run_steps(2);
    let send = |method: &str, params: Value| {
        let (req, reply) = ControlRequest::new(method, params);
        tx.send(req).unwrap();
        reply
    };
    let wait = |h: &mut Harness<'_, EffectcraftApp>, reply: &std::sync::mpsc::Receiver<Value>| {
        for _ in 0..400 {
            if let Ok(v) = reply.try_recv() {
                return v;
            }
            h.run_steps(1);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("no reply");
    };
    let r = send("engine.execute", json!({"command": "comp.new", "params": {"name": "Main", "width": 64, "height": 36}}));
    wait(&mut h, &r);
    let r = send("engine.execute", json!({"command": "layer.newSolid", "params": {"color": "#ff0000", "width": 16, "height": 16}}));
    wait(&mut h, &r);
    let expected = {
        let mut s = Session::default();
        s.execute_checked("comp.new", json!({"name": "Main", "width": 64, "height": 36})).unwrap();
        s.execute_checked("layer.newSolid", json!({"color": "#ff0000", "width": 16, "height": 16})).unwrap();
        s.execute_checked("render.frame", json!({"base64": true})).unwrap()
    };
    let by_command = send("engine.execute", json!({"command": "render.frame", "params": {"base64": true}}));
    let by_method = send("render.frame", json!({"base64": true}));
    let meanwhile = send("engine.execute", json!({"command": "comp.new", "params": {"name": "Meanwhile"}}));
    assert_eq!(wait(&mut h, &by_command), json!({"ok": true, "result": expected}));
    assert_eq!(wait(&mut h, &by_method), json!({"ok": true, "result": expected}));
    assert_eq!(wait(&mut h, &meanwhile)["ok"], true);
    assert!(h.state().session.job_log.iter().all(|r| r.result.is_null()), "rendered PNGs don't stay in the job log");
}

/// Agents sharing the app take turns: each client's first request runs before anyone's second.
#[test]
fn control_requests_take_turns_by_client() {
    let req = |client: &str, n: u32| {
        let mut p = json!({"command": "comp.new", "params": {"name": format!("{client}{n}")}});
        if !client.is_empty() {
            p["params"]["client"] = json!(client);
        }
        (ControlRequest::new("engine.execute", p).0, n)
    };
    let reqs = vec![req("a", 1), req("a", 2), req("a", 3), req("b", 1), req("", 1), req("b", 2)];
    let order: Vec<String> = control::fair_order(reqs).iter().map(|(r, _)| r.params["params"]["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(order, ["a1", "b1", "1", "a2", "b2", "a3"]);
}

/// Regression (batch 1): in a comp without a speech map (most comps), the learning loop's
/// `speech.get` over the control channel put "invalid parameters for `speech.get`: the comp has no
/// speech map …" in the user's status toast. A failed query is its caller's error and changes
/// nothing: no toast. A failed edit still shows.
#[test]
fn a_failed_query_does_not_toast_the_user() {
    let mut app = EffectcraftApp::new(Session::default());
    let ctx = egui::Context::default();
    let send = |app: &mut EffectcraftApp, command: &str, params: Value| -> Value {
        let (req, _rx) = ControlRequest::new("engine.execute", json!({"command": command, "params": params}));
        match control::handle(app, &ctx, &req) {
            control::Outcome::Done(v) => v,
            _ => Value::Null,
        }
    };
    assert_eq!(send(&mut app, "comp.new", json!({"name": "Creating Text Presets"}))["ok"], true);
    let r = send(&mut app, "speech.get", json!({}));
    assert_eq!(r["ok"], false, "{r}");
    assert!(r["error"].as_str().unwrap_or("").contains("no speech map"), "the caller still gets the error: {r}");
    assert_eq!(app.ui.status, "", "no toast for a failed read");
    let r = send(&mut app, "speech.import", json!({}));
    assert_eq!(r["ok"], false, "{r}");
    assert!(!app.ui.status.is_empty(), "a failed edit is still reported");
}
