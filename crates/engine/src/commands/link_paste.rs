//! ⌘V link paste (batch 1, CX2): an Instagram link on the clipboard, pasted onto a card or a
//! picture layer, puts what the link points at into that layer. The paste wheel asks what to take:
//!
//! - a reel or post link: Cover picture / Reel video / Post + caption / Author photo;
//! - a profile link: Profile photo / Whole profile / Latest post / Posts → fill selected (one post
//!   per selected layer, newest first, in layer order).
//!
//! Only the layer's source changes (`layer.replaceSource`): keys, effects, masks, parent and timing
//! stay. A card keeps its card shape and its size on screen (the card mask is fitted to the new
//! picture and Scale makes up for a bigger or smaller picture).
//!
//! The engine never touches the network (it builds for wasm32). `link.paste` without `files` hands
//! the fetch to the app (`Event::Frontend` "link.paste"), which downloads public pages only, never
//! with credentials, and runs `link.paste` again with the downloaded `files`; that run is the one
//! undo step.
//!
//! - `link.choices {url}`: what the wheel offers for a link.
//! - `link.paste {url, choice, layers?, files?}`.

use std::sync::Arc;

use effectcraft_project::{ItemId, LayerId, Project};
use serde_json::{Value, json};

use super::{CommandSpec, bad, has_layers, layer_mut, layers_p, str_p, unlocked};
use crate::{EngineError, Result, Session, cmd, query};

/// What a link points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// A reel (`/reel/<code>`) or a post (`/p/<code>`, `/tv/<code>`).
    Post { code: String, reel: bool },
    /// A profile (`/<handle>/`).
    Profile { handle: String },
}

/// The wheel's choices for a post or reel: (id, label, what the log says while it runs).
pub const POST_CHOICES: [(&str, &str, &str); 4] = [
    ("cover", "Cover picture", "Taking the reel's cover frame (it comes with the link)…"),
    ("video", "Reel video", "Downloading the reel video, playing inside the card…"),
    ("post", "Post + caption", "Taking the post with its caption…"),
    ("author", "Author photo", "Taking the author's profile photo…"),
];

/// The wheel's choices for a profile.
pub const PROFILE_CHOICES: [(&str, &str, &str); 4] = [
    ("avatar", "Profile photo", "Taking the profile photo, full size…"),
    ("whole", "Whole profile", "Opening the profile, screenshotting it…"),
    ("latest", "Latest post", "Taking the newest post's picture…"),
    ("fill", "Posts → fill selected", "Taking the newest posts, one per selected layer…"),
];

/// Instagram paths that are not profiles.
const NOT_PROFILES: [&str; 12] = ["explore", "accounts", "stories", "reels", "direct", "about", "legal", "developer", "web", "challenge", "privacy", "tags"];

/// Parse an Instagram link (with or without `https://`, `www.`, a query or a trailing slash).
pub fn parse(url: &str) -> Option<Link> {
    let u = url.trim();
    if u.len() > 2048 || u.chars().any(char::is_whitespace) {
        return None;
    }
    let u = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://")).unwrap_or(u);
    let u = u.strip_prefix("www.").or_else(|| u.strip_prefix("m.")).unwrap_or(u);
    let rest = u.strip_prefix("instagram.com/").or_else(|| u.strip_prefix("instagr.am/"))?;
    let path = rest.split(['?', '#']).next().unwrap_or("");
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let ok_code = |c: &str| !c.is_empty() && c.len() <= 64 && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    // Handles: letters, digits, `_` and `.` (glow.with.valentina).
    let ok_handle = |c: &str| !c.is_empty() && !c.starts_with('.') && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.');
    match segs.as_slice() {
        [kind @ ("reel" | "reels" | "p" | "tv"), code, ..] if ok_code(code) => {
            Some(Link::Post { code: code.to_string(), reel: matches!(*kind, "reel" | "reels" | "tv") })
        }
        // instagram.com/<handle>/reel/<code> (shared from a profile grid)
        [_, kind @ ("reel" | "p"), code, ..] if ok_code(code) => Some(Link::Post { code: code.to_string(), reel: *kind == "reel" }),
        [handle, ..] if ok_handle(handle) && handle.len() <= 30 && !NOT_PROFILES.contains(handle) && !matches!(*handle, "reel" | "p" | "tv") => {
            Some(Link::Profile { handle: handle.to_string() })
        }
        _ => None,
    }
}

