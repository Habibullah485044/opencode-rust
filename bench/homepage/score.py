"""Score one run of the homepage benchmark (see PROMPT.md).

Usage: python -I score.py <folder the tool worked in>

60 points for structure and code (read from the files), 40 for behaviour in
a real headless Edge/Chrome at 360, 768 and 1440 px. Screenshots go to
bench/homepage/results/<folder name>/ for judging the design by eye; the
score does not rate looks. The solution folder is never written to.
"""

import glob
import html.parser
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
SECTIONS = ["hero", "features", "menu", "testimonials", "contact"]
FONT_HOSTS = ("fonts.googleapis.com", "fonts.gstatic.com")
WIDTHS = (360, 768, 1440)


# ---------------------------------------------------------------- browser

def find_browser():
    for env in ("EDGE", "CHROME"):
        if os.environ.get(env):
            return os.environ[env]
    for path in (r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
                 r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
                 r"C:\Program Files\Google\Chrome\Application\chrome.exe",
                 "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
                 "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"):
        if os.path.isfile(path):
            return path
    for name in ("msedge", "microsoft-edge", "google-chrome", "chromium", "chromium-browser", "chrome"):
        found = shutil.which(name)
        if found:
            return found
    return None


def browser(exe, args, profile):
    # Same settings as opencode-rust's page check: no outside network (fonts
    # fall back), background services off — seconds instead of a minute.
    cmd = [exe, "--headless=new", "--disable-gpu", "--no-sandbox", "--no-first-run",
           "--disable-extensions", "--disable-component-update", "--disable-background-networking",
           "--disable-sync", "--no-default-browser-check",
           "--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE localhost , EXCLUDE 127.0.0.1",
           "--hide-scrollbars", f"--user-data-dir={profile}", "--virtual-time-budget=4000", *args]
    try:
        return subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8",
                              errors="replace", timeout=90).stdout
    except subprocess.TimeoutExpired:
        return ""


# Runs first in <head>: collect script errors from the very start.
EARLY = """<script>window.__errs=[];addEventListener('error',function(e){var t=e.target;if(t&&t!==window&&(t.src||t.href))return;
__errs.push(String(e.message||e.type))},true);
addEventListener('unhandledrejection',function(e){__errs.push('rejection: '+e.reason)});</script>"""

# Runs after the page's own scripts: measure, use the menu and the form.
PROBE = r"""<script>(function(){
function vis(el){if(!el)return false;var s=getComputedStyle(el),r=el.getBoundingClientRect();
 return s.display!=='none'&&s.visibility!=='hidden'&&+s.opacity>0.05&&r.width>0&&r.height>0;}
function out(o){if(window.parent!==window){window.parent.postMessage({__probe:o},'*');return;}
 var p=document.createElement('script');p.type='application/json';p.id='__probe';
 p.textContent=JSON.stringify(o);document.body.appendChild(p);}
function run(){var o={w:innerWidth,overflow:document.documentElement.scrollWidth-innerWidth};
 var btn=document.querySelector('header [aria-expanded]');
 o.btn=!!btn;o.btnControls=!!(btn&&btn.getAttribute('aria-controls'));
 var links=document.querySelectorAll('header nav a');
 o.linksBefore=links.length?vis(links[0]):false;
 if(btn&&vis(btn)){btn.click();o.expOpen=btn.getAttribute('aria-expanded');
  o.linksOpen=links.length?vis(links[links.length-1]):false;
  btn.click();o.expClosed=btn.getAttribute('aria-expanded');}
 var f=document.querySelector('#contact form');o.form=!!f;
 if(f){var q=function(s){return f.querySelector(s)};
  var nm=q('input[name*="name" i]')||q('input[id*="name" i]')||q('input[type="text"]');
  var em=q('input[type="email"]')||q('input[name*="mail" i]');var ms=q('textarea');
  o.fields=!!(nm&&em&&ms);var prevented=null;
  f.addEventListener('submit',function(e){setTimeout(function(){prevented=e.defaultPrevented},0)});
  if(o.fields){[nm,em,ms].forEach(function(x){x.value=''});
   try{f.requestSubmit?f.requestSubmit():f.dispatchEvent(new Event('submit',{cancelable:true,bubbles:true}))}catch(e){o.submitErr=String(e)}
   setTimeout(function(){o.emptyPrevented=prevented;
    o.invalid=[nm,em,ms].filter(function(x){return x.getAttribute('aria-invalid')==='true'}).length;
    var msgs=0;[nm,em,ms].forEach(function(x){var d=x.getAttribute('aria-describedby');var m=d&&document.getElementById(d.split(' ')[0]);
     if(!m){m=x.nextElementSibling}if(m&&vis(m)&&m.textContent.trim())msgs++});o.messages=msgs;
    nm.value='Ada Lovelace';em.value='ada@example.com';ms.value='Hello, I would like to order beans.';
    [nm,em,ms].forEach(function(x){x.dispatchEvent(new Event('input',{bubbles:true}))});prevented=null;
    try{f.requestSubmit?f.requestSubmit():f.dispatchEvent(new Event('submit',{cancelable:true,bubbles:true}))}catch(e){o.submitErr=String(e)}
    setTimeout(function(){o.validPrevented=prevented;var st=document.getElementById('form-status');
     o.status=st&&vis(st)?st.textContent.trim().slice(0,120):'';
     o.invalidAfter=[nm,em,ms].filter(function(x){return x.getAttribute('aria-invalid')==='true'}).length;
     o.errs=window.__errs;out(o)},300)},300);return;}}
 o.errs=window.__errs;out(o);}
addEventListener('DOMContentLoaded',function(){setTimeout(run,800)});})();</script>"""


