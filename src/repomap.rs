//! Project map: a short outline of the project (files and their main
//! functions, types and classes) that goes into the system prompt, so the
//! model knows where things are without spending tool calls looking.
//!
//! - Fixed size: at most `agent.repo_map_tokens` (default 1000); a project
//!   with thousands of files gets its most relevant ones, the rest as a
//!   count per folder.
//! - Most relevant first: files named in the chat's first message, files
//!   whose parts it names, files changed in git, entry points.
//! - Cheap: each file's outline is cached by size and modification time, so
//!   only changed files are read again. Ignored folders (.gitignore,
//!   node_modules, target, dist…) are never walked.
//! - Built once per chat and then kept, so the system prompt stays the same
//!   from request to request (prompt caching keeps working).
//!
//! The same outline backs `read` with `outline: true`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Most files walked; a folder with more is not one project.
const MAX_FILES: usize = 5000;
/// Bigger files are listed but not outlined (generated or data files).
const MAX_OUTLINE_BYTES: u64 = 400_000;
/// Parts shown per file in the map.
const MAX_PARTS: usize = 12;
/// A walk stops after this many entries or this long, whatever it found
/// (a home folder or a data folder opened as a project).
const MAX_VISITED: usize = 60_000;
const MAX_SCAN: std::time::Duration = std::time::Duration::from_secs(6);

/// One part of a file: where it starts, what it is, its name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Part {
    pub line: usize,
    pub kind: String,
    pub name: String,
    /// Not indented (a top-level item, not a method or nested helper).
    #[serde(default)]
    pub top: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Lang {
    Rust,
    Python,
    Js,
    Go,
    CLike,
    Ruby,
    Php,
    Markdown,
    Html,
}

fn lang_of(path: &Path) -> Option<Lang> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => Lang::Rust,
        "py" | "pyi" => Lang::Python,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "vue" | "svelte" => Lang::Js,
        "go" => Lang::Go,
        "java" | "kt" | "kts" | "cs" | "c" | "h" | "cpp" | "cc" | "hpp" | "swift" | "scala" | "dart" => Lang::CLike,
        "rb" => Lang::Ruby,
        "php" => Lang::Php,
        "md" | "markdown" => Lang::Markdown,
        "html" | "htm" => Lang::Html,
        _ => return None,
    })
}

