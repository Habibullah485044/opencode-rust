use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Root configuration, loaded from TOML. Where values come from, strongest
/// first:
/// 1. env `OPENCODE_RUST_API_KEY`, `OPENCODE_RUST_BASE_URL`, `OPENCODE_RUST_MODEL`
///    (each only when not empty);
/// 2. the config file: `$OPENCODE_RUST_CONFIG` when set (it must exist),
///    otherwise `~/.config/opencode-rust/config.toml` (skipped when missing);
/// 3. built-in defaults.
/// Unknown keys in the file are reported on stderr and ignored.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct Config {
    pub provider: ProviderCfg,
    pub permissions: PermissionsCfg,
    pub server: ServerCfg,
    pub agent: AgentCfg,
    pub mcp: McpCfg,
    pub browser: BrowserCfg,
}

/// Goal A: the embedded preview panel.
///
/// ```toml
/// [browser]
/// enabled = true
/// home_url = "http://localhost:5173"
/// allow_remote = true          # false = dev-server-only surface
/// profile_dir = "C:/profiles/agent"   # reserved for C2
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct BrowserCfg {
    /// Master switch for the native child webview. Off = chrome only.
    pub enabled: bool,
    /// Page offered when the panel opens with nothing loaded.
    pub home_url: String,
    /// Whether non-loopback URLs may load. `false` makes the panel a
    /// dev-server-only surface (the safest configuration).
    pub allow_remote: bool,
    /// Reserved for Goal C2: a dedicated WebView2 user-data directory.
    pub profile_dir: Option<String>,
}

impl Default for BrowserCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            home_url: String::new(),
            allow_remote: true,
            profile_dir: None,
        }
    }
}

/// M2: MCP servers to launch over stdio at boot.
///
/// ```toml
/// [[mcp.servers]]
/// name = "files"
/// command = "node"
/// args = ["/path/to/server.js"]
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct McpCfg {
    pub servers: Vec<McpServerCfg>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct McpServerCfg {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// F-03: Streamable HTTP endpoint, e.g. "https://example.com/mcp".
    /// When set, `command` and `args` are ignored and nothing is spawned.
    pub url: Option<String>,
    /// F-03: extra HTTP headers for `url`, e.g. an Authorization token.
    pub headers: HashMap<String, String>,
}

impl Default for McpServerCfg {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            url: None,
            headers: HashMap::new(),
        }
    }
}

/// Which wire protocol the provider speaks (`provider.api`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderApi {
    /// OpenAI-compatible `POST {base_url}/chat/completions`. The default.
    #[default]
    OpenAi,
    /// Anthropic Messages API: `POST {base_url}/messages`.
    Anthropic,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ProviderCfg {
    /// OpenAI-compatible base, e.g. "https://xyz.trycloudflare.com/v1" or "http://127.0.0.1:11434/v1"
    pub base_url: String,
    /// Can also come from $OPENCODE_RUST_API_KEY (never stored in the file if set there).
    pub api_key: String,
    pub model: String,
    /// "openai" (default) or "anthropic".
    pub api: ProviderApi,
    /// Longest reply in tokens. Anthropic requires it; "openai" ignores it.
    pub max_tokens: u32,
    /// Optional cheaper model for hidden tasks (compaction, planning).
    pub small_model: Option<String>,
    pub request_timeout_secs: u64,
    /// Extra tries for a failed provider request (connection error,
    /// HTTP 429/500/502/503/504/529). 0 = no retries.
    pub max_retries: u32,
    /// How long to keep waiting out a rate limit (HTTP 429) before a
    /// request fails, in seconds. Every agent of the app waits together, so
    /// parallel agents do not keep hitting the limit. 0 = treat 429 like
    /// any other retryable error (`max_retries`).
    pub rate_limit_wait_secs: u64,
    /// How hard a reasoning model thinks before it answers: "low",
    /// "medium" or "high" (OpenAI-style servers; OpenRouter passes it on).
    /// Thinking is billed as output. Unset = the model's default.
    pub reasoning_effort: Option<String>,
    /// Prompt caching markers: "auto" (default), "on" or "off". Most
    /// providers (OpenAI, DeepSeek, Gemini, Ollama, llama.cpp) reuse a
    /// repeated prompt start on their own; Anthropic models need markers
    /// that say where to cache. "auto" sends them for `api = "anthropic"`
    /// and for Claude models behind an OpenAI-style router (OpenRouter).
    pub prompt_cache: PromptCache,
    /// Ask OpenAI-style servers for token counts at the end of each reply
    /// (`stream_options.include_usage`), so `/usage` can show cache hits.
    /// Turn off for a server that rejects the field.
    pub stream_usage: bool,
}

