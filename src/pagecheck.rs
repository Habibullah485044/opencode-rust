//! Page check: open a web page of the workspace in a real headless browser
//! (Edge or Chrome) at phone and desktop width and report what is broken —
//! sideways scrolling and what causes it, script errors, files that fail to
//! load, a nav that shows bullets or stacks, a mobile menu that does not
//! open, icons that draw nothing or a solid block, text that should be on the
//! page but is not. Runs as the `check_page` tool and after every run that
//! changed a web page (see `server::check_after_run`).
//!
//! The page is served by a small local web server (`127.0.0.1`, random port,
//! unguessable path per folder), so relative links work as on a real site
//! and the desktop app's preview panel, which only opens http pages, can
//! show it. One browser process loads a harness page with one iframe per
//! width; a probe script injected into each frame measures and reports back.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Widths a page is checked at when the caller names none.
pub const DEFAULT_WIDTHS: &[u32] = &[375, 1440];

/// One width's findings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WidthResult {
    pub width: u32,
    pub problems: Vec<String>,
    pub ok: Vec<String>,
}

/// A whole check of one page.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub page: String,
    /// Address the page is served at (without the probe).
    pub url: String,
    pub widths: Vec<WidthResult>,
    /// Problems not tied to one width (expected text missing).
    pub general: Vec<String>,
}

impl Report {
    /// Number of problems: lower is better, 0 = clean.
    pub fn score(&self) -> usize {
        self.general.len() + self.widths.iter().map(|w| w.problems.len()).sum::<usize>()
    }

    /// The text the model reads: numbered problems first, then what passed.
    pub fn render(&self) -> String {
        let widths: Vec<String> = self.widths.iter().map(|w| format!("{}px", w.width)).collect();
        let mut out = String::new();
        let n = self.score();
        if n == 0 {
            out.push_str(&format!("check_page {}: no problems found at {}.\n", self.page, widths.join(" and ")));
        } else {
            out.push_str(&format!(
                "check_page {}: {n} problem{} at {}:\n",
                self.page,
                if n == 1 { "" } else { "s" },
                widths.join(" and ")
            ));
            let mut i = 0;
            for p in &self.general {
                i += 1;
                out.push_str(&format!("{i}. {p}\n"));
            }
            for w in &self.widths {
                for p in &w.problems {
                    i += 1;
                    out.push_str(&format!("{i}. [{}px] {p}\n", w.width));
                }
            }
        }
        let mut passed: Vec<&str> = Vec::new();
        for w in &self.widths {
            for o in &w.ok {
                if !passed.contains(&o.as_str()) {
                    passed.push(o);
                }
            }
        }
        if !passed.is_empty() {
            out.push_str(&format!("Passed: {}.\n", passed.join(", ")));
        }
        out.push_str(&format!("Page address (for the browser tools): {}", self.url));
        out
    }
}

// ------------------------------------------------------------------ browser