/// Files worth listing even without an outline (styles, config, data).
fn listed(path: &Path) -> bool {
    if lang_of(path).is_some() {
        return true;
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(ext.as_str(), "css" | "scss" | "sql" | "sh" | "toml" | "yaml" | "yml" | "json" | "proto" | "graphql")
        && !name.ends_with(".lock")
        && name != "package-lock.json"
}

struct Rules {
    rust: regex::Regex,
    rust_impl: regex::Regex,
    py: regex::Regex,
    js_fn: regex::Regex,
    js_class: regex::Regex,
    js_const: regex::Regex,
    js_type: regex::Regex,
    js_method: regex::Regex,
    go_fn: regex::Regex,
    go_type: regex::Regex,
    c_type: regex::Regex,
    ruby: regex::Regex,
    php: regex::Regex,
    md: regex::Regex,
    html_title: regex::Regex,
    html_id: regex::Regex,
}

fn rules() -> &'static Rules {
    static R: OnceLock<Rules> = OnceLock::new();
    R.get_or_init(|| {
        let re = |s: &str| regex::Regex::new(s).expect("repomap regex");
        Rules {
            rust: re(r"^ {0,4}(?:pub(?:\([^)]*\))? +)?(?:async +)?(?:const +)?(?:unsafe +)?(?:extern +\S+ +)?(fn|struct|enum|trait|mod|type|union)\s+([A-Za-z_][A-Za-z0-9_]*)"),
            rust_impl: re(r"^impl(?:<[^{]*?>)? +([^{]+?)\s*(?:where\b[^{]*)?\{?\s*$"),
            py: re(r"^( {0,4})(?:async +)?(def|class) +([A-Za-z_][A-Za-z0-9_]*)"),
            js_fn: re(r"^ {0,2}(?:export +)?(?:default +)?(?:async +)?function\*? *([A-Za-z_$][\w$]*)"),
            js_class: re(r"^ {0,2}(?:export +)?(?:default +)?(?:abstract +)?class +([A-Za-z_$][\w$]*)"),
            js_const: re(r"^(?:export +)?(?:const|let|var) +([A-Za-z_$][\w$]*) *= *(?:async *)?(?:\([^)]*\) *=>|[A-Za-z_$][\w$]* *=>|function)"),
            js_type: re(r"^(?:export +)?(?:declare +)?(interface|type|enum) +([A-Za-z_$][\w$]*)"),
            js_method: re(r"^ {2,4}(?:static +)?(?:async +)?(?:get +|set +)?([A-Za-z_$][\w$]*) *\([^)]*\) *\{"),
            go_fn: re(r"^func +(?:\([^)]*\) *)?([A-Za-z_][A-Za-z0-9_]*)"),
            go_type: re(r"^type +([A-Za-z_][A-Za-z0-9_]*) +(struct|interface)"),
            c_type: re(r"^\s*(?:(?:public|private|protected|internal|static|final|abstract|sealed|partial|open|data|export|typedef)\s+)*(class|interface|struct|enum|record|object|protocol)\s+([A-Za-z_][A-Za-z0-9_]*)"),
            ruby: re(r"^ {0,4}(def|class|module) +([A-Za-z_][A-Za-z0-9_?!.]*)"),
            php: re(r"^\s*(?:(?:public|private|protected|static|abstract|final)\s+)*(function|class|interface|trait)\s+([A-Za-z_][A-Za-z0-9_]*)"),
            md: re(r"^(#{1,3}) +(.+?)\s*#*\s*$"),
            html_title: re(r"(?i)<title>([^<]{1,80})</title>"),
            html_id: re(r#"(?i)<(section|nav|header|footer|main|form|dialog)\b[^>]*\bid="([^"]{1,40})""#),
        }
    })
}

const JS_NOT_METHODS: [&str; 9] = ["if", "for", "while", "switch", "catch", "function", "return", "constructor", "else"];

/// The main parts of a file, in order. Empty for kinds of files we do not
/// outline. Rust stops at a top-level `#[cfg(test)]` (tests come last).
pub fn outline(path: &Path, text: &str) -> Vec<Part> {
    let Some(lang) = lang_of(path) else { return Vec::new() };
    let r = rules();
    let mut out = Vec::new();
    let top = std::cell::Cell::new(false);
    let mut push = |line: usize, kind: &str, name: &str| {
        out.push(Part { line, kind: kind.to_string(), name: name.trim().to_string(), top: top.get() });
    };
    for (i, l) in text.lines().enumerate() {
        let n = i + 1;
        top.set(!l.starts_with([' ', '\t']));
        if l.len() > 400 {
            continue; // minified or generated
        }
        match lang {
            Lang::Rust => {
                if l.starts_with("#[cfg(test)]") {
                    break;
                }
                if let Some(c) = r.rust.captures(l) {
                    push(n, &c[1], &c[2]);
                } else if let Some(c) = r.rust_impl.captures(l) {
                    push(n, "impl", &c[1]);
                }
            }
            Lang::Python => {
                if let Some(c) = r.py.captures(l) {
                    push(n, &c[2], &c[3]);
                }
            }
            Lang::Js => {
                if let Some(c) = r.js_fn.captures(l) {
                    push(n, "fn", &c[1]);
                } else if let Some(c) = r.js_class.captures(l) {
                    push(n, "class", &c[1]);
                } else if let Some(c) = r.js_const.captures(l) {
                    push(n, "fn", &c[1]);
                } else if let Some(c) = r.js_type.captures(l) {
                    push(n, &c[1], &c[2]);
                } else if let Some(c) = r.js_method.captures(l) {
                    if !JS_NOT_METHODS.contains(&&c[1]) {
                        push(n, "method", &c[1]);
                    }
                }
            }
            Lang::Go => {
                if let Some(c) = r.go_fn.captures(l) {
                    push(n, "fn", &c[1]);
                } else if let Some(c) = r.go_type.captures(l) {
                    push(n, &c[2], &c[1]);
                }
            }
            Lang::CLike => {
                if let Some(c) = r.c_type.captures(l) {
                    push(n, &c[1], &c[2]);
                }
            }
            Lang::Ruby => {
                if let Some(c) = r.ruby.captures(l) {
                    push(n, &c[1], &c[2]);
                }
            }
            Lang::Php => {
                if let Some(c) = r.php.captures(l) {
                    push(n, &c[1], &c[2]);
                }
            }
            Lang::Markdown => {
                if let Some(c) = r.md.captures(l) {
                    push(n, &c[1], &c[2]);
                }
            }
            Lang::Html => {
                if let Some(c) = r.html_title.captures(l) {
                    push(n, "title", &c[1]);
                }
                for c in r.html_id.captures_iter(l) {
                    push(n, &c[1], &format!("#{}", &c[2]));
                }
            }
        }
    }
    out
}

/// `read` with `outline: true`: the parts with their line numbers.
pub fn outline_text(display: &str, text: &str, abs: &Path) -> String {
    let parts = outline(abs, text);
    let lines = text.lines().count();
    if parts.is_empty() {
        return format!(
            "{display}: {lines} lines, no outline for this kind of file. Read it with offset/limit, or grep for what you need."
        );
    }
    let mut out = format!("{display}: {lines} lines, {} parts (line, kind, name). Read a part with offset:<line>.\n", parts.len());
    for p in parts.iter().take(400) {
        out.push_str(&format!("{:>6}  {} {}\n", p.line, p.kind, p.name));
    }
    if parts.len() > 400 {
        out.push_str(&format!("… {} more\n", parts.len() - 400));
    }
    out
}

/// One file of the project, with its cached outline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub rel: String,
    size: u64,
    mtime: u128,
    pub parts: Vec<Part>,
}