impl Link {
    pub fn choices(&self) -> &'static [(&'static str, &'static str, &'static str); 4] {
        match self {
            Link::Post { .. } => &POST_CHOICES,
            Link::Profile { .. } => &PROFILE_CHOICES,
        }
    }

    /// The public page to fetch.
    pub fn page_url(&self) -> String {
        match self {
            Link::Post { code, reel: true } => format!("https://www.instagram.com/reel/{code}/"),
            Link::Post { code, reel: false } => format!("https://www.instagram.com/p/{code}/"),
            Link::Profile { handle } => format!("https://www.instagram.com/{handle}/"),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Link::Post { reel: true, .. } => "reel",
            Link::Post { .. } => "post",
            Link::Profile { .. } => "profile",
        }
    }
}

fn link_p(p: &Value, cmd: &str) -> Result<Link> {
    let url = str_p(p, "url").ok_or_else(|| bad(cmd, "missing `url` (an Instagram reel, post or profile link)"))?;
    parse(url).ok_or_else(|| bad(cmd, format!("not an Instagram reel, post or profile link: {url}")))
}

fn choices(_s: &mut Session, p: &Value) -> Result<Value> {
    let link = link_p(p, "link.choices")?;
    let items: Vec<Value> = link.choices().iter().map(|(id, label, log)| json!({"id": id, "label": label, "log": log})).collect();
    Ok(json!({"kind": link.kind(), "page": link.page_url(), "choices": items}))
}

/// Run `f` as one undo step called `label`: on an error everything it did is rolled back (as an
/// atomic `engine.batch`).
pub(crate) fn atomic(s: &mut Session, label: &str, f: impl FnOnce(&mut Session) -> Result<Value>) -> Result<Value> {
    let (project, history, state) = (s.project.clone(), s.history.clone(), s.state.clone());
    let (events, journal, learn) = (s.events.clone(), s.journal.clone(), s.learn.clone());
    let key_move = s.key_move.clone();
    let (warp_pending, camera_pending, roto_pending, analyses_checked) =
        (s.warp_pending.clone(), s.camera_pending.clone(), s.roto_pending.clone(), s.analyses_checked.clone());
    match f(s) {
        Ok(v) => {
            super::batch::fold(s, &project, label);
            Ok(v)
        }
        Err(e) => {
            s.project = project;
            s.history = history;
            s.state = state;
            s.events = events;
            s.journal = journal;
            s.learn = learn;
            s.key_move = key_move;
            s.warp_pending = warp_pending;
            s.camera_pending = camera_pending;
            s.roto_pending = roto_pending;
            s.analyses_checked = analyses_checked;
            s.sanitize_state();
            s.bump();
            Err(e)
        }
    }
}

/// The source size of a layer in `project` (0×0 without one).
fn size_in(project: &Arc<Project>, cid: ItemId, lid: LayerId) -> (f64, f64) {
    let Some(l) = project.comp(cid).and_then(|c| c.layer(lid)) else { return (0.0, 0.0) };
    let (w, h) = effectcraft_render::source_size(project, l);
    (w as f64, h as f64)
}

const MAX_FILES: usize = 64;