/// `provider.prompt_cache`: when to send prompt-caching markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptCache {
    /// Markers for Anthropic's API and Claude models on routers.
    #[default]
    Auto,
    /// Markers on every request (for a router or proxy that supports them).
    On,
    /// No markers. Providers that cache on their own still do.
    Off,
}

impl Default for ProviderCfg {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            api: ProviderApi::OpenAi,
            max_tokens: 8192,
            small_model: None,
            request_timeout_secs: 600,
            max_retries: 3,
            rate_limit_wait_secs: 300,
            reasoning_effort: None,
            prompt_cache: PromptCache::Auto,
            stream_usage: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PermissionsCfg {
    /// Per-tool default action when no rule matches: "allow" | "ask" | "deny".
    /// Tools you do not list keep their built-in default (see
    /// `builtin_permission_defaults`); a listed tool overrides it.
    #[serde(deserialize_with = "merge_with_builtin_defaults")]
    pub defaults: HashMap<String, String>,
    /// Ordered rules; first match wins.
    /// `tool` supports `*` wildcards; `pattern` is glob-matched against the
    /// detail (command for bash, path for file tools, url for webfetch).
    pub rules: Vec<RuleCfg>,
    /// Seconds an approval may wait before being auto-denied (serve mode safety).
    pub approval_timeout_secs: u64,
}

impl Default for PermissionsCfg {
    fn default() -> Self {
        let mut defaults = HashMap::new();
        // Safe reads: allow. Anything that mutates the machine: ask.
        for t in ["read", "list", "glob", "grep", "check_page"] {
            defaults.insert(t.to_string(), "allow".to_string());
        }
        for t in ["bash", "write", "edit", "webfetch"] {
            defaults.insert(t.to_string(), "ask".to_string());
        }
        // Goal B: reading the page and waiting are safe; clicking and typing
        // can submit forms, so they go through the approval modal (D1).
        for t in ["browser_navigate", "browser_snapshot", "browser_wait", "browser_screenshot"] {
            defaults.insert(t.to_string(), "allow".to_string());
        }
        for t in ["browser_click", "browser_type"] {
            defaults.insert(t.to_string(), "ask".to_string());
        }
        // Starting and managing subagents is safe by itself: each child still
        // asks before its own writes and commands.
        for t in ["spawn", "agent"] {
            defaults.insert(t.to_string(), "allow".to_string());
        }
        Self { defaults, rules: Vec::new(), approval_timeout_secs: 300 }
    }
}

/// The defaults every config starts from (the list in
/// `impl Default for PermissionsCfg`). A user's `[permissions.defaults]`
/// table is merged on top of this list; it does not replace it.
pub fn builtin_permission_defaults() -> HashMap<String, String> {
    PermissionsCfg::default().defaults
}

/// serde helper for `PermissionsCfg::defaults`: user entries on top of the
/// built-in ones.
fn merge_with_builtin_defaults<'de, D>(d: D) -> std::result::Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let user = HashMap::<String, String>::deserialize(d)?;
    let mut merged = builtin_permission_defaults();
    merged.extend(user);
    Ok(merged)
}

