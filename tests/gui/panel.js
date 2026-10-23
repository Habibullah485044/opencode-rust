// Appended to a copy of gui.html and run in headless Edge. Writes PASS/FAIL
// lines into #testout so `--dump-dom` can be grepped for them.
//
// The panel deliberately delays revealing the native view by 300ms (CSS cannot
// clip a child view), so this harness collapses that delay to zero instead of
// waiting: `--dump-dom` snapshots at load, before any timer would fire.
(function () {
  var sent = [];
  window.ipc = {
    postMessage: function (s) {
      try { sent.push(JSON.parse(s)); } catch (e) { sent.push({ type: "unparsable", raw: s }); }
    }
  };

  // Deterministic clock: run the panel's reveal delay and rAF work inline.
  var realTimeout = window.setTimeout;
  window.setTimeout = function (fn, ms) {
    if (ms === 300 || ms === 320) { fn(); return 0; }
    return realTimeout(fn, ms);
  };
  window.requestAnimationFrame = function (fn) { fn(0); return 0; };

  var log = [];
  function ok(name, cond, extra) {
    log.push((cond ? "PASS " : "FAIL ") + name + (extra !== undefined ? " [" + extra + "]" : ""));
  }
  function last(type) {
    for (var i = sent.length - 1; i >= 0; i--) if (sent[i].type === type) return sent[i];
    return null;
  }
  function count(type) {
    return sent.filter(function (m) { return m.type === type; }).length;
  }
  function key(el, k) {
    el.dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true, cancelable: true }));
  }
  function click(id) {
    var el = document.getElementById(id);
    if (!el) throw new Error("missing #" + id);
    el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
  }
  function payload(browser) {
    return {
      session: "s", title: "T", busy: false, api: [true, 12],
      sessions: [], entries: [],
      browser: browser,
      dev_server: "http://localhost:5173"
    };
  }
  window.addEventListener("error", function (e) {
    log.push("FAIL script error: " + e.message);
  });

  // The app wires itself on DOMContentLoaded, so the assertions must run after
  // it — otherwise every listener is still unattached and everything "fails".
  function run() {
  var url = document.getElementById("url");
  var panel = document.getElementById("browser");
  ok("page booted (render + initApp present)", typeof window.render === "function" && !!url && !!panel);
  try {

  // --- A-F1: the URL bar is a real, focusable input (the paste bug) ---
  ok("url bar is an <input>", !!url && url.tagName === "INPUT", url && url.tagName);
  ok("url bar is not readonly/disabled", !!url && !url.readOnly && !url.disabled);
  ok("panel starts collapsed", panel.classList.contains("collapsed"));
  url.focus();
  ok("url bar can take focus", document.activeElement === url, document.activeElement && document.activeElement.id);
  url.value = "typed";
  ok("url bar accepts typed text", url.value === "typed", url.value);

  // --- A-F1/A-F2/A-F4: render drives the chrome from the payload ---
  url.blur(); // render must not fight the user while they are typing
  window.render(payload({
    url: "http://localhost:3000/app", title: "Fixture app",
    can_back: true, can_forward: false, loading: true, error: null, visible: true
  }));
  ok("render shows the current url", url.value === "http://localhost:3000/app", url.value);
  ok("back arrow enabled from payload", !document.getElementById("nav-back").disabled);
  ok("forward arrow disabled from payload", document.getElementById("nav-fwd").disabled);
  ok("reload spins while loading", document.getElementById("nav-reload").classList.contains("spin"));
  ok("tab title comes from the page", document.getElementById("b-tab-title").textContent === "Fixture app",
    document.getElementById("b-tab-title").textContent);
  ok("empty card hidden once a page loads", document.getElementById("b-empty").hidden);

  window.render(payload({
    url: "", title: "", can_back: false, can_forward: false, loading: false,
    error: "Blocked: `file:///etc/passwd` - the panel only loads http(s) pages.", visible: false
  }));
  ok("error card shown for a refusal", !document.getElementById("b-error").hidden);
  ok("error text rendered", /Blocked/.test(document.getElementById("b-error-msg").textContent),
    document.getElementById("b-error-msg").textContent.slice(0, 24));
  ok("url bar flagged bad", url.classList.contains("bad"));
  ok("empty card hidden while erroring", document.getElementById("b-empty").hidden);

  // --- A-F1: Enter navigates, with shorthand expansion ---
  sent = [];
  url.value = "localhost:3000/x";
  key(url, "Enter");
  var b1 = last("browse");
  ok("Enter posts browse", !!b1, b1 && b1.url);
  ok("localhost:port expands to http", b1 && b1.url === "http://localhost:3000/x", b1 && b1.url);

  sent = [];
  url.value = ":5173";
  key(url, "Enter");
  ok(":port shorthand expands", last("browse") && last("browse").url === "http://localhost:5173",
    last("browse") && last("browse").url);

  sent = [];
  url.value = "news.ycombinator.com";
  key(url, "Enter");
  ok("bare host gets https", last("browse") && last("browse").url === "https://news.ycombinator.com",
    last("browse") && last("browse").url);

  sent = [];
  url.value = "https://example.com/a";
  key(url, "Escape");
  ok("Escape restores the current url", url.value === "", "value=" + url.value);

  // --- A-F1: paste of a link with surrounding words navigates ---
  sent = [];
  try {
    var dt = new DataTransfer();
    dt.setData("text/plain", "look at https://example.com/page please");
    url.dispatchEvent(new ClipboardEvent("paste", { clipboardData: dt, bubbles: true, cancelable: true }));
    ok("pasting a link navigates", last("browse") && last("browse").url === "https://example.com/page",
      last("browse") && last("browse").url);
  } catch (e) {
    ok("pasting a link navigates", false, "harness: " + e.message);
  }

  // --- A-F2: nav buttons post their verbs ---
  window.render(payload({
    url: "http://localhost:3000", title: "F", can_back: true, can_forward: true,
    loading: false, error: null, visible: true
  }));
  sent = [];
  click("nav-back");
  click("nav-fwd");
  click("nav-reload");
  ok("back posts nav_back", count("nav_back") === 1);
  ok("forward posts nav_fwd", count("nav_fwd") === 1);
  ok("reload posts nav_reload", count("nav_reload") === 1);
  ok("reload stops spinning when loaded", !document.getElementById("nav-reload").classList.contains("spin"));

  // --- A-F2: disabled arrows post nothing ---
  window.render(payload({
    url: "", title: "", can_back: false, can_forward: false, loading: false, error: null, visible: false
  }));
  sent = [];
  click("nav-back");
  ok("disabled back posts nothing", count("nav_back") === 0);
  sent = [];
  click("nav-reload");
  ok("reload with no page focuses the bar instead", count("nav_reload") === 0 && document.activeElement === url);

  // --- A-F3: opening the panel reports geometry, then reveals the view ---
  // Renders above may have surfaced the panel (agent-driven navigation); the
  // toggle test needs a known starting state. The observer frame is flushed so
  // the geometry cache knows the panel is now zero-width.
  if (!panel.classList.contains("collapsed")) {
    click("close-browser-btn");
    window.dispatchEvent(new Event("resize"));
  }
  sent = [];
  ok("close button collapses the panel", panel.classList.contains("collapsed"));
  click("toggle-browser-btn");
  ok("Preview button opens the panel", !panel.classList.contains("collapsed"));
  var rect = last("panel_rect");
  var vis = last("panel_visible");
  ok("open reports a panel rect", !!rect, rect && rect.x + "," + rect.y + " " + rect.w + "x" + rect.h);
  ok("rect is a usable size", !!rect && rect.w > 100 && rect.h > 100, rect && rect.w + "x" + rect.h);
  ok("rect uses whole pixels", !!rect && (rect.w | 0) === rect.w && (rect.h | 0) === rect.h);
  ok("open reveals the native view", !!vis && vis.visible === true, JSON.stringify(vis));
  ok("rect is sent before visibility", !!rect && !!vis, "ordering is what avoids a flash at a stale rect");

  // Resize must re-report geometry (resize event + observer share sendRect).
  sent = [];
  // The panel animates its width; kill the transition so this measures the
  // geometry reporting, not a 250ms animation that has not started yet.
  panel.style.transition = "none";
  panel.style.width = "300px";
  var bc = document.getElementById("bcontent");
  window.dispatchEvent(new Event("resize"));
  var r2 = last("panel_rect");
  ok("resize re-reports the rect", !!r2 && r2.w < rect.w,
    (r2 && r2.w) + " vs " + rect.w + " | panel=" + Math.round(panel.getBoundingClientRect().width) +
    " content=" + Math.round(bc.getBoundingClientRect().width));

  // Collapse: the view must go away immediately, not after the animation.
  sent = [];
  click("close-browser-btn");
  ok("close applies the collapsed class", panel.classList.contains("collapsed"));
  ok("close hides the native view", !!last("panel_visible") && last("panel_visible").visible === false,
    JSON.stringify(last("panel_visible")));

  // Ctrl+L reopens and focuses.
  sent = [];
  document.dispatchEvent(new KeyboardEvent("keydown", { key: "l", ctrlKey: true, bubbles: true, cancelable: true }));
  ok("ctrl+L reopens the panel", !panel.classList.contains("collapsed"));
  ok("ctrl+L focuses the address bar", document.activeElement === url, document.activeElement && document.activeElement.id);
  ok("ctrl+L reveals the view", !!last("panel_visible") && last("panel_visible").visible === true);

  // --- B-F1: an agent navigation must not happen invisibly ---
  if (!panel.classList.contains("collapsed")) click("close-browser-btn");
  ok("panel is collapsed before the agent browses", panel.classList.contains("collapsed"));
  window.render(payload({
    url: "http://localhost:4000/agent-page", title: "Agent page", can_back: false,
    can_forward: false, loading: true, error: null, visible: true
  }));
  ok("agent navigation opens the panel", !panel.classList.contains("collapsed"));

  // --- B-37: incremental transcript ---
  var tp = payload({ url: "", title: "", can_back: false, can_forward: false, loading: false, error: null, visible: false });
  tp.entries = [{ k: "u", t: "› hi" }, { k: "t", t: "✻ think" }, { k: "a", t: "Hel" }];
  window.render(tp);
  var logEl = document.getElementById("log");
  var firstRow = logEl.children[0];
  logEl.querySelector("details").open = true;
  var tp2 = Object.assign({}, tp, { entries: [{ k: "a", t: "Hello **there**" }], entries_from: 2, entries_len: 3 });
  window.render(tp2);
  ok("a tail update keeps earlier rows", logEl.children.length === 3 && logEl.children[0] === firstRow);
  ok("a tail update renders the new text", /Hello/.test(logEl.children[2].textContent) &&
    !!logEl.children[2].querySelector("strong"));
  ok("an open reasoning box stays open", logEl.querySelector("details").open === true);
  sent = [];
  window.render(Object.assign({}, tp, { entries: [{ k: "a", t: "x" }], entries_from: 7, entries_len: 8 }));
  ok("a gap asks Rust for a full resend", count("resync") === 1, JSON.stringify(sent));
  window.render(Object.assign({}, tp, { entries: [] }));
  ok("an empty list clears the transcript", logEl.children.length === 0, logEl.children.length);

  // --- D1: approval modal ---
  function withApproval(ap) {
    return Object.assign(
      payload({ url: "", title: "", can_back: false, can_forward: false, loading: false, error: null, visible: false }),
      { approval: ap }
    );
  }
  window.render(withApproval({ id: "apr_9", tool: "bash", detail: "rm -rf build", secs_left: 287 }));
  var apEl = document.getElementById("approval");
  ok("approval modal shows", !apEl.hidden);
  ok("modal names the tool", document.getElementById("ap-tool").textContent === "bash",
    document.getElementById("ap-tool").textContent);
  ok("modal shows the exact command", document.getElementById("ap-detail").textContent === "rm -rf build");
  ok("modal shows the countdown", document.getElementById("ap-count").textContent === "287s",
    document.getElementById("ap-count").textContent);

  sent = [];
  click("ap-allow");
  ok("allow once posts approve", !!last("approve") && last("approve").id === "apr_9" &&
    last("approve").always === false, JSON.stringify(last("approve")));
  sent = [];
  click("ap-allow");
  ok("answering twice posts nothing", count("approve") === 0);

  window.render(withApproval({ id: "apr_10", tool: "write", detail: "src/main.rs", secs_left: 300 }));
  ok("modal takes keyboard focus", !!document.activeElement && document.activeElement.id === "ap-card",
    document.activeElement && document.activeElement.id);
  sent = [];
  document.dispatchEvent(new KeyboardEvent("keydown", { key: "a", bubbles: true, cancelable: true }));
  ok("plain 'a' does not always-allow", count("approve") === 0, JSON.stringify(sent));
  document.dispatchEvent(new KeyboardEvent("keydown", { key: "A", shiftKey: true, bubbles: true, cancelable: true }));
  ok("Shift+A always-allows", !!last("approve") && last("approve").always === true &&
    last("approve").id === "apr_10", JSON.stringify(last("approve")));

  window.render(withApproval({ id: "apr_10b", tool: "bash", detail: "make", secs_left: 300 }));
  sent = [];
  click("ap-always");
  ok("first click on Always only arms it", count("approve") === 0);
  click("ap-always");
  ok("second click always-allows", !!last("approve") && last("approve").always === true &&
    last("approve").id === "apr_10b", JSON.stringify(last("approve")));

  window.render(withApproval({ id: "apr_10c", tool: "bash", detail: "ls", secs_left: 0,
    other_chat: true, chat: "Docs chat", queued: 2 }));
  ok("expired countdown says so", document.getElementById("ap-count").textContent === "expired",
    document.getElementById("ap-count").textContent);
  ok("modal names the other chat", !document.getElementById("ap-chat").hidden &&
    /Docs chat/.test(document.getElementById("ap-chat").textContent));
  ok("modal counts waiting requests", /2 more/.test(document.getElementById("ap-queue").textContent));

  window.render(withApproval({ id: "apr_11", tool: "bash", detail: "rm -rf /tmp/x", secs_left: 90 }));
  sent = [];
  document.dispatchEvent(new KeyboardEvent("keydown", { key: "n", bubbles: true, cancelable: true }));
  ok("'n' denies", !!last("deny") && last("deny").id === "apr_11", JSON.stringify(last("deny")));

  // Typing in the composer must never answer a permission prompt.
  window.render(withApproval({ id: "apr_12", tool: "bash", detail: "ls", secs_left: 300 }));
  sent = [];
  var composer = document.getElementById("input");
  composer.focus();
  composer.dispatchEvent(new KeyboardEvent("keydown", { key: "y", bubbles: true, cancelable: true }));
  ok("typing 'y' in the composer does not approve", count("approve") === 0);

  window.render(payload({ url: "", title: "", can_back: false, can_forward: false, loading: false, error: null, visible: false }));
  ok("modal hides when the run settles", document.getElementById("approval").hidden);

  // --- S-18: CSP, no network fonts, Copy without inline onclick ---
  ok("the page declares a CSP", !!document.querySelector('meta[http-equiv="Content-Security-Policy"]'));
  ok("no stylesheet is loaded from the network", document.querySelectorAll('link[rel="stylesheet"]').length === 0);
  var probe = document.createElement("div");
  probe.innerHTML = '<button id="csp-probe" onclick="window.__cspProbe = 1">x</button>';
  document.body.appendChild(probe);
  document.getElementById("csp-probe").click();
  ok("the CSP refuses inline event handlers", window.__cspProbe === undefined);
  probe.remove();
  var cp = payload({ url: "", title: "", can_back: false, can_forward: false, loading: false, error: null, visible: false });
  cp.entries = [{ k: "a", t: "```js\nlet x = 1;\n```" }];
  window.render(cp);
  var copyBtn = document.querySelector(".copy-btn");
  ok("code blocks get a Copy button without inline onclick",
    !!copyBtn && !copyBtn.hasAttribute("onclick") && !!copyBtn.getAttribute("data-copy"));

  // --- Transcript look: Markdown paragraphs and tool steps ---
  function basePage0() {
    return { url: "", title: "", can_back: false, can_forward: false, loading: false, error: null, visible: false };
  }
  var md = payload(basePage0());
  md.entries = [{ k: "a", t: "one\ntwo\n\nthree\n\n1. a\n2. b" }];
  window.render(md);
  var mdBox = document.querySelector("#log .e-bubble-a");
  ok("blank lines end a paragraph instead of adding <br> rows",
    mdBox.querySelectorAll("p").length === 2 && mdBox.innerHTML.indexOf("<br><br>") < 0, mdBox.innerHTML);
  ok("consecutive lines stay one paragraph", /one<br>two/.test(mdBox.querySelector("p").innerHTML));
  ok("numbered lists render as <ol>", mdBox.querySelectorAll("ol li").length === 2);

  var steps = payload(basePage0());
  steps.busy = true;
  steps.entries = [
    { k: "w", t: "⏺ read(src/a.rs)" }, { k: "w", t: "  ✓ read" },
    { k: "w", t: "⏺ bash(cargo test)" }, { k: "w", t: "  ✗ bash" },
    { k: "w", t: "⏺ grep(todo)" }
  ];
  window.render(steps);
  var cards = document.querySelectorAll("#log .tool-card");
  ok("a call and its result are one step", cards.length === 3, cards.length);
  ok("a finished step shows ok", cards[0].classList.contains("ok"));
  ok("a failed step shows fail", cards[1].classList.contains("fail"));
  ok("an open step spins while the run is busy", cards[2].classList.contains("run"));
  ok("live result rows are hidden", document.querySelectorAll("#log .tool-res.has-out").length === 0);
  ok("neighbouring steps stack", document.querySelectorAll("#log .chain-up").length === 2);
  steps.busy = false;
  window.render(steps);
  ok("an open step stops spinning when the run ends",
    document.querySelectorAll("#log .tool-card")[2].classList.contains("idle"));
  var withUsage = payload(basePage0());
  withUsage.usage = { requests: 4, input: 1200000, cached: 900000, cache_write: 0, output: 3400 };
  window.render(withUsage);
  var usageEl = document.getElementById("usage-line");
  ok("the sidebar shows the chat's cache share",
    !usageEl.hidden && /Cache 75%/.test(usageEl.textContent) && /1\.2M in/.test(usageEl.textContent),
    usageEl.textContent);
  window.render(payload(basePage0()));
  ok("no usage line before the first reply", usageEl.hidden);
  var replay = payload(basePage0());
  replay.entries = [{ k: "w", t: "⏺ bash(ls)" }, { k: "w", t: "  ✓ Cargo.toml\nsrc" }];
  window.render(replay);
  ok("a replayed result shows its output under the step",
    !!document.querySelector("#log .tool-res.has-out") &&
    document.querySelector("#log .tool-card").classList.contains("ok"));

  // --- Projects sidebar ---
  try { localStorage.removeItem("habi.sidebar"); } catch (e) { /* ignore */ }
  function chatOf(id, project, extra) {
    var c = { id: id, title: "Chat " + id, project: project, updated: 1000 - Number(id.slice(1)),
      pinned: false, archived: false, empty: false, running: false, needs: false, active: false };
    for (var k in (extra || {})) c[k] = extra[k];
    return c;
  }
  var sb = payload(basePage0());
  sb.projects = [{ id: "pa", name: "alpha", root: "C:/alpha" }, { id: "pb", name: "beta", root: "C:/beta" }];
  sb.current_project = "pa";
  sb.sessions = [
    chatOf("c1", "pa", { active: true }), chatOf("c2", "pa"), chatOf("c3", "pa"), chatOf("c4", "pa"),
    chatOf("c5", "pa"), chatOf("c6", "pa"), chatOf("c7", "pa"),
    chatOf("c8", "pb", { pinned: true }), chatOf("c9", "pb", { running: true }),
    chatOf("c10", "pb", { archived: true }), chatOf("c11", "pb", { empty: true }),
    chatOf("c12", null)
  ];
  window.render(sb);
  var side = document.getElementById("sesslist");
  var secs = Array.prototype.map.call(side.querySelectorAll(".side-sec"), function (e) { return e.textContent; });
  ok("sidebar sections in order", secs.join("|") === "Pinned|Active|Projects|Chats without a project", secs.join("|"));
  function rowIds() {
    return Array.prototype.map.call(side.querySelectorAll(".sess"), function (e) { return e.dataset.id; });
  }
  var ids = rowIds();
  ok("archived and never-used chats are hidden", ids.indexOf("c10") < 0 && ids.indexOf("c11") < 0, ids.join(","));
  ok("a pinned chat is listed once, under Pinned", ids.filter(function (i) { return i === "c8"; }).length === 1);
  ok("a project shows its newest 5 chats", ids.indexOf("c6") < 0 && ids.indexOf("c5") >= 0, ids.join(","));
  ok("the running chat shows a spinner", !!side.querySelector('.sess[data-id="c9"] .st.run'));
  var moreBtn = side.querySelector(".show-more");
  ok("Show more names how many are hidden", !!moreBtn && /Show 2 more/.test(moreBtn.textContent));
  moreBtn.click();
  ok("Show more lists the rest", rowIds().indexOf("c7") >= 0);
  sent = [];
  side.querySelector('.proj-head [data-act="new"]').click();
  ok("+ on a project starts a chat there", (last("new_in_project") || {}).project === "pa", JSON.stringify(sent));
  side.querySelector(".proj-head").click();
  ok("a project header collapses its chats", side.querySelector(".proj-body").classList.contains("collapsed"));
  side.querySelector(".proj-head").click();
  sent = [];
  side.querySelector('.sess[data-id="c2"]').click();
  ok("clicking a chat opens it", (last("load") || {}).id === "c2");
  side.querySelector('.sess[data-id="c2"] [data-act="pin"]').click();
  ok("the pin button pins", (last("pin") || {}).on === true && last("pin").id === "c2");
  var search = document.getElementById("side-search");
  search.value = "c12";
  search.dispatchEvent(new Event("input", { bubbles: true }));
  ok("search filters every chat", rowIds().join(",") === "c12", rowIds().join(","));
  search.value = "";
  search.dispatchEvent(new Event("input", { bubbles: true }));
  sent = [];
  document.getElementById("history-btn").click();
  var hist = document.getElementById("history");
  ok("History opens and hides the native preview", !hist.hidden && count("panel_visible") <= 1);
  ok("History lists archived chats too",
    /Chat c10/.test(document.getElementById("hist-list").textContent) &&
    !/Chat c11/.test(document.getElementById("hist-list").textContent));
  document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  ok("Escape closes History", hist.hidden);

  // --- Permission mode selector: default / manual / turbo ---
  var basePage = { url: "", title: "", can_back: false, can_forward: false, loading: false, error: null, visible: false };
  function withMode(mode) {
    var p = payload(basePage);
    p.perm_mode = mode;
    return p;
  }
  function modeEl(mode) { return document.querySelector('.mode-btn[data-mode="' + mode + '"]'); }
  ok("composer offers default, manual and turbo",
    ["default", "manual", "turbo"].every(function (m) { return !!modeEl(m); }));
  ok("composer keeps the send button", !!document.getElementById("send"));

  window.render(withMode("default"));
  ok("default mode is the selected one",
    modeEl("default").classList.contains("on") && !modeEl("turbo").classList.contains("on"));
  ok("composer is not flagged turbo in default mode",
    !document.getElementById("comp").classList.contains("turbo"));

  window.render(withMode("turbo"));
  ok("turbo is selected from the payload",
    modeEl("turbo").classList.contains("on") && modeEl("turbo").getAttribute("aria-checked") === "true");
  ok("default is deselected in turbo", !modeEl("default").classList.contains("on"));
  ok("composer says turbo is prompt-free",
    /Turbo/.test(document.getElementById("mode-hint").textContent),
    document.getElementById("mode-hint").textContent);
  ok("composer is flagged turbo", document.getElementById("comp").classList.contains("turbo"));

  // Layout: three pills + hint + send must still fit one row at 1280px wide.
  var compBox = document.getElementById("comp").getBoundingClientRect();
  var turboBox = modeEl("turbo").getBoundingClientRect();
  var sendBox = document.getElementById("send").getBoundingClientRect();
  ok("mode pills have real size and stay inside the composer",
    turboBox.width > 0 && turboBox.left >= compBox.left - 0.5 && turboBox.right <= compBox.right + 0.5,
    turboBox.left + "-" + turboBox.right + " in " + compBox.left + "-" + compBox.right);
  ok("send button still sits on the row",
    sendBox.width > 0 && sendBox.right <= compBox.right + 0.5);
  ok("the composer row does not overflow",
    document.getElementById("comp").scrollWidth <= document.getElementById("comp").clientWidth + 1,
    document.getElementById("comp").scrollWidth + ">" + document.getElementById("comp").clientWidth);

  sent = [];
  modeEl("manual").dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
  ok("picking a mode posts it to Rust",
    !!last("mode") && last("mode").mode === "manual", JSON.stringify(last("mode")));
  window.render(withMode("manual"));
  ok("manual mode is selected", modeEl("manual").classList.contains("on"));
  ok("manual hint mentions asking",
    /Manual/.test(document.getElementById("mode-hint").textContent),
    document.getElementById("mode-hint").textContent);
  ok("manual clears the turbo styling",
    !document.getElementById("comp").classList.contains("turbo"));

  // --- F-06: multiline composer, IME guard, Stop button ---
  var ta = document.getElementById("input");
  window.render(payload(basePage));
  ok("composer is a textarea", ta.tagName === "TEXTAREA", ta.tagName);
  ta.focus();
  ta.value = "line one";
  sent = [];
  ta.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", shiftKey: true, bubbles: true, cancelable: true }));
  ok("Shift+Enter does not send", count("send") === 0);
  ta.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", isComposing: true, bubbles: true, cancelable: true }));
  ok("Enter while an IME composes does not send", count("send") === 0);
  ta.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
  ok("Enter sends", count("send") === 1 && last("send").text === "line one", JSON.stringify(last("send")));

  var busyPage = payload(basePage);
  busyPage.busy = true;
  window.render(busyPage);
  ok("send turns into Stop while busy", document.getElementById("send").classList.contains("stop"));
  sent = [];
  click("send");
  ok("Stop posts stop", count("stop") === 1, JSON.stringify(sent));
  ta.value = "follow-up";
  sent = [];
  ta.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
  ok("Enter while busy does not replace the run", count("send") === 0);
  ta.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", ctrlKey: true, bubbles: true, cancelable: true }));
  ok("Ctrl+Enter while busy sends", count("send") === 1);
  sent = [];
  ta.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
  ok("Escape while busy posts stop", count("stop") === 1);
  window.render(payload(basePage));
  ok("Stop turns back into send", !document.getElementById("send").classList.contains("stop"));

  // --- Queue while busy, prompt history, open in editor ---
  var qta = document.getElementById("input");
  var busyPage = payload(basePage);
  busyPage.busy = true;
  window.render(busyPage);
  sent = [];
  qta.value = "next step please";
  key(qta, "Enter");
  ok("Enter while busy queues instead of sending", count("send") === 0 && !document.getElementById("queue-chip").hidden &&
    document.getElementById("queue-text").textContent === "next step please");
  window.render(busyPage);
  ok("the queue waits while the run goes on", count("send") === 0);
  window.render(payload(basePage));
  ok("the queued message is sent when the run ends", (last("send") || {}).text === "next step please" &&
    document.getElementById("queue-chip").hidden, JSON.stringify(sent));
  window.render(busyPage);
  qta.value = "changed my mind";
  key(qta, "Enter");
  click("queue-cancel");
  ok("removing the queued message puts its text back", qta.value === "changed my mind" && document.getElementById("queue-chip").hidden);
  window.render(payload(basePage));
  ok("a removed message is not sent", count("send") === 1, JSON.stringify(sent));
  qta.value = "";
  key(qta, "ArrowUp");
  ok("Up in an empty composer recalls the last prompt", qta.value === "next step please", qta.value);
  key(qta, "ArrowDown");
  ok("Down goes back to what was being typed", qta.value === "", qta.value);
  var edPage = payload(basePage);
  edPage.editors = ["VS Code", "Zed"];
  window.render(edPage);
  var edBtn = document.getElementById("open-editor-btn");
  ok("an installed editor shows its button", !edBtn.hidden && edBtn.textContent === "Open in VS Code");
  sent = [];
  click("open-editor-btn");
  ok("the button asks to open the project in that editor", (last("open_in_editor") || {}).editor === "VS Code", JSON.stringify(sent));
  window.render(payload(basePage));
  ok("no editor, no button", edBtn.hidden);

  // --- Settings page ---
  var setPage = payload(basePage);
  setPage.settings = { base_url: "", model: "", reasoning_effort: "", has_key: false, needs_setup: true,
    models: ["qwen/qwen3-coder:free", "deepseek/deepseek-chat"], config_path: "C:/x/config.toml" };
  window.render(setPage);
  var setBox = document.getElementById("settings");
  ok("no provider: Settings opens by itself", !setBox.hidden && !document.getElementById("set-setup").hidden);
  ok("listed models fill the model suggestions", document.querySelectorAll("#set-models option").length === 2);
  setBox.querySelector(".set-presets button").click();
  document.getElementById("set-model").value = "qwen/qwen3-coder:free";
  document.getElementById("set-key").value = "sk-or-123";
  document.getElementById("set-effort").value = "low";
  sent = [];
  document.getElementById("set-form").dispatchEvent(new Event("submit", { cancelable: true }));
  var saved = last("save_settings") || {};
  ok("Save sends the provider, model, key and effort", saved.base_url === "https://openrouter.ai/api/v1" &&
    saved.model === "qwen/qwen3-coder:free" && saved.api_key === "sk-or-123" && saved.reasoning_effort === "low", JSON.stringify(saved));
  ok("the key field is cleared and the page closes", document.getElementById("set-key").value === "" && setBox.hidden);
  setPage.settings.needs_setup = false;
  setPage.settings.has_key = true;
  window.render(setPage);
  ok("it opens by itself only once", setBox.hidden);
  click("settings-btn");
  sent = [];
  document.getElementById("set-form").dispatchEvent(new Event("submit", { cancelable: true }));
  ok("an empty key field keeps the saved key", !("api_key" in (last("save_settings") || {})), JSON.stringify(sent));

  // --- Theme, drafts, export ---
  click("settings-btn");
  document.getElementById("set-theme").value = "light";
  document.getElementById("set-form").dispatchEvent(new Event("submit", { cancelable: true }));
  ok("the light theme applies", document.documentElement.getAttribute("data-theme") === "light");
  click("settings-btn");
  document.getElementById("set-theme").value = "dark";
  document.getElementById("set-form").dispatchEvent(new Event("submit", { cancelable: true }));
  ok("dark removes it again", !document.documentElement.hasAttribute("data-theme"));
  var dta = document.getElementById("input");
  var chatA = payload(basePage); chatA.session = "chat-a";
  var chatB = payload(basePage); chatB.session = "chat-b";
  window.render(chatA);
  dta.value = "half-typed idea";
  dta.dispatchEvent(new Event("input"));
  window.render(chatB);
  ok("another chat starts with its own (empty) box", dta.value === "", dta.value);
  window.render(chatA);
  ok("coming back brings the draft back", dta.value === "half-typed idea", dta.value);
  dta.value = "";
  dta.dispatchEvent(new Event("input"));
  window.render(payload(basePage));
  sent = [];
  click("export-btn");
  ok("Export asks for the chat as Markdown", count("export") === 1);

  // --- ask_user: question with answer buttons ---
  var askPage = payload(basePage);
  askPage.question = { text: "Which color theme?", options: ["Warm brown", "Dark roast"] };
  window.render(askPage);
  var askBox = document.getElementById("ask-box");
  ok("a question shows with one button per answer", !askBox.hidden &&
    document.getElementById("ask-text").textContent === "Which color theme?" &&
    document.querySelectorAll("#ask-options .ask-opt").length === 2);
  sent = [];
  document.querySelectorAll("#ask-options .ask-opt")[1].click();
  ok("clicking an answer sends it", (last("send") || {}).text === "Dark roast", JSON.stringify(sent));
  window.render(payload(basePage));
  ok("no question, no box", askBox.hidden);

  // --- A4: agents panel ---
  try { localStorage.removeItem("habi.agents.collapsed"); } catch (e) { /* ignore */ }
  var agentsEl = document.getElementById("agents");
  ok("no agents panel without agents", agentsEl.hidden);
  var withAgents = payload(basePage);
  withAgents.agents = [
    { id: "ses_a", name: "explore", task: "map the auth code", state: "working", detail: "grep(login)", secs: 12 },
    { id: "ses_b", name: "worker", task: "fix the parser", state: "queued", detail: "waiting for a free place…", secs: 3 },
    { id: "ses_c", name: "review", task: "check the diff", state: "done", detail: "looks good", secs: 75 }
  ];
  window.render(withAgents);
  var agCards = document.querySelectorAll("#ag-list .ag-card");
  ok("one card per agent", !agentsEl.hidden && agCards.length === 3, agCards.length);
  ok("the head counts running and finished agents",
    document.getElementById("ag-count").textContent === "2 running · 1 finished",
    document.getElementById("ag-count").textContent);
  ok("a card shows role, task, current tool and time",
    agCards[0].querySelector(".ag-role").textContent === "explore" &&
    agCards[0].querySelector(".ag-task").textContent === "map the auth code" &&
    agCards[0].querySelector(".ag-detail").textContent === "grep(login)" &&
    agCards[2].querySelector(".ag-time").textContent === "1m 15s");
  ok("only running agents have Stop",
    !!agCards[0].querySelector(".ag-btn.stop") && !!agCards[1].querySelector(".ag-btn.stop") &&
    !agCards[2].querySelector(".ag-btn.stop"));
  sent = [];
  agCards[0].querySelector(".ag-btn.stop").click();
  ok("Stop posts agent_stop with the id", (last("agent_stop") || {}).id === "ses_a", JSON.stringify(sent));
  agCards[2].querySelector(".ag-btn").click();
  ok("Open loads the agent's transcript", (last("load") || {}).id === "ses_c", JSON.stringify(sent));
  click("ag-head");
  ok("the head collapses the list", document.getElementById("ag-box").classList.contains("collapsed"));
  window.render(withAgents);
  ok("collapsed stays collapsed across paints", document.getElementById("ag-box").classList.contains("collapsed"));
  click("ag-head");
  window.render(payload(basePage));
  ok("another chat without agents hides the panel", agentsEl.hidden);

  } catch (e) {
    ok("harness ran to completion", false, "threw: " + (e && e.message));
  }

  var out = document.createElement("pre");
  out.id = "testout";
  out.textContent = log.join("\n");
  document.body.appendChild(out);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", run);
  } else {
    run();
  }
})();
