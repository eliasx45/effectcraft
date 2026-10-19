//! Cards in the UI (batch 1, CX1): the card wheel ("Make card ▾" in the quick row, lane A, calls
//! [`open_card_wheel`]). Its slices are the card styles (`card.styles`: Devin 2:3 · 8.6 %, Reel
//! 9:16, Post 4:5, Square 1:1, Very round, Sharp, then the saved ones) and "+ Save mine".
//! Picking a style runs `card.make` (pictures become cards, cards are restyled) or `card.style`
//! (every selected layer is a card already); "+ Save mine" runs `card.saveStyle`.
//!
//! Everything is a command, so agents use `card.make {layers, style}` directly.
//!
//! ⌘V link paste (CX2): with a layer selected and an Instagram link on the clipboard, ⌘V opens
//! the paste wheel ([`before_shortcuts`] takes the paste before Edit ▸ Paste sees it). The pick
//! runs `link.paste {url, choice}`; its frontend half ([`start_fetch`], `menus/agent_native.rs`)
//! downloads the public page's picture or video on a thread (curl, no cookies, no login), and the
//! next frame runs `link.paste` again with the files (one undo step). The steps tick in the
//! Motion Library's Link tab ([`link_tab`]).

use std::sync::{Mutex, MutexGuard, PoisonError};

use egui::{Align2, Pos2, Rect, pos2, vec2};
use serde_json::{Value, json};

use crate::theme::Tokens;

use crate::EffectcraftApp;
use crate::wheel::{self, WheelItem};

/// The card wheel's key (`wheel.card.<style id>` automation ids).
pub const CARD_WHEEL: &str = "card";

fn items_id() -> egui::Id {
    egui::Id::new("cards-wheel-items")
}

/// The wheel's slices for `styles` (a `card.styles` reply), then "+ Save mine".
fn wheel_items(styles: &Value) -> Vec<WheelItem> {
    let mut items: Vec<WheelItem> = styles["styles"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|st| {
                    let id = st.get("id")?.as_str()?;
                    let label = st.get("label")?.as_str()?;
                    let hint = format!(
                        "{} · corners {} % of the width{}",
                        st.get("aspect").and_then(Value::as_str).unwrap_or(""),
                        st.get("radius").and_then(Value::as_f64).unwrap_or(0.0),
                        if st.get("saved").and_then(Value::as_bool) == Some(true) { " · Motion Library ▸ Elias ▸ Cards" } else { "" }
                    );
                    Some(WheelItem::new(id, label, &hint))
                })
                .collect()
        })
        .unwrap_or_default();
    if items.is_empty() {
        items.push(WheelItem::new("devin", "Devin 2:3 · 8.6 %", "2:3 · corners 8.6 % of the width"));
    }
    items.push(WheelItem::new("save", "+ Save mine", "Save the selected card's style to Motion Library ▸ Elias ▸ Cards"));
    items
}

/// Open the card wheel around `center` (the "Make card ▾" button, lane A's quick row).
pub fn open_card_wheel(ctx: &egui::Context, center: Pos2) {
    let items: Vec<WheelItem> = ctx.data(|d| d.get_temp::<Vec<WheelItem>>(items_id())).unwrap_or_else(|| wheel_items(&Value::Null));
    wheel::open(ctx, CARD_WHEEL, center, "CARD", items, None);
}

/// Once per frame (from the app's frame, next to `wheel::paint`): keeps the wheel's styles
/// current and runs a picked slice.
pub fn frame(app: &mut EffectcraftApp, ctx: &egui::Context) {
    // The styles change with the settings (a saved style): read them again then.
    let rev_id = egui::Id::new("cards-wheel-prefs-rev");
    let rev = app.session.prefs_revision;
    if ctx.data(|d| d.get_temp::<u64>(rev_id)) != Some(rev) || ctx.data(|d| d.get_temp::<Vec<WheelItem>>(items_id())).is_none() {
        // A query: not journaled, no undo step.
        let styles = app.session.execute("card.styles", json!({})).unwrap_or(Value::Null);
        let items = wheel_items(&styles);
        ctx.data_mut(|d| {
            d.insert_temp(rev_id, rev);
            d.insert_temp(items_id(), items);
        });
    }
    if let Some(pick) = wheel::take_pick(ctx, CARD_WHEEL) {
        let r = pick_style(app, ctx, &pick);
        match r {
            Ok(msg) => app.toast = Some((msg, ctx.input(|i| i.time))),
            Err(e) => app.ui.status = e,
        }
    }
    link_frame(app, ctx);
}