fn paste(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "link.paste";
    let link = link_p(p, cmd)?;
    let choice = str_p(p, "choice").ok_or_else(|| bad(cmd, "missing `choice` (see link.choices)"))?;
    let Some((_, label, log)) = link.choices().iter().find(|(id, ..)| *id == choice) else {
        let ids: Vec<&str> = link.choices().iter().map(|(id, ..)| *id).collect();
        return Err(bad(cmd, format!("`choice` for a {} link: one of {}", link.kind(), ids.join(", "))));
    };
    let (cid, ids) = layers_p(s, p)?;
    if ids.is_empty() {
        return Err(bad(cmd, "select the card (or picture layer) to paste into, or pass `layers`"));
    }
    let ids = unlocked(s, cid, ids, cmd)?;
    let files: Vec<String> = match p.get("files") {
        None | Some(Value::Null) => vec![],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).take(MAX_FILES).collect(),
        Some(_) => return Err(bad(cmd, "`files`: an array of downloaded file paths")),
    };
    if files.is_empty() {
        // The app fetches it, then runs this again with the files.
        let layers: Vec<u64> = ids.iter().map(|l| l.0).collect();
        s.events.push(crate::Event::Frontend {
            command: cmd.into(),
            params: json!({"url": link.page_url(), "choice": choice, "layers": layers, "comp": cid.0, "kind": link.kind()}),
        });
        return Ok(json!({"fetching": true, "choice": choice, "label": label, "log": log, "layers": layers}));
    }
    // Every file must exist before anything changes.
    let label = format!("Paste Link: {label}");
    let fill = choice == "fill";
    atomic(s, &label, |s| {
        let r = s.execute("file.import", json!({"paths": files}))?;
        let items: Vec<u64> = r["items"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
        if items.is_empty() {
            let why = r["errors"].as_array().and_then(|e| e.first()).and_then(Value::as_str).unwrap_or("nothing could be imported").to_string();
            return Err(EngineError::Other(format!("link.paste: {why}")));
        }
        let mut done = vec![];
        for (i, lid) in ids.iter().enumerate() {
            // Fill: one post per layer, in order (fewer posts than layers: they repeat).
            let Some(item) = (if fill { items.get(i % items.len()) } else { items.first() }).copied() else { continue };
            let before = size_in(&s.project, cid, *lid);
            s.execute("layer.replaceSource", json!({"comp": cid.0, "layer": lid.0, "item": item}))?;
            let after = size_in(&s.project, cid, *lid);
            let card = s.project.comp(cid).and_then(|c| c.layer(*lid)).and_then(|l| super::cards::style_of(s, l));
            if let Some(st) = card {
                let ratio = match (super::cards::card_width(before, &st), super::cards::card_width(after, &st)) {
                    (Some(a), Some(b)) if b > 0.0 => a / b,
                    _ => 1.0,
                };
                let cycle = s.prefs.appearance.cycle_mask_colors;
                let lid = *lid;
                s.edit("Fit Card", None, |proj, _| {
                    let mut next = proj.next_id;
                    let l = layer_mut(proj, cid, lid)?;
                    super::cards::put_card_mask(l, &mut next, after, &st, crate::prefs::mask_color(cycle, 0)).map_err(EngineError::Other)?;
                    super::cards::scale_layer(l, ratio);
                    proj.next_id = next;
                    Ok(())
                })?;
            }
            done.push(json!({"layer": lid.0, "item": item}));
        }
        Ok(json!({"pasted": done, "choice": choice, "items": items}))
    })
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "link.paste",
            "Paste Link",
            [],
            None,
            "{url: an Instagram reel, post or profile link, choice: cover|video|post|author (reel/post) or avatar|whole|latest|fill (profile), layers?, files? (downloaded files; without them the app fetches the public page and runs this again)} → {fetching, log} or {pasted: [{layer, item}]}: only the layers' source changes (keys and effects stay); a card keeps its shape and size; one undo step",
            has_layers,
            paste
        ),
        query!("link.choices", "Link Paste Choices", "{url} → {kind: reel|post|profile, page, choices: [{id, label, log}]}", choices),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reels_posts_and_profiles() {
        assert_eq!(parse("https://www.instagram.com/reel/DPq3xAb_1/?igsh=abc"), Some(Link::Post { code: "DPq3xAb_1".into(), reel: true }));
        assert_eq!(parse("instagram.com/p/C0de-1/"), Some(Link::Post { code: "C0de-1".into(), reel: false }));
        assert_eq!(parse("https://instagram.com/glow.with.valentina/"), Some(Link::Profile { handle: "glow.with.valentina".into() }));
        assert_eq!(parse("https://www.instagram.com/glow_with_valentina/"), Some(Link::Profile { handle: "glow_with_valentina".into() }));
        assert_eq!(parse("https://www.instagram.com/explore/"), None);
        assert_eq!(parse("https://example.com/reel/abc"), None);
        assert_eq!(parse("not a link"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn paste_without_files_asks_the_app_to_fetch() {
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "L", "width": 100, "height": 100, "frameRate": 30, "duration": 1})).unwrap();
        let a = s.execute("layer.newSolid", json!({"color": "#ffffff", "width": 50, "height": 50})).unwrap()["layer"].as_u64().unwrap();
        let undo = s.history.undo.len();
        let r = s.execute("link.paste", json!({"url": "https://www.instagram.com/reel/ABC123/", "choice": "cover", "layers": [a]})).unwrap();
        assert_eq!(r["fetching"], json!(true));
        assert_eq!(s.history.undo.len(), undo, "nothing changed yet");
        assert!(s.events.iter().any(|e| matches!(e, crate::Event::Frontend { command, .. } if command == "link.paste")));
        assert!(s.execute("link.paste", json!({"url": "https://www.instagram.com/reel/ABC123/", "choice": "avatar", "layers": [a]})).is_err());
        // A missing file rolls everything back.
        assert!(
            s.execute(
                "link.paste",
                json!({"url": "https://www.instagram.com/reel/ABC123/", "choice": "cover", "layers": [a], "files": ["/nonexistent/x.jpg"]})
            )
            .is_err()
        );
        assert_eq!(s.history.undo.len(), undo);
    }
}
