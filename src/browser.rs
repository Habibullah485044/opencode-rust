//! Goal B: let the agent drive the preview panel.
//!
//! The browser lives in the GUI's native child webview, so tools cannot touch
//! it directly. Instead the server hands a [`Request`] to the GUI over a
//! channel; the GUI evaluates a small JS runtime against the real page and
//! sends the JSON result back. Read-only tools are cheap; nothing here opens a
//! port or spawns another browser.
//!
//! Refused by design: BrowserOS's Chromium fork. This is the same *shape*
//! (navigate / snapshot / click / type / wait) implemented on WebView2.

use crate::provider::{ToolFnSpec, ToolSpec};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

/// Longest page snapshot we hand to the model. A snapshot is the difference
/// between "the agent knows what is on screen" and "the context window is
/// gone"; 8k characters is roughly 2k tokens on typical markup.
pub const SNAPSHOT_BUDGET: usize = 8_000;

/// One call the GUI has to run against its webview.
pub struct Request {
    pub method: String,
    pub input: Value,
    pub reply: oneshot::Sender<Result<String, String>>,
}

/// Server-side half of the bridge. Cloneable; dropping it means the panel is
/// gone (closed window, or the GUI never started).
#[derive(Clone)]
pub struct Handle {
    tx: mpsc::UnboundedSender<Request>,
    wake: std::sync::Arc<dyn Fn() + Send + Sync>,
}

impl Handle {
    pub fn new(
        tx: mpsc::UnboundedSender<Request>,
        wake: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self { tx, wake }
    }

    /// Run one browser action and wait for its JSON result.
    pub async fn call(&self, method: &str, input: Value) -> Result<String, String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Request { method: method.to_string(), input, reply: tx })
            .map_err(|_| "the preview panel is not open".to_string())?;
        // The GUI's event loop is parked in `ControlFlow::Wait`; a queued
        // command alone would never be noticed.
        (self.wake)();
        match rx.await {
            Ok(res) => res,
            Err(_) => Err("the preview panel closed before answering".to_string()),
        }
    }
}

/// Tool definitions for the model. Names are `browser_*` so dispatch and the
/// permission defaults can recognise them without a second registry.
pub fn specs() -> Vec<ToolSpec> {
    fn spec(name: &str, description: &str, params: Value) -> ToolSpec {
        ToolSpec {
            kind: "function".into(),
            function: ToolFnSpec {
                name: name.into(),
                description: description.into(),
                parameters: params,
            },
        }
    }
    vec![
        spec(
            "browser_navigate",
            "Load a URL in the embedded preview panel and wait for it to finish loading. \
             Use this before snapshot/click/type on a page.",
            json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}),
        ),
        spec(
            "browser_snapshot",
            "Read the current page as a compact list of interactive elements (ref, role, name, \
             value, href) plus headings and a short text extract. Elements carry `ref` ids used \
             by browser_click / browser_type. Prefer this over fetching raw HTML.",
            json!({"type":"object","properties":{}}),
        ),
        spec(
            "browser_click",
            "Click the element with the given ref from the most recent browser_snapshot.",
            json!({"type":"object","properties":{"ref":{"type":"string"}},"required":["ref"]}),
        ),
        spec(
            "browser_type",
            "Type text into the element with the given ref, then optionally submit the form.",
            json!({
                "type":"object",
                "properties":{
                    "ref":{"type":"string"},
                    "text":{"type":"string"},
                    "submit":{"type":"boolean"}
                },
                "required":["ref","text"]
            }),
        ),
        spec(
            "browser_wait",
            "Wait until a CSS selector exists, some text appears, or a number of milliseconds pass.",
            json!({
                "type":"object",
                "properties":{
                    "selector":{"type":"string"},
                    "text":{"type":"string"},
                    "ms":{"type":"integer"}
                }
            }),
        ),
    ]
}