/// Run the picked slice on the selected layers.
fn pick_style(app: &mut EffectcraftApp, ctx: &egui::Context, pick: &str) -> Result<String, String> {
    if app.session.state.selected_layers.is_empty() {
        return Err("Select a card or a picture first".into());
    }
    if pick == "save" {
        let r = crate::menus::invoke(app, ctx, "card.saveStyle", json!({}))?;
        let label = r["style"]["label"].as_str().unwrap_or("your style");
        return Ok(format!("Saved to Motion Library ▸ Elias ▸ Cards as “{label}”: it’s on the wheel now"));
    }
    // Restyle when every selected layer is a card already; otherwise make cards (which also
    // restyles the cards among them).
    let all_cards = app.session.active_comp().is_some_and(|c| {
        app.session.state.selected_layers.iter().all(|id| c.layer(*id).is_some_and(effectcraft_engine::commands::cards::is_card))
    });
    let id = if all_cards { "card.style" } else { "card.make" };
    let r = crate::menus::invoke(app, ctx, id, json!({"style": pick}))?;
    let label = r["style"]["label"].as_str().unwrap_or(pick);
    let n = r["cards"].as_array().map(Vec::len).unwrap_or(0);
    let skipped = r["skipped"].as_array().map(Vec::len).unwrap_or(0);
    let what = if all_cards { "Restyled" } else { "Card made" };
    Ok(if skipped > 0 { format!("{what}: {label} on {n} layer(s); {skipped} skipped") } else { format!("{what}: {label} · from Motion Library ▸ Elias ▸ Cards") })
}

// ── ⌘V link paste (CX2) ────────────────────────────────────────────────────────────────────

/// The paste wheel's key (`wheel.paste.<choice>`).
pub const PASTE_WHEEL: &str = "paste";

/// The Link tab's log and the fetch in flight.
#[derive(Default)]
struct LinkState {
    /// (seconds since the app started, line), oldest first.
    log: Vec<(f64, String)>,
    /// The link the paste wheel is open for.
    wheel_url: Option<String>,
    /// A finished fetch: the `link.paste` params plus `files`, or the error.
    done: Option<Result<Value, String>>,
    busy: bool,
    /// The Link tab's URL field.
    field: String,
}

static LINK: Mutex<Option<LinkState>> = Mutex::new(None);

fn link_state() -> MutexGuard<'static, Option<LinkState>> {
    LINK.lock().unwrap_or_else(PoisonError::into_inner)
}

fn with_link<T>(f: impl FnOnce(&mut LinkState) -> T) -> T {
    let mut g = link_state();
    f(g.get_or_insert_with(LinkState::default))
}

const MAX_LOG: usize = 200;

fn log_line(now: f64, line: impl Into<String>) {
    with_link(|st| {
        st.log.push((now, line.into()));
        let over = st.log.len().saturating_sub(MAX_LOG);
        st.log.drain(..over);
    });
}