def probe(exe, site, width, profile):
    page = os.path.join(site, "index.html")
    text = open(page, encoding="utf-8", errors="replace").read()
    text = re.sub(r"(<head[^>]*>)", lambda m: m.group(1) + EARLY, text, count=1, flags=re.I)
    text = re.sub(r"(</body>)", lambda m: PROBE + m.group(1), text, count=1, flags=re.I) if re.search("</body>", text, re.I) else text + PROBE
    probed = os.path.join(site, "__probe.html")
    open(probed, "w", encoding="utf-8").write(text)
    # Headless Chromium will not make its window narrower than ~500px, so the
    # page runs in an iframe of exactly `width`; it posts its result up.
    wrap = os.path.join(site, f"__wrap{width}.html")
    open(wrap, "w", encoding="utf-8").write(
        "<!doctype html><html><body style='margin:0'><script>addEventListener('message',function(e){"
        "if(e.data&&e.data.__probe){var s=document.createElement('script');s.type='application/json';"
        "s.id='__probe';s.textContent=JSON.stringify(e.data.__probe);document.body.appendChild(s);}});</script>"
        f"<iframe src='__probe.html' width='{width}' height='900' style='border:0;display:block'></iframe></body></html>")
    dom = browser(exe, ["--window-size=1600,1000", "--dump-dom", url(wrap)], profile)
    m = re.search(r'<script type="application/json" id="__probe">(.*?)</script>', dom, re.S)
    try:
        return json.loads(m.group(1)) if m else None
    except ValueError:
        return None


def url(path):
    return "file:///" + os.path.abspath(path).replace("\\", "/").lstrip("/")


# ---------------------------------------------------------------- static