/// Edge or Chrome on this machine. `OPENCODE_RUST_BROWSER` picks one.
pub fn find_browser() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("OPENCODE_RUST_BROWSER") {
        let p = PathBuf::from(p.trim());
        if p.is_file() {
            return Some(p);
        }
    }
    let mut tries: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
        if let Ok(dir) = std::env::var(var) {
            tries.push(Path::new(&dir).join(r"Microsoft\Edge\Application\msedge.exe"));
            tries.push(Path::new(&dir).join(r"Google\Chrome\Application\chrome.exe"));
        }
    }
    if let Ok(dir) = std::env::var("LOCALAPPDATA") {
        tries.push(Path::new(&dir).join(r"Google\Chrome\Application\chrome.exe"));
    }
    tries.push("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into());
    tries.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
    if let Some(found) = tries.into_iter().find(|p| p.is_file()) {
        return Some(found);
    }
    let names = ["msedge", "microsoft-edge", "google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "chrome"];
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        for name in names {
            for file in [name.to_string(), format!("{name}.exe")] {
                let p = dir.join(&file);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// Run the browser headless on `url` and return the final DOM.
///
/// Measured on an old laptop: a fresh profile adds ~10s and Edge's background
/// services ~20s, so one kept profile is reused (checks run one at a time)
/// and the extras are switched off. Outside hosts do not resolve: the check
/// is fast, private and the same every time (web fonts fall back).
async fn dump_dom(exe: &Path, url: &str, timeout: Duration) -> Result<String> {
    static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _turn = ONE_AT_A_TIME.lock().await;
    let profile = std::env::temp_dir().join("opencode-rust-pagecheck-profile");
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args([
        "--headless=new",
        "--disable-gpu",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-extensions",
        "--disable-component-update",
        "--disable-background-networking",
        "--disable-sync",
        "--disable-features=msEdgeSidebarV2,EdgeCollections,msShopping",
        "--hide-scrollbars",
        "--mute-audio",
        "--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE localhost , EXCLUDE 127.0.0.1",
        "--virtual-time-budget=4000",
        "--window-size=1600,1000",
    ])
    .arg(format!("--user-data-dir={}", profile.display()))
    .arg("--dump-dom")
    .arg(url)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .kill_on_drop(true);
    let child = cmd.spawn().with_context(|| format!("starting {}", exe.display()))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output()).await;
    let out = out.map_err(|_| anyhow!("the browser did not finish within {}s", timeout.as_secs()))??;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ------------------------------------------------------------ local server

struct Preview {
    addr: std::net::SocketAddr,
    roots: Mutex<HashMap<String, PathBuf>>,
}

static PREVIEW: tokio::sync::OnceCell<Arc<Preview>> = tokio::sync::OnceCell::const_new();

async fn preview() -> Result<Arc<Preview>> {
    PREVIEW
        .get_or_try_init(|| async {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
            let addr = listener.local_addr()?;
            let p = Arc::new(Preview { addr, roots: Mutex::new(HashMap::new()) });
            let app = axum::Router::new()
                .route("/p/{token}/{*path}", axum::routing::get(serve_file))
                .route("/c/{token}/{*path}", axum::routing::get(serve_harness))
                .with_state(p.clone());
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Ok::<_, anyhow::Error>(p)
        })
        .await
        .cloned()
}

/// Random path part: other programs on this machine cannot guess it.
fn unguessable() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for i in 0..2u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// Serve `root` and return its base address, `http://127.0.0.1:<port>/p/<token>/`.
async fn serve_root(root: &Path) -> Result<(Arc<Preview>, String)> {
    let p = preview().await?;
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let token = {
        let mut roots = p.roots.lock().unwrap_or_else(|e| e.into_inner());
        match roots.iter().find(|(_, r)| **r == root) {
            Some((t, _)) => t.clone(),
            None => {
                let t = unguessable();
                roots.insert(t.clone(), root);
                t
            }
        }
    };
    Ok((p, token))
}

/// The address the page `rel` (relative to `root`) is served at.
pub async fn page_url(root: &Path, rel: &str) -> Result<String> {
    let (p, token) = serve_root(root).await?;
    Ok(format!("http://{}/p/{token}/{}", p.addr, url_path(rel)))
}

fn url_path(rel: &str) -> String {
    rel.replace('\\', "/").trim_start_matches("./").trim_start_matches('/').to_string()
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("txt" | "md") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

type Resp = axum::response::Response;

fn status(code: u16, text: &str) -> Resp {
    use axum::response::IntoResponse;
    (axum::http::StatusCode::from_u16(code).unwrap_or(axum::http::StatusCode::NOT_FOUND), text.to_string()).into_response()
}

async fn serve_file(
    axum::extract::State(p): axum::extract::State<Arc<Preview>>,
    axum::extract::Path((token, path)): axum::extract::Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Resp {
    use axum::response::IntoResponse;
    let Some(root) = p.roots.lock().unwrap_or_else(|e| e.into_inner()).get(&token).cloned() else {
        return status(404, "not found");
    };
    // Same rule as the file tools: nothing outside the folder.
    let Ok(mut file) = crate::tools::confine_path(&root, &path) else {
        return status(403, "outside the served folder");
    };
    if file.is_dir() {
        file = file.join("index.html");
    }
    let Ok(bytes) = std::fs::read(&file) else {
        return status(404, "not found");
    };
    let kind = content_type(&file);
    let probe = query.as_deref().is_some_and(|q| q.split('&').any(|kv| kv == "__probe=1"));
    let body = if probe && kind.starts_with("text/html") {
        inject_probe(&String::from_utf8_lossy(&bytes)).into_bytes()
    } else {
        bytes
    };
    (
        [(axum::http::header::CONTENT_TYPE, kind), (axum::http::header::CACHE_CONTROL, "no-store")],
        body,
    )
        .into_response()
}

/// One iframe per width (`?w=375,1440`); each frame reports to `__report`.
async fn serve_harness(
    axum::extract::Path((token, path)): axum::extract::Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Resp {
    use axum::response::IntoResponse;
    let widths: Vec<u32> = query
        .as_deref()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("w=")))
        .map(|w| w.split(',').filter_map(|x| x.parse().ok()).filter(|x| (200..=4000).contains(x)).collect())
        .unwrap_or_default();
    let widths = if widths.is_empty() { DEFAULT_WIDTHS.to_vec() } else { widths };
    let path = url_path(&path).replace('"', "%22");
    let frames: String = widths
        .iter()
        .map(|w| {
            format!(
                "<iframe src=\"/p/{token}/{path}?__probe=1\" width=\"{w}\" height=\"900\" \
                 style=\"border:0;display:block\"></iframe>"
            )
        })
        .collect();
    let html = format!(
        "<!doctype html><html><body style=\"margin:0\"><script>window.__res={{}};\
         window.__report=function(o){{__res[o.w]=o;if(Object.keys(__res).length>={n}){{\
         var s=document.createElement('script');s.type='application/json';s.id='__probe';\
         s.textContent=JSON.stringify(__res);document.body.appendChild(s);}}}};</script>{frames}</body></html>",
        n = widths.len()
    );
    ([(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

// -------------------------------------------------------------------- probe

/// Runs first: collect script errors and failed loads from the start.
const EARLY: &str = "<script>window.__errs=[];window.__fails=[];\
addEventListener('error',function(e){var t=e.target;if(t&&t!==window&&(t.src||t.href)){__fails.push(t)}\
else{__errs.push(String(e.message||'error'))}},true);\
addEventListener('unhandledrejection',function(e){__errs.push('unhandled promise rejection: '+e.reason)});\
(function(){var ce=console.error;console.error=function(){try{__errs.push([].map.call(arguments,String).join(' '))}catch(x){}\
return ce.apply(console,arguments)}})();</script>";

/// Runs after the page's own scripts. Every finding is written as a sentence
/// the model can act on.
const PROBE: &str = r#"<script>(function(){
function vis(el){if(!el||!el.getBoundingClientRect)return false;var s=getComputedStyle(el),r=el.getBoundingClientRect();
 return s.display!=='none'&&s.visibility!=='hidden'&&+s.opacity>0.05&&r.width>0&&r.height>0;}
function desc(el){var s=el.tagName.toLowerCase();if(el.id)return s+'#'+el.id;
 if(el.classList&&el.classList.length)s+='.'+[].slice.call(el.classList,0,2).join('.');return s;}
function run(){
 var W=document.documentElement.clientWidth,P=[],OK=[];
 var sw=document.documentElement.scrollWidth;
 if(sw>W+1){var out=[],all=document.body?document.body.getElementsByTagName('*'):[];
  for(var i=0;i<all.length&&out.length<4;i++){var el=all[i],r=el.getBoundingClientRect();
   if(r.width<=0||r.right<=W+1||getComputedStyle(el).position==='fixed')continue;
   var par=el.parentElement,pr=par&&par.getBoundingClientRect();
   if(par&&par!==document.body&&pr.right>W+1)continue;
   out.push(desc(el)+' (right edge at '+Math.round(r.right)+'px, '+Math.round(r.width)+'px wide)');}
  P.push('the page scrolls sideways: it is '+sw+'px wide on a '+W+'px screen. Overflow starts at: '+(out.join('; ')||'unknown'));
 }else OK.push('no sideways scrolling');
 var lists=document.querySelectorAll('nav ul, nav ol'),bul=false;
 for(var j=0;j<lists.length;j++){var li=lists[j].querySelector('li');
  if(li&&vis(li)&&getComputedStyle(li).listStyleType!=='none'&&getComputedStyle(li).display==='list-item')bul=true;}
 if(bul)P.push('the navigation list shows bullet points: give the nav <ul> list-style: none (and no default padding)');
 var navs=document.querySelectorAll('header nav');if(!navs.length)navs=document.querySelectorAll('nav');
 var links=[].filter.call(navs.length?navs[0].querySelectorAll('a'):[],vis);
 if(W>=1024&&links.length>=3){var rows={};links.forEach(function(a){rows[Math.round(a.getBoundingClientRect().top/8)]=1});
  if(Object.keys(rows).length===links.length)P.push('the navigation links are stacked one per line instead of in a row');
  else OK.push('navigation in a row');}
 var btn=[].filter.call(document.querySelectorAll('header [aria-expanded], nav [aria-expanded]'),function(b){return b.tagName==='BUTTON'||b.getAttribute('role')==='button'})[0];
 if(btn&&vis(btn)){var kids=btn.querySelectorAll('*');for(var k=0;k<kids.length;k++){var c=kids[k],cr=c.getBoundingClientRect(),cs=getComputedStyle(c);
   var bg=cs.backgroundColor,opaque=bg&&bg!=='transparent'&&!/rgba\(.*,\s*0\)$/.test(bg);
   if(vis(c)&&opaque&&cr.width>=12&&cr.height>=12&&cr.width/cr.height>0.6&&cr.width/cr.height<1.6&&c.children.length===0&&!c.textContent.trim()){
    P.push('the menu button icon draws a solid block ('+Math.round(cr.width)+'x'+Math.round(cr.height)+'px '+desc(c)+'), not a recognisable icon');break;}}}
 var svgs=document.querySelectorAll('svg');for(var s=0;s<svgs.length;s++){var sv=svgs[s];
  if(vis(sv)&&!sv.querySelector('path,circle,rect,line,polyline,polygon,ellipse,use,text,image')){P.push('an svg icon draws nothing: '+desc(sv));break;}}
 var imgs=document.images;for(var m=0;m<imgs.length;m++){if(imgs[m].complete&&imgs[m].naturalWidth===0&&vis(imgs[m]))P.push('an image does not load: '+(imgs[m].getAttribute('src')||desc(imgs[m])));}
 // Files of this page that did not load: the browser's own request log
 // with HTTP status (error events miss stylesheet 404s, and a stylesheet
 // whose @import of an outside font fails, blocked here, fires one wrongly).
 var bad={};(performance.getEntriesByType('resource')||[]).forEach(function(r){
  if(r.name.indexOf(location.origin)===0&&(r.responseStatus>=400||r.responseStatus===0))bad[r.name]=1;});
 (window.__fails||[]).forEach(function(t){var u=t.src||t.href||'';if(u.indexOf(location.origin)===0&&!(t.tagName==='LINK'&&t.sheet))bad[u]=1;});
 Object.keys(bad).slice(0,5).forEach(function(u){P.push('a file failed to load: '+decodeURI(u.slice(location.origin.length).replace(/^\/p\/[0-9a-f]+\//,'').split('?')[0]))});
 var E=(window.__errs||[]);if(E.length)P.push('script error'+(E.length>1?'s':'')+': '+E.slice(0,3).join(' | '));else OK.push('no script errors');
 if(W<768){
  if(!links.length&&!(btn&&vis(btn)))P.push('the navigation cannot be reached: its links are hidden and there is no visible menu button');
  else if(btn&&vis(btn)){var target=document.getElementById(btn.getAttribute('aria-controls')||'')||btn.closest('header')||document;
   btn.click();var open=btn.getAttribute('aria-expanded'),shown=[].some.call(target.querySelectorAll('a'),vis);
   if(open!=='true')P.push('the menu button does not set aria-expanded="true" when clicked');
   if(!shown)P.push('clicking the menu button shows no navigation links');
   btn.click();if(btn.getAttribute('aria-expanded')!=='false')P.push('clicking the menu button again does not close it (aria-expanded stays "'+btn.getAttribute('aria-expanded')+'")');
   if(open==='true'&&shown)OK.push('mobile menu opens and closes');}
 }
 var o={w:innerWidth,problems:P,ok:OK,text:(document.body?document.body.innerText:'').slice(0,20000)};
 if(window.parent&&window.parent!==window&&window.parent.__report){window.parent.__report(o)}
 else{var sc=document.createElement('script');sc.type='application/json';sc.id='__probe';sc.textContent=JSON.stringify(o);document.body.appendChild(sc);}
}
function later(){setTimeout(function(){try{run()}catch(e){var o={w:innerWidth,problems:['the page check itself failed: '+e],ok:[],text:''};
 if(window.parent&&window.parent.__report)window.parent.__report(o)}},900)}
if(document.readyState==='loading')addEventListener('DOMContentLoaded',later);else later();
})();</script>"#;

fn inject_probe(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = html.to_string();
    // The probe goes last so the page's own scripts have run.
    match lower.rfind("</body>") {
        Some(i) => out.insert_str(i, PROBE),
        None => out.push_str(PROBE),
    }
    match lower.find("<head").and_then(|i| lower[i..].find('>').map(|j| i + j + 1)) {
        Some(i) => out.insert_str(i, EARLY),
        None => out.insert_str(0, EARLY),
    }
    out
}

// -------------------------------------------------------------------- check

/// Check `page` (relative to `root`) at `widths`. `expect` are texts that must
/// appear on the page as written (brand names, headings from the request).
pub async fn check(root: &Path, page: &str, widths: &[u32], expect: &[String]) -> Result<Report> {
    let file = crate::tools::confine_path(root, page)?;
    if !file.is_file() {
        return Err(anyhow!("{page} does not exist"));
    }
    let exe = find_browser().ok_or_else(|| {
        anyhow!("no Edge or Chrome found to check the page with (set OPENCODE_RUST_BROWSER to its path)")
    })?;
    // `confine_path` keeps the workspace spelled as given (no `\\?\`), so
    // strip that form; an absolute `page` becomes relative here too.
    let base = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let rel = file
        .strip_prefix(&base)
        .or_else(|_| file.strip_prefix(root.canonicalize().unwrap_or_else(|_| root.to_path_buf())))
        .map(|p| p.to_path_buf())
        .map_err(|_| anyhow!("{page} is outside the folder being checked"))?;
    let rel = url_path(&rel.to_string_lossy());
    let (p, token) = serve_root(root).await?;
    let widths: Vec<u32> = if widths.is_empty() { DEFAULT_WIDTHS.to_vec() } else { widths.to_vec() };
    let list = widths.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let harness = format!("http://{}/c/{token}/{rel}?w={list}", p.addr);
    let dom = dump_dom(&exe, &harness, Duration::from_secs(120)).await?;
    let json = between(&dom, "<script type=\"application/json\" id=\"__probe\">", "</script>")
        .ok_or_else(|| anyhow!("the page did not finish loading in the browser (no probe result)"))?;
    let all: HashMap<String, Value> =
        serde_json::from_str(&html_unescape(json)).context("reading the probe result")?;
    let mut report = Report { page: rel.clone(), url: format!("http://{}/p/{token}/{rel}", p.addr), ..Default::default() };
    let mut text = String::new();
    for w in &widths {
        let Some(r) = all.get(&w.to_string()) else { continue };
        let strings = |k: &str| -> Vec<String> {
            r.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default()
        };
        if text.is_empty() {
            text = r.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        }
        report.widths.push(WidthResult { width: *w, problems: strings("problems"), ok: strings("ok") });
    }
    let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let page_text = flat(&text);
    for e in expect {
        let want = flat(e);
        if !want.is_empty() && !page_text.contains(&want) {
            report.general.push(format!("the page should show \"{want}\" as written, but it does not appear in the visible text"));
        }
    }
    Ok(report)
}

/// `--dump-dom` prints the DOM as HTML: text inside <script> is not escaped,
/// but be safe about the few entities a serializer may emit.
fn html_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&")
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = s.find(start)? + start.len();
    let j = s[i..].find(end)? + i;
    Some(&s[i..j])
}

/// Quoted phrases in a request ("Lumen Coffee Roasters"): names the page
/// should show exactly. Short ones only; code-like text is skipped.
pub fn expected_texts(request: &str) -> Vec<String> {
    let mut out = Vec::new();
    for q in ['"', '“'] {
        let close = if q == '“' { '”' } else { '"' };
        let mut rest = request;
        while let Some(i) = rest.find(q) {
            let after = &rest[i + q.len_utf8()..];
            let Some(j) = after.find(close) else { break };
            let phrase = after[..j].trim();
            let wordy = phrase.chars().any(char::is_alphabetic)
                && !phrase.contains(['<', '>', '=', '{', '}', '/', '#', '_'])
                && phrase.split_whitespace().count() >= 2;
            if wordy && phrase.len() <= 60 && !out.iter().any(|p: &String| p == phrase) {
                out.push(phrase.to_string());
            }
            rest = &after[j + close.len_utf8()..];
        }
    }
    out.truncate(5);
    out
}

/// The `check_page` tool.
pub async fn run_tool(root: &Path, input: &Value) -> Result<(String, Report)> {
    let page = input.get("path").and_then(Value::as_str).filter(|p| !p.trim().is_empty()).unwrap_or("index.html");
    let widths: Vec<u32> = input
        .get("widths")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).map(|w| w as u32).filter(|w| (200..=4000).contains(w)).take(4).collect())
        .unwrap_or_default();
    let expect: Vec<String> = input
        .get("expect_text")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).take(10).collect())
        .unwrap_or_default();
    let report = check(root, page, &widths, &expect).await?;
    Ok((report.render(), report))
}

fn web_ext(p: &Path) -> &'static str {
    match p.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("html" | "htm") => "page",
        Some("css" | "js" | "mjs") => "asset",
        _ => "",
    }
}

/// The pages to check for these changed files: changed .html files, and
/// the index.html next to a changed .css/.js file. At most `max`, as paths
/// relative to `root`.
pub fn pages_for(root: &Path, files: &[PathBuf], max: usize) -> Vec<String> {
    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let mut pages: Vec<PathBuf> = Vec::new();
    for f in files {
        let page = match web_ext(f) {
            "page" => f.clone(),
            "asset" => match f.parent() {
                Some(dir) => dir.join("index.html"),
                None => continue,
            },
            _ => continue,
        };
        if page.is_file() && !pages.contains(&page) {
            pages.push(page);
        }
    }
    pages
        .into_iter()
        .filter_map(|p| p.strip_prefix(&root).ok().map(|r| url_path(&r.to_string_lossy())))
        .take(max)
        .collect()
}

/// Web files under `root` modified at or after `since` (for changes made by
/// shell commands, which the write log does not see). Shallow: at most 3
/// folders deep; build, dependency and hidden folders are skipped.
pub fn recently_modified(root: &Path, since: std::time::SystemTime) -> Vec<PathBuf> {
    const SKIP: &[&str] = &["node_modules", "target", "dist", "build", "vendor", "__pycache__"];
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut seen = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            seen += 1;
            if seen > 5_000 {
                return out;
            }
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                if depth < 3 && !name.starts_with('.') && !SKIP.contains(&name.as_str()) {
                    stack.push((path, depth + 1));
                }
            } else if !web_ext(&path).is_empty() && meta.modified().is_ok_and(|m| m >= since) {
                out.push(path);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_names_become_expected_texts() {
        let req = "build a homepage for \"Lumen Coffee Roasters\" with id \"form-status\" and a \"<header>\", “Fresh Daily”";
        assert_eq!(expected_texts(req), vec!["Lumen Coffee Roasters".to_string(), "Fresh Daily".to_string()]);
    }

    #[test]
    fn the_probe_goes_into_head_and_body() {
        let out = inject_probe("<html><head><title>t</title></head><body><p>x</p></body></html>");
        assert!(out.find("__errs").unwrap() < out.find("<title>").unwrap(), "error capture first");
        assert!(out.find("__report").unwrap() > out.find("<p>x</p>").unwrap(), "probe after the content");
    }

    #[test]
    fn a_report_numbers_problems_and_scores_them() {
        let r = Report {
            page: "index.html".into(),
            url: "http://x/p/t/index.html".into(),
            widths: vec![
                WidthResult { width: 375, problems: vec!["a".into()], ok: vec!["no script errors".into()] },
                WidthResult { width: 1440, problems: vec![], ok: vec!["no script errors".into()] },
            ],
            general: vec!["g".into()],
        };
        assert_eq!(r.score(), 2);
        let text = r.render();
        assert!(text.contains("1. g\n2. [375px] a"), "{text}");
        assert_eq!(text.matches("no script errors").count(), 1, "passes listed once");
    }

    #[test]
    fn changed_files_map_to_the_pages_to_check() {
        let dir = std::env::temp_dir().join(format!("ocrs-pages-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("site")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        let before = std::time::SystemTime::now() - Duration::from_secs(5);
        std::fs::write(dir.join("site").join("index.html"), "<p>").unwrap();
        std::fs::write(dir.join("site").join("styles.css"), "p{}").unwrap();
        std::fs::write(dir.join("about.html"), "<p>").unwrap();
        std::fs::write(dir.join("node_modules").join("x.html"), "<p>").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        let css = vec![dir.join("site").join("styles.css"), dir.join("notes.txt")];
        assert_eq!(pages_for(&dir, &css, 3), vec!["site/index.html".to_string()], "css → its index.html");
        let mut found = recently_modified(&dir, before);
        found.sort();
        assert_eq!(found.len(), 3, "node_modules and non-web files skipped: {found:?}");
        assert!(recently_modified(&dir, std::time::SystemTime::now() + Duration::from_secs(60)).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A real browser run, when Edge or Chrome is installed: an overflowing
    /// page with a bulleted nav and a block icon is caught; a fixed one passes.
    #[tokio::test]
    async fn a_real_browser_catches_layout_bugs() {
        if find_browser().is_none() {
            eprintln!("skipped: no Edge/Chrome");
            return;
        }
        let dir = std::env::temp_dir().join(format!("ocrs-pagecheck-site-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bad = r##"<!doctype html><html><head><meta name="viewport" content="width=device-width"><link rel="stylesheet" href="s.css"><link rel="stylesheet" href="missing.css"></head>
<body><header><nav><ul><li><a href="#a">A</a></li><li><a href="#b">B</a></li><li><a href="#c">C</a></li></ul></nav>
<button aria-expanded="false" aria-controls="n"><span class="icon"></span></button></header>
<main><div class="wide">Lumen<span>Coffee</span> Roasters</div></main><script>nope()</script></body></html>"##;
        std::fs::write(dir.join("index.html"), bad).unwrap();
        std::fs::write(dir.join("s.css"), ".wide{width:900px} .icon{display:block;width:20px;height:20px;background:#000}").unwrap();
        let expect = vec!["Lumen Coffee Roasters".to_string()];
        let r = check(&dir, "index.html", &[375, 1440], &expect).await.unwrap();
        let text = r.render();
        assert!(text.contains("scrolls sideways") && text.contains("div.wide"), "{text}");
        assert!(text.contains("bullet points"), "{text}");
        assert!(text.contains("stacked one per line"), "{text}");
        assert!(text.contains("solid block"), "{text}");
        assert!(text.contains("script error"), "{text}");
        assert!(text.contains("\"Lumen Coffee Roasters\""), "{text}");
        assert!(text.contains("a file failed to load: missing.css"), "a real 404 is reported by name: {text}");
        assert!(!text.contains("s.css"), "a stylesheet that loaded is not blamed: {text}");

        // A stylesheet that @imports a web font (outside hosts are blocked
        // during the check) loaded fine and must not be reported.
        std::fs::write(dir.join("g.css"), "@import url('https://fonts.googleapis.com/css2?family=Inter');\nbody{margin:0}").unwrap();
        let good = r##"<!doctype html><html><head><meta name="viewport" content="width=device-width"><link rel="stylesheet" href="g.css"><style>
nav ul{list-style:none;display:flex;gap:8px;margin:0;padding:0} button{display:none}
@media (max-width:767px){nav{display:none} nav.open{display:block} button{display:block}}</style></head>
<body><header><nav id="n"><ul><li><a href="#a">A</a></li><li><a href="#b">B</a></li><li><a href="#c">C</a></li></ul></nav>
<button aria-expanded="false" aria-controls="n">Menu</button></header><main><h1>Lumen Coffee Roasters</h1></main>
<script>var b=document.querySelector('button'),n=document.getElementById('n');b.onclick=function(){var o=b.getAttribute('aria-expanded')!=='true';
b.setAttribute('aria-expanded',String(o));n.classList.toggle('open',o)}</script></body></html>"##;
        std::fs::write(dir.join("index.html"), good).unwrap();
        // The agent may name the page by its full path.
        let full = dir.join("index.html").to_string_lossy().to_string();
        let r = check(&dir, &full, &[375, 1440], &expect).await.unwrap();
        assert_eq!(r.page, "index.html");
        assert_eq!(r.score(), 0, "{}", r.render());
        assert!(r.render().contains("mobile menu opens and closes"), "{}", r.render());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