/// Dispatch one `browser_*` tool call. `None` = not ours.
pub async fn execute(
    handle: &Handle,
    name: &str,
    input: &Value,
) -> Option<Result<String, String>> {
    let res = match name {
        "browser_navigate" => {
            let url = input.get("url").and_then(Value::as_str).unwrap_or("").trim();
            if url.is_empty() {
                return Some(Err("browser_navigate needs a `url`".into()));
            }
            handle.call("navigate", json!({ "url": url })).await
        }
        "browser_snapshot" => handle.call("snapshot", json!({})).await.map(|out| {
            // Hard cap: a huge page must not blow the context window.
            let (body, cut) = truncate(&out, SNAPSHOT_BUDGET);
            if cut {
                format!("{body}\n[snapshot truncated: page exceeded {SNAPSHOT_BUDGET} chars]")
            } else {
                body
            }
        }),
        "browser_click" => {
            let r = input.get("ref").and_then(Value::as_str).unwrap_or("");
            if !valid_ref(r) {
                return Some(Err(format!("`{r}` is not a ref like `e12` — take a fresh snapshot")));
            }
            handle.call("click", json!({ "ref": r })).await
        }
        "browser_type" => {
            let r = input.get("ref").and_then(Value::as_str).unwrap_or("");
            if !valid_ref(r) {
                return Some(Err(format!("`{r}` is not a ref like `e12` — take a fresh snapshot")));
            }
            let text = input.get("text").and_then(Value::as_str).unwrap_or("");
            let submit = input.get("submit").and_then(Value::as_bool).unwrap_or(false);
            handle
                .call("type", json!({ "ref": r, "text": text, "submit": submit }))
                .await
        }
        "browser_wait" => {
            let selector = input.get("selector").and_then(Value::as_str);
            let text = input.get("text").and_then(Value::as_str);
            let ms = input.get("ms").and_then(Value::as_u64);
            if selector.is_none() && text.is_none() && ms.is_none() {
                return Some(Err("browser_wait needs one of `selector`, `text`, `ms`".into()));
            }
            handle
                .call("wait", json!({ "selector": selector, "text": text, "ms": ms }))
                .await
        }
        _ => return None,
    };
    Some(res)
}

/// Refs are generated by our own snapshot; anything else is a model mistake or
/// a stale reference from a previous page.
pub fn valid_ref(r: &str) -> bool {
    let Some(digits) = r.strip_prefix('e') else { return false };
    !digits.is_empty() && digits.len() <= 4 && digits.chars().all(|c| c.is_ascii_digit())
}