class Page(html.parser.HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.tags = []  # (tag, attrs dict, depth path)
        self.stack = []
        self.text_in = {}  # section id -> text
        self.labels_for = set()
        self.wrapped = set()  # ids of controls inside a <label>

    def handle_starttag(self, tag, attrs):
        a = {k: (v or "") for k, v in attrs}
        self.tags.append((tag, a, [t for t, _ in self.stack]))
        if tag == "label" and a.get("for"):
            self.labels_for.add(a["for"])
        if tag in ("input", "textarea", "select") and any(t == "label" for t, _ in self.stack):
            self.wrapped.add(a.get("id") or a.get("name") or tag)
        if tag not in ("meta", "link", "br", "img", "input", "hr", "source", "path", "circle", "rect", "line", "polyline", "polygon", "ellipse", "use", "stop"):
            self.stack.append((tag, a))

    def handle_endtag(self, tag):
        for i in range(len(self.stack) - 1, -1, -1):
            if self.stack[i][0] == tag:
                del self.stack[i:]
                break

    def handle_data(self, data):
        for t, a in self.stack:
            if t == "section" and a.get("id"):
                self.text_in[a["id"]] = self.text_in.get(a["id"], "") + data


def static_checks(sol):
    res = []

    def check(name, pts, ok, got=""):
        res.append((name, pts, bool(ok), got))

    files = {f: os.path.isfile(os.path.join(sol, f)) for f in ("index.html", "styles.css", "script.js")}
    check("index.html, styles.css and script.js exist", 6, all(files.values()), files)
    if not files["index.html"]:
        return res
    def read(f):
        path = os.path.join(sol, f)
        return open(path, encoding="utf-8", errors="replace").read() if os.path.isfile(path) else ""

    htm, css, js = read("index.html"), read("styles.css"), read("script.js")
    p = Page()
    p.feed(htm)
    tags = p.tags
    first = lambda t: next((a for tag, a, _ in tags if tag == t), None)
    metas = [a for t, a, _ in tags if t == "meta"]
    head = [
        (first("html") or {}).get("lang", "").strip() != "",
        any(m.get("name", "").lower() == "viewport" for m in metas),
        re.search(r"<title>\s*\S", htm, re.I) is not None,
        any(m.get("name", "").lower() == "description" and m.get("content", "").strip() for m in metas),
    ]
    check("html lang, viewport meta, title, meta description", 8, all(head), head)

    order = [a.get("id") for t, a, _ in tags if t == "section" and a.get("id") in SECTIONS]
    pos = {t: i for i, (t, _, _) in reversed(list(enumerate(tags)))}
    hdr = pos.get("header", 10**9) < min([i for i, (t, a, _) in enumerate(tags) if t == "section"] or [10**9])
    ftr = any(t == "footer" for t, _, _ in tags) and max(i for i, (t, _, _) in enumerate(tags) if t == "footer") > max([i for i, (t, a, _) in enumerate(tags) if t == "section"] or [-1])
    nav_in_header = any(t == "nav" and "header" in path for t, _, path in tags)
    check("header with nav, sections hero, features, menu, testimonials, contact, footer", 10,
          order == SECTIONS and hdr and ftr and nav_in_header, {"sections": order, "header_first": hdr, "footer_last": ftr, "nav_in_header": nav_in_header})

    hrefs = {a.get("href", "") for t, a, path in tags if t == "a" and "nav" in path}
    check("the nav links to every section", 4, all(f"#{s}" in hrefs for s in SECTIONS), sorted(hrefs))

    h1s = [path for t, _, path in tags if t == "h1"]
    hero = re.search(r'<section[^>]*id="hero"[\s\S]*?</section>', htm, re.I)
    hero_html = hero.group(0).lower() if hero else ""
    check("one <h1>, in #hero, with a call-to-action", 4,
          len(h1s) == 1 and "<h1" in hero_html and re.search(r"<(a|button)\b", hero_html) is not None,
          {"h1": len(h1s)})

    prices = re.findall(r"(?:[$€£]\s?\d+(?:[.,]\d{2})?|\d+[.,]\d{2}\s?(?:USD|EUR|\$|€|£))", p.text_in.get("menu", ""))
    check("#menu lists at least 4 prices", 4, len(prices) >= 4, len(prices))

    urls = re.findall(r"https?://[^\s\"')<>]+", htm + "\n" + css + "\n" + js)
    outside = sorted({u for u in urls if not any(h in u for h in FONT_HOSTS) and "www.w3.org" not in u})
    imgs = [a.get("src", "") for t, a, _ in tags if t == "img"]
    css_imgs = re.findall(r"url\(\s*['\"]?(?!data:)([^'\")]+\.(?:png|jpe?g|gif|webp|avif|svg))", css, re.I)
    img_files = [f for f in os.listdir(sol) if re.search(r"\.(png|jpe?g|gif|webp|avif)$", f, re.I)]
    no_net = not outside and not [s for s in imgs if s and not s.startswith("data:")] and not css_imgs and not img_files
    check("no network requests except Google Fonts, no image files or URLs", 6, no_net,
          {"urls": outside[:5], "img_src": imgs[:3], "css": css_imgs[:3], "files": img_files[:3]})

    unnamed_img = [a for t, a, _ in tags if t == "img" and not a.get("alt", "").strip() and a.get("aria-hidden") != "true"]
    svg_blocks = re.findall(r"<svg\b[^>]*>[\s\S]*?</svg>", htm, re.I)
    unnamed_svg = [b[:60] for b in svg_blocks if not re.search(r'aria-label=|aria-labelledby=|aria-hidden="true"|role="presentation"|<title>', b, re.I)]
    check("every svg and img has an accessible name (or is aria-hidden)", 4,
          not unnamed_svg and not unnamed_img, {"svg": unnamed_svg[:2], "img": len(unnamed_img)})

    root_vars = re.search(r":root\s*\{([^}]*)\}", css)
    check("colors in CSS custom properties on :root", 3, bool(root_vars) and len(re.findall(r"--[\w-]+\s*:", root_vars.group(1))) >= 3)
    check("dark mode via prefers-color-scheme", 3, re.search(r"prefers-color-scheme\s*:\s*dark", css, re.I) is not None)
    check("a media query at the 768px breakpoint", 2, re.search(r"@media[^{]*(?:767|768)", css) is not None)

    form = re.search(r"<form\b[^>]*>", htm, re.I)
    controls = [a for t, a, _ in tags if t in ("input", "textarea") and a.get("type", "text") not in ("submit", "button", "hidden")]
    labelled = [c for c in controls if c.get("id") in p.labels_for or (c.get("id") or c.get("name")) in p.wrapped]
    check("form uses novalidate; name, email and message each have a <label>", 6,
          bool(form) and "novalidate" in form.group(0).lower() and len(controls) >= 3 and len(labelled) >= 3,
          {"controls": len(controls), "labelled": len(labelled)})
    return res


def dynamic_checks(sol, exe):
    res = []

    def check(name, pts, ok, got=""):
        res.append((name, pts, bool(ok), got))

    work = tempfile.mkdtemp(prefix="home-")
    site = os.path.join(work, "site")
    shutil.copytree(sol, site, ignore=shutil.ignore_patterns(".git", "node_modules", "_score"))
    profile = os.path.join(work, "profile")
    runs = {w: probe(exe, site, w, profile) for w in WIDTHS}
    if not all(runs.values()):
        check("the page loads and the probe runs at every width", 40, False, {w: bool(r) for w, r in runs.items()})
        return res, runs
    errs = sorted({e for r in runs.values() for e in r.get("errs", [])})
    check("no script errors", 6, not errs, errs[:3])
    check("no horizontal scrolling at 360px", 6, runs[360]["overflow"] <= 1, runs[360]["overflow"])
    check("no horizontal scrolling at 768px", 3, runs[768]["overflow"] <= 1, runs[768]["overflow"])
    check("no horizontal scrolling at 1440px", 3, runs[1440]["overflow"] <= 1, runs[1440]["overflow"])
    m = runs[360]
    check("mobile menu button has aria-controls and aria-expanded", 3, m.get("btn") and m.get("btnControls"))
    check("at 360px the menu opens: aria-expanded=true, links visible", 5,
          m.get("expOpen") == "true" and m.get("linksOpen") and not m.get("linksBefore"),
          {k: m.get(k) for k in ("linksBefore", "expOpen", "linksOpen")})
    check("the menu closes again: aria-expanded=false", 2, m.get("expClosed") == "false", m.get("expClosed"))
    d = runs[1440]
    check("an empty submit is stopped, marks 3 fields aria-invalid, shows messages", 6,
          d.get("emptyPrevented") is not False and d.get("invalid", 0) >= 3 and d.get("messages", 0) >= 3,
          {k: d.get(k) for k in ("fields", "emptyPrevented", "invalid", "messages")})
    check("a valid submit shows the thanks in #form-status and clears errors", 6,
          d.get("validPrevented") is not False and bool(d.get("status")) and d.get("invalidAfter", 1) == 0,
          {k: d.get(k) for k in ("validPrevented", "status", "invalidAfter")})
    return res, runs


def screenshots(sol, exe):
    out_dir = os.path.join(HERE, "results", os.path.basename(os.path.normpath(sol)))
    os.makedirs(out_dir, exist_ok=True)
    profile = tempfile.mkdtemp(prefix="home-shot-")
    for w in WIDTHS:
        shot = os.path.join(out_dir, f"{w}px.png")
        browser(exe, [f"--window-size={w},3000", f"--screenshot={shot}", url(os.path.join(sol, "index.html"))], profile)
    return out_dir


def main():
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(errors="replace")
    if len(sys.argv) != 2 or not os.path.isdir(sys.argv[1]):
        sys.exit("usage: python -I score.py <solution folder>")
    sol = os.path.abspath(sys.argv[1])
    exe = find_browser()
    results = static_checks(sol)
    runs = {}
    if exe and os.path.isfile(os.path.join(sol, "index.html")):
        dyn, runs = dynamic_checks(sol, exe)
        results += dyn
    elif not exe:
        print("note: no Edge/Chrome found (set EDGE=); browser checks skipped")
    score = 0
    for name, pts, ok, got in results:
        score += pts if ok else 0
        line = f"{'PASS' if ok else 'FAIL'} {pts:>2}  {name}"
        if not ok and got != "":
            line += f"   got: {ascii(got)[:180]}"
        print(line)
    size = sum(os.path.getsize(f) for f in glob.glob(os.path.join(sol, "*.*")) if os.path.isfile(f))
    print(f"\nSCORE {score}/100   (code size {size // 1024} KB)")
    if exe and os.path.isfile(os.path.join(sol, "index.html")):
        print(f"screenshots: {screenshots(sol, exe)}")


if __name__ == "__main__":
    main()