impl PermissionsCfg {
    /// Every action word must be "allow", "ask" or "deny" (any letter case).
    /// The error lists every bad entry at once.
    pub fn validate(&self) -> Result<()> {
        let ok = |s: &str| matches!(s.trim().to_ascii_lowercase().as_str(), "allow" | "ask" | "deny");
        let mut bad = Vec::new();
        let mut names: Vec<&String> = self.defaults.keys().collect();
        names.sort();
        for tool in names {
            let action = &self.defaults[tool];
            if !ok(action) {
                bad.push(format!("permissions.defaults.{tool} = {action:?}"));
            }
        }
        for (i, r) in self.rules.iter().enumerate() {
            if !ok(&r.action) {
                bad.push(format!(
                    "permissions.rules[{i}].action = {:?} (tool {:?}, pattern {:?})",
                    r.action, r.tool, r.pattern
                ));
            }
        }
        if bad.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "invalid permission action (use \"allow\", \"ask\" or \"deny\"): {}",
                bad.join("; ")
            )
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RuleCfg {
    pub tool: String,
    pub pattern: String,
    pub action: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerCfg {
    /// Loopback only, by design. 0 = ephemeral port (tests).
    pub port: u16,
}

impl Default for ServerCfg {
    fn default() -> Self { Self { port: 0 } }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentCfg {
    pub max_steps: usize,
    pub system_prompt: Option<String>,
    /// Vendor the Ponytail (lazy senior dev) discipline into every system
    /// prompt: smallest complete change, reuse over rewrite, never cut safety.
    pub ponytail: bool,
    /// Automatically run a ponytail review after runs that changed files.
    pub auto_review: bool,
    /// F-05: give each `worker` subagent and team worker its own git
    /// worktree. Their changes come back as a patch file instead of landing
    /// in the workspace. Off by default.
    pub isolate_workers: bool,
    /// Most subagents one chat runs at the same time; more spawns wait in
    /// line for a free place.
    pub max_subagents: usize,
    /// F-08: command that checks the work after a run that changed files,
    /// for example "cargo test". Unset (the default) = no check.
    pub verify_command: Option<String>,
    /// No `verify_command` set: find the project's check (Cargo.toml →
    /// `cargo check`, package.json test script, pytest tests, go.mod).
    pub auto_verify: bool,
    /// After a run that changed a web page, open it in a headless browser
    /// at phone and desktop width and hand the problems back to the agent.
    pub check_pages: bool,
    /// F-08: how many fix rounds the agent gets when the check fails.
    pub verify_max_rounds: u32,
    /// F-08: seconds before a check that hangs is stopped.
    pub verify_timeout_secs: u64,
    /// Laziness Meter: hint the model when a write duplicates something the
    /// repo already has, and flag overbuilt writes.
    pub reuse_gate: bool,
    /// F-07: the model's context window in tokens. When the next request
    /// would use about 80% of it, older messages are compacted first.
    /// 0 = off (default).
    pub context_tokens: usize,
}

impl Default for AgentCfg {
    fn default() -> Self {
        Self {
            max_steps: 40,
            system_prompt: None,
            ponytail: true,
            isolate_workers: false,
            max_subagents: 4,
            verify_command: None,
            auto_verify: true,
            check_pages: true,
            verify_max_rounds: 2,
            verify_timeout_secs: 600,
            auto_review: true,
            reuse_gate: true,
            context_tokens: 0,
        }
    }
}

impl Config {
    pub fn load(workspace: &Path) -> Result<Self> {
        let explicit = std::env::var("OPENCODE_RUST_CONFIG").ok().filter(|p| !p.trim().is_empty());
        let path = match &explicit {
            Some(p) => PathBuf::from(p),
            None => default_config_path(),
        };
        // A path the user set on purpose must exist; silently using the
        // defaults instead hides the mistake.
        if explicit.is_some() && !path.is_file() {
            anyhow::bail!("OPENCODE_RUST_CONFIG points to {}, which is not a file", path.display());
        }
        let mut cfg = if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading config {}", path.display()))?;
            let cfg: Config =
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            for key in unknown_config_keys(&text, &cfg) {
                eprintln!("warning: {}: unknown key `{key}` (ignored)", path.display());
            }
            cfg
        } else {
            Config::default()
        };
        // Environment overrides — API key should prefer env over file.
        if let Ok(k) = std::env::var("OPENCODE_RUST_API_KEY") {
            if !k.is_empty() { cfg.provider.api_key = k; }
        }
        if let Ok(u) = std::env::var("OPENCODE_RUST_BASE_URL") {
            if !u.is_empty() { cfg.provider.base_url = u; }
        }
        if let Ok(m) = std::env::var("OPENCODE_RUST_MODEL") {
            if !m.is_empty() { cfg.provider.model = m; }
        }
        cfg.permissions
            .validate()
            .with_context(|| format!("checking [permissions] in {}", path.display()))?;
        let _ = workspace; // reserved: project-level .opencode-rust.toml in M2
        Ok(cfg)
    }

    pub fn effective_base_url(&self) -> Result<String> {
        let u = self.provider.base_url.trim().trim_end_matches('/').to_string();
        if u.is_empty() {
            anyhow::bail!(
                "no provider configured — set provider.base_url + provider.model in \
                 ~/.config/opencode-rust/config.toml or OPENCODE_RUST_BASE_URL / OPENCODE_RUST_MODEL"
            );
        }
        Ok(u)
    }
}

/// Keys in the config text that this version does not know (typos, old
/// names), as dotted paths like `provider.modle` or `mcp.servers[0].argz`.
pub fn unknown_config_keys(text: &str, cfg: &Config) -> Vec<String> {
    fn walk(user: &toml::Value, known: &toml::Value, path: &str, out: &mut Vec<String>) {
        match (user, known) {
            (toml::Value::Table(u), toml::Value::Table(k)) => {
                for (key, v) in u {
                    let here = if path.is_empty() { key.clone() } else { format!("{path}.{key}") };
                    match k.get(key) {
                        Some(kv) => walk(v, kv, &here, out),
                        None => out.push(here),
                    }
                }
            }
            (toml::Value::Array(u), toml::Value::Array(k)) => {
                for (i, (uv, kv)) in u.iter().zip(k).enumerate() {
                    walk(uv, kv, &format!("{path}[{i}]"), out);
                }
            }
            _ => {}
        }
    }
    let (Ok(user), Ok(known)) = (text.parse::<toml::Table>(), toml::Value::try_from(cfg)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(&toml::Value::Table(user), &known, "", &mut out);
    out
}

pub fn default_config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".config")
        });
    base.join("opencode-rust").join("config.toml")
}