/// Before the shortcuts run (from the app's frame): ⌘V with an Instagram link on the clipboard
/// and a layer selected opens the paste wheel instead of Edit ▸ Paste.
pub fn before_shortcuts(app: &mut EffectcraftApp, ctx: &egui::Context) {
    if app.session.state.text_edit.is_some() || ctx.egui_wants_keyboard_input() || app.session.state.selected_layers.is_empty() {
        return;
    }
    let Some(text) = ctx.input(|i| i.events.iter().rev().find_map(|e| if let egui::Event::Paste(t) = e { Some(t.clone()) } else { None })) else { return };
    let url = text.trim().to_string();
    // A query: what the wheel offers (an error = not an Instagram link: Edit ▸ Paste as usual).
    let Ok(r) = app.session.execute("link.choices", json!({"url": url})) else { return };
    let items: Vec<WheelItem> = r["choices"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| Some(WheelItem::new(c.get("id")?.as_str()?, c.get("label")?.as_str()?, c.get("log").and_then(Value::as_str).unwrap_or(""))))
                .collect()
        })
        .unwrap_or_default();
    if items.is_empty() {
        return;
    }
    // Edit ▸ Paste doesn't get this paste.
    ctx.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Paste(_))));
    let at = ctx.input(|i| i.pointer.hover_pos()).unwrap_or_else(|| ctx.content_rect().center());
    with_link(|st| st.wheel_url = Some(url.clone()));
    log_line(ctx.input(|i| i.time), format!("{} link on the clipboard: {}", r["kind"].as_str().unwrap_or("Instagram"), url));
    wheel::open(ctx, PASTE_WHEEL, at, "⌘V", items, None);
}

/// Paste `url` taking `choice` into the selected layers (the wheel's pick, the Link tab).
pub fn paste_link(app: &mut EffectcraftApp, ctx: &egui::Context, url: &str, choice: &str) {
    let now = ctx.input(|i| i.time);
    match crate::menus::invoke(app, ctx, "link.paste", json!({"url": url, "choice": choice})) {
        Ok(r) => {
            if let Some(l) = r["log"].as_str() {
                log_line(now, l.to_string());
            }
        }
        Err(e) => {
            log_line(now, format!("✗ {e}"));
            app.ui.status = e;
        }
    }
}

/// Wheel picks and finished fetches, once per frame.
fn link_frame(app: &mut EffectcraftApp, ctx: &egui::Context) {
    if let Some(choice) = wheel::take_pick(ctx, PASTE_WHEEL)
        && let Some(url) = with_link(|st| st.wheel_url.take())
    {
        paste_link(app, ctx, &url, &choice);
    }
    let Some(done) = with_link(|st| st.done.take()) else { return };
    let now = ctx.input(|i| i.time);
    match done {
        Ok(params) => {
            log_line(now, "Fitting it into the card…");
            match crate::menus::invoke(app, ctx, "link.paste", params) {
                Ok(r) => {
                    let n = r["pasted"].as_array().map(Vec::len).unwrap_or(0);
                    let msg = if n > 1 { format!("✓ Done: {n} layers, only their pictures changed") } else { "✓ Done: only the picture changed".to_string() };
                    log_line(now, msg.clone());
                    app.toast = Some((msg, now));
                }
                Err(e) => {
                    log_line(now, format!("✗ {e}"));
                    app.ui.status = e;
                }
            }
        }
        Err(e) => {
            log_line(now, format!("✗ {e}"));
            app.ui.status = e;
        }
    }
}

/// The frontend half of `link.paste` without files: fetch on a thread.
pub fn start_fetch(app: &mut EffectcraftApp, ctx: &egui::Context, params: &Value) -> Result<Value, String> {
    let _ = app;
    if with_link(|st| st.busy) {
        return Err("A link is still being fetched; wait for it (Motion Library ▸ Link)".into());
    }
    let page = params.get("url").and_then(Value::as_str).ok_or("link.paste: no url")?.to_string();
    let choice = params.get("choice").and_then(Value::as_str).ok_or("link.paste: no choice")?.to_string();
    let count = params.get("layers").and_then(Value::as_array).map(Vec::len).unwrap_or(1).max(1);
    with_link(|st| st.busy = true);
    log_line(ctx.input(|i| i.time), format!("Fetching {page} …"));
    let c2 = ctx.clone();
    let mut params = params.clone();
    let work = move || {
        let r = fetch::files(&page, &choice, count).map(|files| {
            if let Some(o) = params.as_object_mut() {
                o.remove("kind");
                o.insert("files".into(), json!(files));
            }
            params
        });
        with_link(|st| {
            st.busy = false;
            st.done = Some(r);
        });
        c2.request_repaint();
    };
    #[cfg(not(target_arch = "wasm32"))]
    let spawned = std::thread::Builder::new().name("ec-link-fetch".into()).spawn(work).is_ok();
    #[cfg(target_arch = "wasm32")]
    let spawned = {
        drop(work);
        false
    };
    if !spawned {
        with_link(|st| st.busy = false);
        return Err("couldn't start the link download (link paste works in the desktop app)".into());
    }
    Ok(json!({"fetching": true}))
}