fn cache_path(state_dir: &Path, root: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf()).hash(&mut h);
    state_dir.join("repomap").join(format!("{:016x}.json", h.finish()))
}

/// Folders that are never part of the map, ignored or not.
fn skipped_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "node_modules" | "target" | "dist" | "build" | "out" | ".next" | ".nuxt" | "vendor"
            | "__pycache__" | ".venv" | "venv" | ".opencode" | "coverage" | ".cache" | "third_party"
    )
}

/// Walk `root` and outline its files, reusing the cache for files whose
/// size and modification time did not change. Saves the cache again.
pub fn scan(root: &Path, state_dir: &Path) -> Vec<FileEntry> {
    let cache_file = cache_path(state_dir, root);
    let old: HashMap<String, FileEntry> = std::fs::read(&cache_file)
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<FileEntry>>(&b).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|e| (e.rel.clone(), e))
        .collect();
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .require_git(false)
        .filter_entry(|e| !(e.file_type().is_some_and(|t| t.is_dir()) && e.file_name().to_str().is_some_and(skipped_dir)))
        .build();
    let started = std::time::Instant::now();
    for (visited, entry) in walker.flatten().enumerate() {
        if out.len() >= MAX_FILES || visited >= MAX_VISITED || started.elapsed() > MAX_SCAN {
            break;
        }
        let path = entry.path();
        if !entry.file_type().is_some_and(|t| t.is_file()) || !listed(path) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        let size = meta.len();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        if let Some(e) = old.get(&rel).filter(|e| e.size == size && e.mtime == mtime) {
            out.push(e.clone());
            continue;
        }
        let parts = if size <= MAX_OUTLINE_BYTES && lang_of(path).is_some() {
            std::fs::read(path).map(|b| outline(path, &String::from_utf8_lossy(&b))).unwrap_or_default()
        } else {
            Vec::new()
        };
        out.push(FileEntry { rel, size, mtime, parts });
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    if let Some(dir) = cache_file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec(&out) {
        let _ = std::fs::write(&cache_file, json);
    }
    out
}