/// JS installed into the panel on every navigation. It is the whole reason a
/// snapshot stays small: the page does the pruning, we never receive raw HTML.
///
/// Refs (`data-habi-ref`) are only valid for the snapshot that produced them;
/// a navigation wipes the document, so a stale ref fails loudly instead of
/// clicking something the model did not mean.
pub const RUNTIME_JS: &str = r#"(function () {
  if (window.__habi) return;
  var SEL = 'a[href],button,input,select,textarea,summary,[contenteditable=""],[contenteditable="true"],[role="button"],[role="link"],[role="checkbox"],[role="tab"],[role="menuitem"],[role="searchbox"]';
  function vis(el) {
    var r = el.getBoundingClientRect();
    var s = getComputedStyle(el);
    return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none';
  }
  function nm(el) {
    var v = el.getAttribute('aria-label') || el.innerText || el.value || el.getAttribute('placeholder') || el.getAttribute('title') || el.tagName;
    return String(v).replace(/\s+/g, ' ').trim().slice(0, 90);
  }
  function role(el) {
    var r = el.getAttribute('role');
    if (r) return r;
    var t = el.tagName.toLowerCase();
    if (t === 'a') return 'link';
    if (t === 'input') return el.type || 'text';
    return t;
  }
  window.__habi = {
    snapshot: function () {
      var nodes = document.querySelectorAll(SEL);
      var out = [];
      for (var i = 0; i < nodes.length && out.length < 100; i++) {
        var el = nodes[i];
        if (!vis(el)) continue;
        var id = 'e' + (out.length + 1);
        el.setAttribute('data-habi-ref', id);
        var n = { ref: id, role: role(el), name: nm(el) };
        if (el.value) n.value = String(el.value).slice(0, 80);
        if (el.tagName === 'A' && el.href) n.href = String(el.href).slice(0, 140);
        if (el.checked) n.checked = true;
        out.push(n);
      }
      var hs = [];
      var hh = document.querySelectorAll('h1,h2,h3');
      for (var j = 0; j < hh.length && hs.length < 15; j++) {
        if (vis(hh[j])) hs.push(String(hh[j].innerText || '').replace(/\s+/g, ' ').trim().slice(0, 110));
      }
      var text = document.body ? String(document.body.innerText || '').replace(/\s+/g, ' ').trim() : '';
      return {
        url: location.href,
        title: document.title,
        ready: document.readyState,
        headings: hs,
        nodes: out,
        text: text.slice(0, 1000)
      };
    },
    click: function (ref) {
      var el = document.querySelector('[data-habi-ref="' + ref + '"]');
      if (!el) throw new Error('no element with ref ' + ref + ' — take a fresh snapshot');
      el.scrollIntoView({ block: 'center' });
      if (el.focus) el.focus();
      el.click();
      return { clicked: ref, name: nm(el), url: location.href };
    },
    type: function (ref, text, submit) {
      var el = document.querySelector('[data-habi-ref="' + ref + '"]');
      if (!el) throw new Error('no element with ref ' + ref + ' — take a fresh snapshot');
      el.scrollIntoView({ block: 'center' });
      el.focus();
      var proto = el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
      var d = Object.getOwnPropertyDescriptor(proto, 'value');
      if (d && d.set) d.set.call(el, text); else el.value = text;
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
      if (submit) {
        if (el.form && el.form.requestSubmit) el.form.requestSubmit();
        else el.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
      }
      return { typed: ref, chars: text.length, submitted: !!submit };
    },
    waitFor: function (id, spec) {
      var t0 = Date.now();
      var limit = spec.timeout_ms || 8000;
      function done(ok, payload) {
        window.ipc.postMessage(JSON.stringify(ok
          ? { hab: id, ok: true, result: payload }
          : { hab: id, ok: false, error: payload }));
      }
      function tick() {
        var found = true;
        if (spec.selector) found = !!document.querySelector(spec.selector);
        else if (spec.text) found = (document.body ? document.body.innerText : '').indexOf(spec.text) >= 0;
        if (found) return done(true, { found: true, ms: Date.now() - t0 });
        if (Date.now() - t0 > limit) return done(false, 'timed out after ' + limit + 'ms');
        setTimeout(tick, 250);
      }
      setTimeout(tick, 200);
    }
  };
})();"#;

fn js_string(input: &Value, key: &str) -> Option<String> {
    let s = input.get(key)?.as_str()?;
    // serde_json quoting: the model's text can contain quotes, newlines, or
    // `</script>` — it must never become JS source unchecked.
    serde_json::to_string(s).ok()
}

/// Wrap one page call so its return value comes back over IPC.
pub fn eval_wrapper(method: &str, input: &Value, id: u64) -> Option<String> {
    let call = match method {
        "snapshot" => "__habi.snapshot()".to_string(),
        "click" => format!("__habi.click({})", js_string(input, "ref")?),
        "type" => format!(
            "__habi.type({}, {}, {})",
            js_string(input, "ref")?,
            js_string(input, "text")?,
            input.get("submit").and_then(Value::as_bool).unwrap_or(false)
        ),
        _ => return None,
    };
    Some(format!(
        "(function(){{try{{var __r=(function(){{return {call};}})();\
         window.ipc.postMessage(JSON.stringify({{hab:{id},ok:true,result:__r}}));}}\
         catch(e){{window.ipc.postMessage(JSON.stringify({{hab:{id},ok:false,error:String((e&&e.message)||e)}}));}}}})();"
    ))
}