/// Fetching public Instagram pages (desktop only; no cookies, no login, public pages only).
mod fetch {
    /// The browser the request says it is (Instagram serves its Open Graph tags to it).
    const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";
    const MAX_PAGE: usize = 8 << 20;

    #[cfg(not(target_arch = "wasm32"))]
    fn curl(url: &str, out: Option<&std::path::Path>) -> Result<Vec<u8>, String> {
        if !url.starts_with("https://") {
            return Err(format!("only https links are fetched ({url})"));
        }
        let mut cmd = std::process::Command::new("curl");
        // No cookies or credentials: a fresh, logged-out request. TODO(batch1-verify): curl on Windows (curl.exe ships with Windows 10+).
        cmd.args(["-sS", "-L", "--fail", "--max-time", "30", "--max-filesize", "300000000", "--proto", "=https", "-A", UA, "-H", "Accept-Language: en-US,en"]);
        if let Some(p) = out {
            cmd.arg("-o").arg(p);
        }
        cmd.arg(url);
        let o = cmd.output().map_err(|e| format!("couldn't run curl: {e}"))?;
        if !o.status.success() {
            let err = String::from_utf8_lossy(&o.stderr);
            return Err(format!("Instagram didn't answer ({}): {}", url, err.trim().lines().last().unwrap_or("no reply")));
        }
        Ok(o.stdout)
    }

    #[cfg(target_arch = "wasm32")]
    fn curl(_url: &str, _out: Option<&std::path::Path>) -> Result<Vec<u8>, String> {
        Err("Link paste works in the desktop app".into())
    }

    /// `content` of `<meta property="{prop}" content="…">` in `html` (either attribute order).
    pub(super) fn meta(html: &str, prop: &str) -> Option<String> {
        let needle = format!("\"{prop}\"");
        let mut from = 0;
        while let Some(i) = html.get(from..)?.find(&needle) {
            let at = from + i;
            let start = html.get(..at)?.rfind('<')?;
            let end = at + html.get(at..)?.find('>')?;
            let tag = html.get(start..end)?;
            if tag.starts_with("<meta")
                && let Some(c) = tag.find("content=\"")
            {
                let rest = tag.get(c + 9..)?;
                let v = rest.get(..rest.find('"')?)?;
                return Some(unescape(v));
            }
            from = at + needle.len();
        }
        None
    }

    fn unescape(s: &str) -> String {
        s.replace("&amp;", "&").replace("&quot;", "\"").replace("&#039;", "'").replace("&#x27;", "'").replace("&lt;", "<").replace("&gt;", ">")
    }