/// Words of the request that may name a file or a part (3+ characters).
fn request_words(request: &str) -> Vec<String> {
    request
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '/' || c == '-'))
        .map(|w| w.trim_matches(|c| c == '.' || c == '/' || c == '-').to_lowercase())
        .filter(|w| w.chars().count() >= 3)
        .collect()
}

/// The lowercase pieces of a name: `start_heartbeat` and `startHeartbeat`
/// both give `start`, `heartbeat`, plus the whole name.
fn name_pieces(name: &str) -> Vec<String> {
    let mut pieces = vec![name.to_lowercase()];
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                pieces.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            pieces.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        pieces.push(cur);
    }
    pieces
}

/// Does the request name this part (its name or a 4+ letter piece of it)?
fn part_named(p: &Part, words: &[String]) -> bool {
    name_pieces(&p.name)
        .iter()
        .enumerate()
        .any(|(i, piece)| (i == 0 || piece.chars().count() >= 4) && words.iter().any(|w| w == piece))
}

fn score(e: &FileEntry, words: &[String], changed: &[String]) -> i64 {
    let rel = e.rel.to_lowercase();
    let file = rel.rsplit('/').next().unwrap_or(&rel).to_string();
    let stem = file.split('.').next().unwrap_or(&file).to_string();
    let mut s: i64 = 0;
    if words.iter().any(|w| *w == file || *w == stem || (w.contains('/') && rel.ends_with(w.as_str()))) {
        s += 60;
    }
    if e.parts.iter().any(|p| part_named(p, words)) {
        s += 35;
    }
    if changed.iter().any(|c| c.eq_ignore_ascii_case(&e.rel)) {
        s += 40;
    }
    if matches!(stem.as_str(), "main" | "lib" | "index" | "app" | "server" | "cli" | "mod" | "manage" | "setup") {
        s += 15;
    }
    if rel == "readme.md" {
        s += 10;
    } else if file.ends_with(".md") {
        s -= 8;
    }
    if rel.contains("test") || rel.contains("spec") || rel.contains("example") {
        s -= 6;
    }
    s += (e.parts.len().min(24) / 4) as i64;
    s -= 3 * rel.matches('/').count() as i64;
    s
}

/// The parts worth a place in the map, best first: the ones the request
/// names, then types, then top-level functions, then methods. `impl`
/// blocks are left out (their methods carry the names), and documents
/// show only their first headings.
fn map_parts<'a>(e: &'a FileEntry, words: &[String]) -> Vec<&'a Part> {
    let doc = e.parts.first().is_some_and(|p| p.kind.starts_with('#') || p.kind == "title");
    let rank = |p: &Part| -> u8 {
        if part_named(p, words) {
            return 0;
        }
        match p.kind.as_str() {
            "struct" | "enum" | "trait" | "class" | "interface" | "type" | "record" | "union" | "module" | "object" => 1,
            _ if p.top => 2,
            _ => 3,
        }
    };
    let mut v: Vec<&Part> = e.parts.iter().filter(|p| p.kind != "impl" && !(doc && p.kind == "###")).collect();
    if doc {
        v.truncate(5);
        return v;
    }
    v.sort_by_key(|p| (rank(p), p.line));
    v
}

fn part_label(p: &Part) -> String {
    match p.kind.as_str() {
        "fn" | "def" | "function" | "method" => format!("{}()", p.name),
        "impl" => format!("impl {}", p.name),
        "#" | "##" | "###" | "title" => format!("\"{}\"", p.name),
        _ => p.name.clone(),
    }
}

