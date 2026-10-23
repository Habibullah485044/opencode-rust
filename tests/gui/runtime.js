// Appended after the real RUNTIME_JS on a fixture page; writes PASS/FAIL into
// #testout so `--dump-dom` can be grepped.
(function () {
  var frames = [];
  window.ipc = { postMessage: function (s) { frames.push(JSON.parse(s)); } };

  var log = [];
  function ok(n, c, x) {
    log.push((c ? "PASS " : "FAIL ") + n + (x !== undefined ? " [" + x + "]" : ""));
  }

  try {
    var snap = window.__habi.snapshot();
    ok("snapshot finds interactive nodes", snap.nodes.length >= 3, snap.nodes.length);
    ok("snapshot reports url + title", typeof snap.url === "string" && typeof snap.title === "string");
    ok("snapshot skips hidden elements", !snap.nodes.some(function (n) { return n.name === "Nope"; }));
    ok("snapshot includes headings", snap.headings.indexOf("Fixture Title") >= 0, snap.headings.join("|"));
    ok("snapshot caps the text extract", snap.text.length <= 1000, snap.text.length);

    var save = snap.nodes.filter(function (n) { return n.name.indexOf("Save") >= 0; })[0];
    ok("button gets role + ref", !!save && save.role === "button" && /^e\d+$/.test(save.ref), JSON.stringify(save));

    var input = snap.nodes.filter(function (n) { return n.role === "text"; })[0];
    ok("text input is discovered", !!input, JSON.stringify(input));

    var link = snap.nodes.filter(function (n) { return n.role === "link"; })[0];
    ok("link carries its href", !!link && /example\.com/.test(link.href), link && link.href);

    var clicked = false;
    document.getElementById("b1").addEventListener("click", function () { clicked = true; });
    var r = window.__habi.click(save.ref);
    ok("click fires the real handler", clicked);
    ok("click echoes the ref", r.clicked === save.ref, JSON.stringify(r));

    window.__habi.type(input.ref, "me@example.com", false);
    ok("type sets the value", document.getElementById("i1").value === "me@example.com",
      document.getElementById("i1").value);

    var submitted = false;
    document.getElementById("i1").form.addEventListener("submit", function (e) {
      e.preventDefault();
      submitted = true;
    });
    window.__habi.type(input.ref, "again", true);
    ok("submit flag submits the form", submitted);

    try {
      window.__habi.click("e999");
      ok("stale ref fails loudly", false);
    } catch (e) {
      ok("stale ref fails loudly", /fresh snapshot/.test(e.message), e.message);
    }

    // waitFor must post exactly one frame, keyed by the request id.
    var realTimeout = window.setTimeout;
    window.setTimeout = function (fn) { fn(); return 0; };
    window.__habi.waitFor(42, { selector: "#b1", timeout_ms: 500 });
    window.setTimeout = realTimeout;
    var frame = frames.filter(function (f) { return f.hab === 42; })[0];
    ok("waitFor posts a success frame", !!frame && frame.ok === true, JSON.stringify(frame));

    // A condition that never appears must report the timeout, not hang.
    // The fake timer advances the clock so the poll gives up immediately.
    window.setTimeout = function (fn) {
      var t = Date.now();
      while (Date.now() - t < 3) { /* burn 3ms */ }
      fn();
      return 0;
    };
    window.__habi.waitFor(43, { text: "never-on-this-page", timeout_ms: 1 });
    window.setTimeout = realTimeout;
    var miss = frames.filter(function (f) { return f.hab === 43; })[0];
    ok("waitFor reports a timeout", !!miss && miss.ok === false && /timed out/.test(miss.error),
      JSON.stringify(miss));

    ok("no frame leaks the raw document", JSON.stringify(snap).length < 8000,
      JSON.stringify(snap).length);

    // press / scroll / console / evaluate
    var keys = [];
    document.getElementById("i1").addEventListener("keydown", function (e) { keys.push(e.key); });
    var snap2 = window.__habi.snapshot();
    var field = snap2.nodes.filter(function (n) { return n.role === "text"; })[0];
    var subs = 0;
    var formEl = document.getElementById("i1").form;
    formEl.addEventListener("submit", function (e) { e.preventDefault(); subs++; });
    var pr = window.__habi.press("Enter", field.ref);
    ok("press sends the key to the element", keys.indexOf("Enter") >= 0 && pr.pressed === "Enter", JSON.stringify(pr));
    ok("Enter in a form field submits it", subs === 1, subs);
    var sc = window.__habi.scroll("bottom");
    ok("scroll reports the position", typeof sc.scrollY === "number" && sc.pageHeight > 0, JSON.stringify(sc));
    console.warn("careful", { n: 1 });
    ok("console output is kept", /warn: careful \{"n":1\}/.test(window.__habi.logs()), window.__habi.logs());
    ok("evaluate returns JSON", window.__habi.evaluate("({a: 1 + 1})") === '{"a":2}');
  } catch (e) {
    ok("harness ran to completion", false, "threw: " + (e && e.message));
  }

  var out = document.createElement("pre");
  out.id = "testout";
  out.textContent = log.join("\n");
  document.body.appendChild(out);
})();
