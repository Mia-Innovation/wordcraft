//! The host command channel (local patch): `window.__wcExec` queues an engine
//! command answered to the parent page as a `wordcraft:exec-result`
//! `postMessage`; `window.__wcSelection` returns the selection serialized on
//! the last frame. The framing page drives both through its own bridge script.
//!
//! `__wcExec(id, json)` takes `{"command": <id>, "params": {…}, "expectRev"?}`
//! — `command` is any engine command (`select.range`, `text.insert`,
//! `file.save`, …) run through [`WordApp::execute`], which never prompts.
//! `expectRev` refuses to run when `Session::rev()` moved past the selection
//! the params were built on. The reply posted to the parent is
//! `{type: "wordcraft:exec-result", id, ok, value|error, rev}`.

use std::cell::RefCell;

use serde_json::{Value, json};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::closure::Closure;
use wordcraft_engine::Selection;
use wordcraft_engine::cmd::pos_json;
use wordcraft_ui_egui::WordApp;

/// One `__wcExec` call, run on the next frame.
struct Pending {
    id: String,
    command: String,
    params: Value,
    expect_rev: Option<u64>,
    /// A request that isn't usable JSON is answered `ok: false` with this.
    malformed: Option<String>,
}

thread_local! {
    /// Commands queued by `__wcExec` since the last frame.
    static PENDING: RefCell<Vec<Pending>> = const { RefCell::new(Vec::new()) };
    /// What `__wcSelection` returns: the last frame's selection, as JSON.
    static SNAPSHOT: RefCell<String> = const { RefCell::new(String::new()) };
    /// (rev, selection) the snapshot was built from — it is rebuilt only when
    /// either moves, so a static caret costs nothing per frame.
    static SNAPSHOT_KEY: RefCell<Option<(u64, Selection)>> = const { RefCell::new(None) };
    /// The frame's egui context, kept so `__wcExec` can wake the loop.
    static CTX: RefCell<Option<egui::Context>> = const { RefCell::new(None) };
}

/// Exposes `__wcExec` / `__wcSelection` on `window`. Called once at startup.
pub fn install() {
    let Some(window) = web_sys::window() else { return };

    let exec = Closure::<dyn FnMut(String, String)>::new(|id: String, body: String| {
        let pending = match serde_json::from_str::<Value>(&body) {
            Ok(v) => Pending {
                id,
                command: v.get("command").and_then(Value::as_str).unwrap_or_default().to_string(),
                params: v.get("params").cloned().filter(|p| !p.is_null()).unwrap_or_else(|| json!({})),
                expect_rev: v.get("expectRev").and_then(Value::as_u64),
                malformed: None,
            },
            Err(e) => {
                Pending { id, command: String::new(), params: json!({}), expect_rev: None, malformed: Some(format!("invalid request json: {e}")) }
            }
        };
        PENDING.with(|queue| queue.borrow_mut().push(pending));
        wake();
    });
    if js_sys::Reflect::set(&window, &"__wcExec".into(), exec.as_ref().unchecked_ref()).is_err() {
        return;
    }
    exec.forget();

    let selection = Closure::<dyn FnMut() -> String>::new(|| SNAPSHOT.with(|s| s.borrow().clone()));
    if js_sys::Reflect::set(&window, &"__wcSelection".into(), selection.as_ref().unchecked_ref()).is_ok() {
        selection.forget();
    }
}

/// Runs every queued command and refreshes the selection snapshot; called
/// once per frame, before the app's own logic.
pub fn drain(app: &mut WordApp, ctx: &egui::Context) {
    CTX.with(|c| *c.borrow_mut() = Some(ctx.clone()));
    let pending: Vec<Pending> = PENDING.with(|q| std::mem::take(&mut *q.borrow_mut()));
    for req in &pending {
        post(&result(app, req));
    }
    snapshot(app);
}

/// The reply to one queued request, with the document's revision after it ran.
fn result(app: &mut WordApp, req: &Pending) -> Value {
    if let Some(error) = &req.malformed {
        return reply(app, req, json!({"ok": false, "error": error}));
    }
    if req.command.is_empty() {
        return reply(app, req, json!({"ok": false, "error": "request needs \"command\""}));
    }
    if let Some(expect) = req.expect_rev
        && app.session.rev() != expect
    {
        return reply(app, req, json!({"ok": false, "error": "stale document"}));
    }
    match app.execute(&req.command, req.params.clone()) {
        Ok(value) => reply(app, req, json!({"ok": true, "value": value})),
        Err(error) => reply(app, req, json!({"ok": false, "error": error})),
    }
}

fn reply(app: &WordApp, req: &Pending, mut body: Value) -> Value {
    body["type"] = json!("wordcraft:exec-result");
    body["id"] = json!(req.id);
    body["rev"] = json!(app.session.rev());
    body
}

/// The selection `__wcSelection` answers, rebuilt only when rev or sel moved.
fn snapshot(app: &WordApp) {
    let key = (app.session.rev(), app.session.sel.clone());
    if SNAPSHOT_KEY.with(|k| k.borrow().as_ref() == Some(&key)) {
        return;
    }
    SNAPSHOT_KEY.with(|k| *k.borrow_mut() = Some(key));
    let s = &app.session;
    let value = json!({
        "text": s.selected_text(),
        "anchor": pos_json(&s.sel.anchor),
        "focus": pos_json(&s.sel.focus),
        "collapsed": s.sel.is_collapsed(),
        "rev": s.rev(),
        "documentId": s.document_id(),
    });
    SNAPSHOT.with(|snap| *snap.borrow_mut() = value.to_string());
}

/// Posts the current selection to the parent as `wordcraft:selection` — the
/// same message a copy would emit (`ui.sendToChat`, the mini toolbar button).
pub fn send_to_chat() {
    let body = SNAPSHOT.with(|s| s.borrow().clone());
    if body.is_empty() || body == "null" {
        return;
    }
    let Ok(mut value) = serde_json::from_str::<Value>(&body) else { return };
    value["type"] = json!("wordcraft:selection");
    post(&value);
}

/// Wakes the frame loop so a queued command is answered without user input.
fn wake() {
    CTX.with(|c| {
        if let Some(ctx) = &*c.borrow() {
            ctx.request_repaint();
        }
    });
}

/// Posts the reply to the same-origin parent page, when framed.
fn post(value: &Value) {
    let Some((parent, origin)) = parent() else { return };
    let Ok(text) = serde_json::to_string(value) else { return };
    let Ok(object) = js_sys::JSON::parse(&text) else { return };
    if let Err(e) = parent.post_message(&object, &origin) {
        log::warn!("wc_bridge: could not post exec result: {e:?}");
    }
}

/// The parent window and this page's origin — `None` unframed or cross-origin,
/// where a reply couldn't be delivered anyway.
fn parent() -> Option<(web_sys::Window, String)> {
    let window = web_sys::window()?;
    let parent = window.parent().ok().flatten()?;
    if js_sys::Object::is(&parent, &window) {
        return None;
    }
    let origin = window.location().origin().ok()?;
    if parent.location().origin().ok()? != origin {
        return None;
    }
    Some((parent, origin))
}