/// `browser_wait` runs in the page: it polls and posts exactly one reply.
pub fn wait_script(
    id: u64,
    selector: Option<&str>,
    text: Option<&str>,
    timeout_ms: u64,
) -> String {
    let js = |v: Option<&str>| match v {
        Some(s) => serde_json::to_string(s).unwrap_or_else(|_| "null".into()),
        None => "null".into(),
    };
    format!(
        "__habi.waitFor({id}, {{selector: {}, text: {}, timeout_ms: {timeout_ms}}});",
        js(selector),
        js(text)
    )
}

/// Parse a page → Rust reply frame: `{hab, ok, result|error}`.
pub fn parse_reply(raw: &str) -> Option<(u64, Result<String, String>)> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let id = v.get("hab")?.as_u64()?;
    if v.get("ok")?.as_bool()? {
        let result = v.get("result").cloned().unwrap_or(Value::Null);
        let text = match result {
            Value::String(s) => s,
            other => other.to_string(),
        };
        Some((id, Ok(text)))
    } else {
        let err = v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the page rejected the action");
        Some((id, Err(err.to_string())))
    }
}

/// Cut a string to `max` chars on a char boundary, reporting whether it was cut.
pub fn truncate(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s.to_string(), false);
    }
    (s.chars().take(max).collect(), true)
}