    fn page(url: &str) -> Result<String, String> {
        let bytes = curl(url, None)?;
        if bytes.len() > MAX_PAGE {
            return Err("the page is too big".into());
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The author's handle of a post page (`@handle` in its title or description).
    pub(super) fn author(html: &str) -> Option<String> {
        let text = meta(html, "og:title").into_iter().chain(meta(html, "og:description")).collect::<Vec<_>>().join(" ");
        let i = text.find('@')?;
        let h: String = text.get(i + 1..)?.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.').collect();
        let h = h.trim_end_matches('.').to_string();
        (!h.is_empty()).then_some(h)
    }

    fn save(url: &str, stem: &str, ext: &str) -> Result<String, String> {
        let dir = std::env::temp_dir().join("effectcraft-links");
        std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't make {}: {e}", dir.display()))?;
        let safe: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(80).collect();
        let path = dir.join(format!("{safe}.{ext}"));
        curl(url, Some(&path))?;
        Ok(path.to_string_lossy().into_owned())
    }

    fn ext_of(url: &str, fallback: &str) -> String {
        let path = url.split(['?', '#']).next().unwrap_or("");
        let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        if ["jpg", "jpeg", "png", "webp", "heic", "mp4", "mov"].contains(&ext.as_str()) { ext } else { fallback.to_string() }
    }

    /// Download what `choice` takes from the public page `page_url`; `count` layers are waiting.
    pub(super) fn files(page_url: &str, choice: &str, count: usize) -> Result<Vec<String>, String> {
        let html = page(page_url)?;
        let stem = page_url.trim_end_matches('/').rsplit('/').next().unwrap_or("link").to_string();
        let image = || meta(&html, "og:image").ok_or_else(|| "Instagram showed no picture to a logged-out visitor (a private account?)".to_string());
        match choice {
            "cover" => {
                let u = image()?;
                Ok(vec![save(&u, &format!("{stem}-cover"), &ext_of(&u, "jpg"))?])
            }
            "video" => {
                let u = meta(&html, "og:video:secure_url")
                    .or_else(|| meta(&html, "og:video"))
                    .ok_or("Instagram didn't give the video to a logged-out visitor; try Cover picture")?;
                Ok(vec![save(&u, &format!("{stem}-video"), &ext_of(&u, "mp4"))?])
            }
            // TODO(batch1-verify): "Post + caption" should be a screenshot of the post with its caption
            // (an invisible browser); until then it takes the post's picture.
            "post" => {
                let u = image()?;
                Ok(vec![save(&u, &format!("{stem}-post"), &ext_of(&u, "jpg"))?])
            }
            "author" => {
                let handle = author(&html).ok_or("couldn't find the author's name on the post page")?;
                let prof = page(&format!("https://www.instagram.com/{handle}/"))?;
                let u = meta(&prof, "og:image").ok_or("Instagram showed no profile photo to a logged-out visitor")?;
                Ok(vec![save(&u, &format!("{handle}-photo"), &ext_of(&u, "jpg"))?])
            }
            "avatar" => {
                let u = image()?;
                Ok(vec![save(&u, &format!("{stem}-photo"), &ext_of(&u, "jpg"))?])
            }
            // TODO(batch1-verify): these need a logged-out browser screenshot or the profile's post
            // list, which Instagram's public HTML doesn't carry. Clear errors until then.
            "whole" => Err("Whole profile needs a browser screenshot, which EffectCraft can't take yet; use Profile photo".into()),
            "latest" | "fill" => {
                let _ = count;
                Err("Instagram doesn't list a profile's posts to a logged-out visitor; paste the post links one by one".into())
            }
            other => Err(format!("unknown choice `{other}`")),
        }
    }
}

/// The Motion Library's Link tab: how it works, a link field with the choices, and the log.
pub fn link_tab(app: &mut EffectcraftApp, ui: &mut egui::Ui, area: Rect, s: f32) {
    let t = app.tokens;
    let ctx = ui.ctx().clone();
    let p = ui.painter().with_clip_rect(area);
    p.rect_filled(area, 0.0, t.panel_bg);
    let x0 = area.min.x + 12.0 * s;
    let w = (area.width() - 24.0 * s).max(60.0);
    let mut y = area.min.y + 12.0 * s;
    p.text(pos2(x0, y + 8.0 * s), Align2::LEFT_CENTER, "Paste an Instagram link onto a layer", Tokens::semibold(12.5 * s), t.text);
    y += 22.0 * s;
    let how = "Copy an Instagram link → select a layer → ⌘V → a wheel asks what to take. EffectCraft fetches the public page and puts it inside that layer: only its picture changes.";
    let g = p.layout(how.to_string(), Tokens::ui(11.0 * s), t.text_dim, w);
    let gh = g.size().y;
    p.galley(pos2(x0, y), g, t.text_dim);
    y += gh + 10.0 * s;
    // The link field and the choices it offers.
    let mut field = with_link(|st| st.field.clone());
    let fr = Rect::from_min_size(pos2(x0, y), vec2(w, 22.0 * s));
    let resp = ui.put(fr, egui::TextEdit::singleline(&mut field).hint_text("instagram.com/reel/… or instagram.com/<name>/"));
    app.auto.add_response("motionLibrary.link.url", &resp, "Instagram link");
    with_link(|st| st.field = field.clone());
    y += 28.0 * s;
    if let Ok(r) = app.session.execute("link.choices", json!({"url": field.trim()})) {
        let mut x = x0;
        for c in r["choices"].as_array().cloned().unwrap_or_default() {
            let (Some(id), Some(label)) = (c["id"].as_str(), c["label"].as_str()) else { continue };
            let bw = (label.chars().count() as f32 * 6.6 + 18.0) * s;
            if x + bw > x0 + w {
                x = x0;
                y += 26.0 * s;
            }
            let b = Rect::from_min_size(pos2(x, y), vec2(bw, 22.0 * s));
            if super::panel_kit::button(app, ui, b, &format!("motionLibrary.link.take.{id}"), label, false) {
                let url = field.trim().to_string();
                paste_link(app, &ctx, &url, id);
            }
            x += bw + 6.0 * s;
        }
        y += 30.0 * s;
    } else if !field.trim().is_empty() {
        p.text(pos2(x0, y + 8.0 * s), Align2::LEFT_CENTER, "Not an Instagram reel, post or profile link", Tokens::ui(11.0 * s), t.warning);
        y += 24.0 * s;
    }
    // The log, newest last.
    p.text(pos2(x0, y + 6.0 * s), Align2::LEFT_CENTER, if with_link(|st| st.busy) { "Steps · fetching…" } else { "Steps" }, Tokens::medium(11.0 * s), t.text_faint);
    y += 18.0 * s;
    let lines: Vec<(f64, String)> = with_link(|st| st.log.clone());
    let room = ((area.max.y - y) / (16.0 * s)).floor().max(0.0) as usize;
    let skip = lines.len().saturating_sub(room);
    for (at, line) in lines.iter().skip(skip) {
        let color = if line.starts_with('✓') { t.cache_green } else if line.starts_with('✗') { t.danger } else { t.text };
        p.text(pos2(x0, y + 7.0 * s), Align2::LEFT_CENTER, format!("{} {line}", super::panel_kit::seconds(*at)), Tokens::mono(10.5 * s), color);
        y += 16.0 * s;
    }
    app.auto.add("motionLibrary.link.log", Rect::from_min_max(pos2(x0, area.min.y), area.max), &lines.last().map(|(_, l)| l.clone()).unwrap_or_default());
    if with_link(|st| st.busy) {
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_lists_styles_then_save() {
        let styles = json!({"styles": [{"id": "devin", "label": "Devin 2:3 · 8.6 %", "aspect": "2:3", "radius": 8.6, "saved": false}, {"id": "my-1", "label": "Mine", "aspect": "2:3", "radius": 12.0, "saved": true}]});
        let items = wheel_items(&styles);
        assert_eq!(items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["devin", "my-1", "save"]);
        assert!(items[1].hint.contains("Elias ▸ Cards"));
        // Nothing known yet: Devin's style and Save.
        assert_eq!(wheel_items(&Value::Null).len(), 2);
    }

    #[test]
    fn reads_open_graph_tags_and_the_author() {
        let html = r#"<html><head><meta property="og:image" content="https://x.cdninstagram.com/a.jpg?x=1&amp;y=2" /><meta content="Valentina (@glow.with.valentina) on Instagram" property="og:title"></head></html>"#;
        assert_eq!(fetch::meta(html, "og:image").as_deref(), Some("https://x.cdninstagram.com/a.jpg?x=1&y=2"));
        assert_eq!(fetch::meta(html, "og:title").as_deref(), Some("Valentina (@glow.with.valentina) on Instagram"));
        assert_eq!(fetch::meta(html, "og:video"), None);
        assert_eq!(fetch::author(html).as_deref(), Some("glow.with.valentina"));
    }
}