pub fn default_state_dir() -> PathBuf {
    // Windows: %APPDATA%\opencode-rust; elsewhere XDG data dir.
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata).join("opencode-rust");
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".local").join("share")
        });
    base.join("opencode-rust")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_permissions_ask_on_mutations() {
        let c = Config::default();
        assert_eq!(c.permissions.defaults.get("bash").unwrap(), "ask");
        assert_eq!(c.permissions.defaults.get("read").unwrap(), "allow");
        // Goal B: reading the page is free, acting on it asks.
        assert_eq!(c.permissions.defaults.get("browser_snapshot").unwrap(), "allow");
        assert_eq!(c.permissions.defaults.get("browser_click").unwrap(), "ask");
        assert_eq!(c.permissions.defaults.get("browser_type").unwrap(), "ask");
    }

    #[test]
    fn browser_defaults_are_sane_and_configurable() {
        let c = Config::default();
        assert!(c.browser.enabled, "the panel is the point of Goal A");
        assert!(c.browser.allow_remote, "general browsing by default, dev-only opt-in");
        assert!(c.browser.home_url.is_empty());
        let c: Config = toml::from_str(
            "[browser]\nenabled = false\nhome_url = 'http://localhost:3000'\nallow_remote = false\n",
        )
        .unwrap();
        assert!(!c.browser.enabled);
        assert!(!c.browser.allow_remote);
        assert_eq!(c.browser.home_url, "http://localhost:3000");
    }

    #[test]
    fn empty_base_url_is_error() {
        assert!(Config::default().effective_base_url().is_err());
    }

    #[test]
    fn toml_roundtrip() {
        let text = r#"
            [provider]
            base_url = "https://x.example/v1"
            model = "m1"
            [agent]
            max_steps = 7
            [[mcp.servers]]
            name = "files"
            command = "node"
            args = ["srv.js", "--flag"]
        "#;
        let c: Config = toml::from_str(text).unwrap();
        assert_eq!(c.provider.model, "m1");
        assert_eq!(c.agent.max_steps, 7);
        assert_eq!(c.mcp.servers.len(), 1);
        assert_eq!(c.mcp.servers[0].name, "files");
        assert_eq!(c.mcp.servers[0].command, "node");
        assert_eq!(c.mcp.servers[0].args, vec!["srv.js", "--flag"]);
        // Default config has no MCP servers.
        assert!(Config::default().mcp.servers.is_empty());
    }

    #[test]
    fn user_defaults_merge_with_builtins() {
        let c: Config = toml::from_str("[permissions.defaults]\nbash = \"allow\"\n").unwrap();
        assert_eq!(c.permissions.defaults.get("bash").unwrap(), "allow", "user value wins");
        assert_eq!(c.permissions.defaults.get("read").unwrap(), "allow", "built-in kept");
        assert_eq!(c.permissions.defaults.get("write").unwrap(), "ask", "built-in kept");
    }

    #[test]
    fn invalid_permission_actions_are_rejected() {
        let c: Config = toml::from_str(
            "[permissions.defaults]\nwrite = \"alow\"\n\
             [[permissions.rules]]\ntool = \"bash\"\npattern = \"rm *\"\naction = \"denny\"\n",
        )
        .unwrap();
        let err = c.permissions.validate().unwrap_err().to_string();
        assert!(err.contains("permissions.defaults.write"), "{err}");
        assert!(err.contains("permissions.rules[0]"), "{err}");
        // Letter case does not matter.
        let c: Config = toml::from_str(
            "[[permissions.rules]]\ntool = \"bash\"\npattern = \"rm *\"\naction = \"Deny\"\n",
        )
        .unwrap();
        assert!(c.permissions.validate().is_ok());
    }

    #[test]
    fn automatic_compaction_is_off_by_default() {
        assert_eq!(Config::default().agent.context_tokens, 0);
        let c: Config = toml::from_str("[agent]\ncontext_tokens = 8192\n").unwrap();
        assert_eq!(c.agent.context_tokens, 8192);
    }

    #[test]
    fn unknown_keys_are_reported() {
        let text = "[provider]\nmodel = \"m\"\nmodle = \"typo\"\n\
                    [[mcp.servers]]\nname = \"x\"\ncommand = \"node\"\nargz = []\n\
                    [colour]\nx = 1\n";
        let cfg: Config = toml::from_str(text).unwrap();
        let mut keys = unknown_config_keys(text, &cfg);
        keys.sort();
        assert_eq!(keys, ["colour", "mcp.servers[0].argz", "provider.modle"]);
        let clean = "[provider]\nmodel = \"m\"\nsmall_model = \"s\"\n";
        let cfg: Config = toml::from_str(clean).unwrap();
        assert!(unknown_config_keys(clean, &cfg).is_empty());
    }
}