/// First `http://localhost:<port>` / `http://127.0.0.1:<port>` in tool output.
///
/// Dev servers print their URL in whatever shape the framework likes
/// ("Local: http://localhost:5173/", "listening on http://127.0.0.1:3000"),
/// so this scans rather than parses. Returns the origin only — the chip and
/// the CTA want something navigable, not a log line.
pub fn detect_dev_server(text: &str) -> Option<String> {
    const PREFIXES: [&str; 2] = ["http://localhost:", "http://127.0.0.1:"];
    for (i, _) in text.char_indices() {
        let Some(tail) = text.get(i..) else { continue };
        for p in PREFIXES {
            let Some(rest) = tail.strip_prefix(p) else { continue };
            let port: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if port.is_empty() {
                continue;
            }
            return Some(format!("{p}{port}"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_must_look_like_our_own() {
        assert!(valid_ref("e1"));
        assert!(valid_ref("e042"));
        assert!(!valid_ref("e"));
        assert!(!valid_ref("x1"));
        assert!(!valid_ref("e1a"));
        assert!(!valid_ref("e12345"), "refs are bounded — no arbitrary selectors");
        assert!(!valid_ref("e1\"]), *"));
    }

    #[test]
    fn snapshots_are_capped_on_a_char_boundary() {
        let (out, cut) = truncate("abc", 10);
        assert_eq!(out, "abc");
        assert!(!cut);
        let (out, cut) = truncate("🔒🔒🔒", 2);
        assert_eq!(out, "🔒🔒");
        assert!(cut, "the model must be told the page was clipped");

        let (out, cut) = truncate(&"x".repeat(SNAPSHOT_BUDGET + 50), SNAPSHOT_BUDGET);
        assert_eq!(out.chars().count(), SNAPSHOT_BUDGET);
        assert!(cut);
    }

    #[tokio::test]
    async fn a_closed_panel_is_an_error_not_a_hang() {
        let (tx, rx) = mpsc::unbounded_channel::<Request>();
        drop(rx);
        let h = Handle::new(tx, std::sync::Arc::new(|| {}));
        let err = h.call("snapshot", json!({})).await.unwrap_err();
        assert!(err.contains("not open"), "{err}");
    }

    #[tokio::test]
    async fn calls_round_trip_through_the_channel() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Request>();
        let h = Handle::new(tx, std::sync::Arc::new(|| {}));
        tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert_eq!(req.method, "click");
            let _ = req.reply.send(Ok("{\"clicked\":\"e1\"}".into()));
        });
        let out = h.call("click", json!({"ref":"e1"})).await.unwrap();
        assert!(out.contains("clicked"));
    }

    #[test]
    fn eval_wrapper_keeps_model_text_out_of_the_js_grammar() {
        let js = eval_wrapper("type", &json!({"ref":"e3","text":"a\"); alert(1); (\"","submit":true}), 7)
            .expect("type is a page op");
        assert!(js.starts_with("(function(){try{"), "{js}");
        // The quote after `a` is escaped, so the text stays one string literal.
        assert!(js.contains(r#"__habi.type("e3", "a\"#), "escaped text: {js}");
        assert!(js.contains(", true)"), "submit flag is a boolean literal: {js}");
        assert!(js.contains("hab:7"));
        assert!(eval_wrapper("navigate", &json!({}), 1).is_none(), "navigation is not a page op");
        assert!(eval_wrapper("click", &json!({}), 1).is_none(), "click needs a ref");
    }

    #[test]
    fn wait_script_carries_the_condition_and_budget() {
        let js = wait_script(4, Some("#done"), Some("Saved"), 8_000);
        assert!(js.starts_with("__habi.waitFor(4,"), "{js}");
        assert!(js.contains("timeout_ms: 8000"), "{js}");
        assert!(js.contains("#done") && js.contains("Saved"), "{js}");
        // Missing conditions become JSON null, never invalid JS.
        let only_ms = wait_script(5, None, None, 10);
        assert!(only_ms.contains("selector: null, text: null"), "{only_ms}");
    }

    #[test]
    fn reply_frames_are_matched_by_id() {
        let (id, res) = parse_reply(r#"{"hab":3,"ok":true,"result":{"url":"x"}}"#).unwrap();
        assert_eq!(id, 3);
        assert!(res.unwrap().contains("\"url\":\"x\""));

        let (id, res) = parse_reply(r#"{"hab":9,"ok":false,"error":"boom"}"#).unwrap();
        assert_eq!(id, 9);
        assert_eq!(res.unwrap_err(), "boom");

        // The app page also posts IPC frames on this channel; those are not
        // ours and must be ignored rather than answered.
        assert!(parse_reply(r#"{"type":"ready"}"#).is_none());
        assert!(parse_reply(r#"{"hab":"x","ok":true}"#).is_none());
        assert!(parse_reply("not json").is_none());
    }

    #[test]
    fn runtime_defines_the_primitives_the_wrappers_call() {
        for need in ["__habi", "snapshot:", "click:", "type:", "waitFor:", "data-habi-ref"] {
            assert!(RUNTIME_JS.contains(need), "runtime is missing {need}");
        }
    }

    #[tokio::test]
    async fn execute_rejects_bad_refs_before_reaching_the_page() {
        let (tx, _rx) = mpsc::unbounded_channel::<Request>();
        let h = Handle::new(tx, std::sync::Arc::new(|| {}));
        let err = execute(&h, "browser_click", &json!({"ref":"body"})).await.unwrap().unwrap_err();
        assert!(err.contains("fresh snapshot"), "{err}");
        assert!(execute(&h, "bash", &json!({})).await.is_none(), "not a browser tool");
    }

    /// A-B5: dev servers announce themselves in log output; the chip and CTA
    /// need the origin, not the sentence around it.
    #[test]
    fn dev_server_is_detected_in_tool_output() {
        assert_eq!(
            detect_dev_server("  VITE v5 ready\n  ➜  Local:   http://localhost:5173/\n").as_deref(),
            Some("http://localhost:5173")
        );
        assert_eq!(
            detect_dev_server("listening on http://127.0.0.1:8080").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(detect_dev_server("http://localhost:"), None, "no port, no chip");
        assert_eq!(detect_dev_server("https://localhost:3000 is tls"), None, "tls is not a dev server");
        assert_eq!(detect_dev_server("compiled successfully"), None);
    }
}