/// The map text for the system prompt, at most about `budget_tokens`.
/// Empty when there is nothing to show.
pub fn render(entries: &[FileEntry], request: &str, changed: &[String], budget_tokens: usize) -> String {
    if entries.is_empty() || budget_tokens == 0 {
        return String::new();
    }
    // Code averages about 3.5 characters per token.
    let budget = budget_tokens * 7 / 2;
    let words = request_words(request);
    let mut ranked: Vec<(i64, &FileEntry)> = entries.iter().map(|e| (score(e, &words, changed), e)).collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.rel.cmp(&b.1.rel)));
    let head = "\n\n## Project map\n\nFiles and their main parts, most relevant first (paths from the project folder). \
                `read` with `outline: true` lists a file's parts with line numbers.\n";
    let mut out = String::from(head);
    let mut shown = 0;
    for (_, e) in &ranked {
        let mut line = format!("- {}", e.rel);
        let parts = map_parts(e, &words);
        if !parts.is_empty() {
            let labels: Vec<String> = parts.iter().take(MAX_PARTS).map(|p| part_label(p)).collect();
            line.push_str(": ");
            line.push_str(&labels.join(", "));
            if parts.len() > MAX_PARTS {
                line.push_str(&format!(" (+{} more)", parts.len() - MAX_PARTS));
            }
        }
        line.push('\n');
        // Keep ~150 characters for the summary of the files left out.
        if out.len() + line.len() > budget.saturating_sub(150) {
            if shown == 0 {
                let cut: String = line.chars().take(budget.saturating_sub(out.len() + 150)).collect();
                out.push_str(&cut);
                out.push('\n');
                shown = 1;
            }
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    let rest = ranked.len() - shown;
    if rest > 0 {
        let mut dirs: HashMap<&str, usize> = HashMap::new();
        for (_, e) in &ranked[shown..] {
            let d = e.rel.split_once('/').map_or(".", |(d, _)| d);
            *dirs.entry(d).or_default() += 1;
        }
        let mut dirs: Vec<(&str, usize)> = dirs.into_iter().collect();
        dirs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let list: Vec<String> = dirs.iter().take(6).map(|(d, n)| if *d == "." { format!("{n} at the top") } else { format!("{n} in {d}/") }).collect();
        out.push_str(&format!("- … {rest} more files: {}\n", list.join(", ")));
    }
    out
}

/// Files changed in git (relative to the project folder), newest state.
fn git_changed(root: &Path) -> Vec<String> {
    crate::gitsnap::git(root, &["status", "--porcelain", "--untracked-files=all", "--", "."])
        .map(|s| {
            s.lines()
                .filter_map(|l| l.get(3..))
                .map(|p| p.rsplit(" -> ").next().unwrap_or(p).trim_matches('"').to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The map for chat `key`: built the first time, then the same text for
/// the rest of the chat (or until the app restarts).
pub fn for_chat(key: &str, root: &Path, state_dir: &Path, request: &str, budget_tokens: usize) -> String {
    static CHATS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let chats = CHATS.get_or_init(Default::default);
    if budget_tokens == 0 {
        return String::new();
    }
    if let Some(m) = chats.lock().unwrap_or_else(|e| e.into_inner()).get(key) {
        return m.clone();
    }
    let map = render(&scan(root, state_dir), request, &git_changed(root), budget_tokens);
    chats.lock().unwrap_or_else(|e| e.into_inner()).insert(key.to_string(), map.clone());
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ocrs-map-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn outlines_common_languages() {
        let rs = "use x;\npub struct App {\n}\nimpl App {\n    pub async fn run(&self) {}\n}\nfn helper() {}\n#[cfg(test)]\nmod tests { fn t() {} }\n";
        let names: Vec<String> = outline(Path::new("a.rs"), rs).into_iter().map(|p| format!("{} {}", p.kind, p.name)).collect();
        assert_eq!(names, ["struct App", "impl App", "fn run", "fn helper"], "tests are left out");
        let py = "class Shop:\n    def add(self):\n        def inner(): pass\nasync def main():\n";
        let names: Vec<String> = outline(Path::new("a.py"), py).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["Shop", "add", "main"], "nested helpers are left out");
        let js = "export function render(d) {\nconst toggle = () => {}\nexport default class Menu {\n  open() {\n    if (x) {\n  }\n}\nexport interface Props {}\n";
        let names: Vec<String> = outline(Path::new("a.ts"), js).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["render", "toggle", "Menu", "open", "Props"]);
        let html = "<title>Bakery</title>\n<nav id=\"top\"></nav><section id=\"menu\">";
        let names: Vec<String> = outline(Path::new("index.html"), html).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["Bakery", "#top", "#menu"]);
        let go = "func (s *Server) Start() {}\ntype Config struct {\n";
        assert_eq!(outline(Path::new("a.go"), go).len(), 2);
    }

    #[test]
    fn name_pieces_split_snake_and_camel_case() {
        assert_eq!(name_pieces("start_heartbeat"), ["start_heartbeat", "start", "heartbeat"]);
        assert_eq!(name_pieces("parseHTTPRequest"), ["parsehttprequest", "parse", "httprequest"]);
    }

    #[test]
    fn outline_text_has_line_numbers() {
        let t = outline_text("src/a.rs", "fn one() {}\n\nfn two() {}\n", Path::new("a.rs"));
        assert!(t.contains("3 lines, 2 parts"), "{t}");
        assert!(t.contains("     3  fn two"), "{t}");
        assert!(outline_text("a.css", "body{}", Path::new("a.css")).contains("no outline"));
    }

    #[test]
    fn scan_skips_ignored_folders_and_reuses_the_cache() {
        let root = temp("scan");
        let state = temp("scan-state");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/x")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("node_modules/x/index.js"), "function big() {}\n").unwrap();
        std::fs::write(root.join("logo.png"), [0u8, 1, 2]).unwrap();
        let first = scan(&root, &state);
        assert_eq!(first.iter().map(|e| e.rel.as_str()).collect::<Vec<_>>(), ["src/main.rs"]);
        // Same file again: read from the cache, not from disk.
        let cache = cache_path(&state, &root);
        let mut cached: Vec<FileEntry> = serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
        cached[0].parts[0].name = "from_cache".into();
        std::fs::write(&cache, serde_json::to_vec(&cached).unwrap()).unwrap();
        assert_eq!(scan(&root, &state)[0].parts[0].name, "from_cache");
        // A changed file is read again.
        std::fs::write(root.join("src/main.rs"), "fn main() {}\nfn more() {}\n").unwrap();
        let again = scan(&root, &state);
        assert_eq!(again[0].parts.len(), 2);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn render_puts_named_files_first_and_stays_in_budget() {
        let e = |rel: &str, parts: &[&str]| FileEntry {
            rel: rel.into(),
            size: 1,
            mtime: 1,
            parts: parts.iter().enumerate().map(|(i, n)| Part { line: i + 1, kind: "fn".into(), name: (*n).into(), top: true }).collect(),
        };
        let mut entries = vec![e("src/lib.rs", &["start"]), e("src/billing/invoice.rs", &["total", "tax"]), e("docs/notes.md", &[])];
        for i in 0..3000 {
            entries.push(e(&format!("gen/file{i}.rs"), &["a", "b", "c"]));
        }
        let map = render(&entries, "fix the tax rounding in invoice.rs", &[], 1000);
        let first = map.lines().find(|l| l.starts_with("- ")).unwrap();
        assert!(first.starts_with("- src/billing/invoice.rs: tax(), total()"), "the named part leads: {map}");
        assert!(map.len() <= 3500, "within ~1000 tokens: {} chars", map.len());
        assert!(map.contains("more files:") && map.contains("in gen/"), "{map}");
        assert_eq!(render(&entries, "x", &[], 0), "", "0 turns the map off");
        // A piece of a name counts: "heartbeat" finds start_heartbeat.
        entries.push(e("src/server.rs", &["note", "start_heartbeat"]));
        let map = render(&entries, "the heartbeat says offline", &[], 1000);
        let first = map.lines().find(|l| l.starts_with("- ")).unwrap();
        assert!(first.starts_with("- src/server.rs: start_heartbeat(), note()"), "named part first: {map}");
        assert_eq!(render(&[], "x", &[], 1000), "");
    }
}