#[cfg(test)]
mod f01_tests {
    use super::*;

    #[test]
    fn provider_max_retries_defaults_to_3() {
        assert_eq!(Config::default().provider.max_retries, 3);
        let c: Config = toml::from_str("[provider]\nmax_retries = 0\n").unwrap();
        assert_eq!(c.provider.max_retries, 0);
        let c: Config = toml::from_str("[provider]\nmodel = 'm'\n").unwrap();
        assert_eq!(c.provider.max_retries, 3, "a missing key keeps the default");
    }
}

#[cfg(test)]
mod f02_tests {
    use super::*;

    #[test]
    fn provider_api_and_max_tokens() {
        let c = Config::default();
        assert_eq!((c.provider.api, c.provider.max_tokens), (ProviderApi::OpenAi, 8192));
        let c: Config = toml::from_str("[provider]\napi = 'anthropic'\nmax_tokens = 4096\n").unwrap();
        assert_eq!((c.provider.api, c.provider.max_tokens), (ProviderApi::Anthropic, 4096));
        assert!(toml::from_str::<Config>("[provider]\napi = 'gemini'\n").is_err());
    }
}

#[cfg(test)]
mod f03_tests {
    use super::*;

    #[test]
    fn mcp_server_url_and_headers_parse() {
        let text = "[[mcp.servers]]\nname = 'docs'\nurl = 'https://example.com/mcp'\nheaders = { Authorization = 'Bearer t' }\n";
        let c: Config = toml::from_str(text).unwrap();
        let s = &c.mcp.servers[0];
        assert_eq!(s.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(s.headers.get("Authorization").map(String::as_str), Some("Bearer t"));
        assert!(s.command.is_empty());
        assert!(McpServerCfg::default().url.is_none(), "stdio stays the default");
    }
}
