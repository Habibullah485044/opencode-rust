use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Root configuration, loaded from TOML.
/// Precedence: $OPENCODE_RUST_CONFIG file > ~/.config/opencode-rust/config.toml > defaults.
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
}

impl Default for McpServerCfg {
    fn default() -> Self {
        Self { name: String::new(), command: String::new(), args: Vec::new() }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ProviderCfg {
    /// OpenAI-compatible base, e.g. "https://xyz.trycloudflare.com/v1" or "http://127.0.0.1:11434/v1"
    pub base_url: String,
    /// Can also come from $OPENCODE_RUST_API_KEY (never stored in the file if set there).
    pub api_key: String,
    pub model: String,
    /// Optional cheaper model for hidden tasks (compaction, planning).
    pub small_model: Option<String>,
    pub request_timeout_secs: u64,
}

impl Default for ProviderCfg {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            small_model: None,
            request_timeout_secs: 600,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PermissionsCfg {
    /// Per-tool default action when no rule matches: "allow" | "ask" | "deny".
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
        for t in ["read", "list", "glob", "grep"] {
            defaults.insert(t.to_string(), "allow".to_string());
        }
        for t in ["bash", "write", "edit", "webfetch"] {
            defaults.insert(t.to_string(), "ask".to_string());
        }
        // Goal B: reading the page and waiting are safe; clicking and typing
        // can submit forms, so they go through the approval modal (D1).
        for t in ["browser_navigate", "browser_snapshot", "browser_wait"] {
            defaults.insert(t.to_string(), "allow".to_string());
        }
        for t in ["browser_click", "browser_type"] {
            defaults.insert(t.to_string(), "ask".to_string());
        }
        Self { defaults, rules: Vec::new(), approval_timeout_secs: 300 }
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
    /// Laziness Meter: hint the model when a write duplicates something the
    /// repo already has, and flag overbuilt writes.
    pub reuse_gate: bool,
}

impl Default for AgentCfg {
    fn default() -> Self {
        Self {
            max_steps: 40,
            system_prompt: None,
            ponytail: true,
            auto_review: true,
            reuse_gate: true,
        }
    }
}

impl Config {
    pub fn load(workspace: &Path) -> Result<Self> {
        let path = match std::env::var("OPENCODE_RUST_CONFIG") {
            Ok(p) => PathBuf::from(p),
            Err(_) => default_config_path(),
        };
        let mut cfg = if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading config {}", path.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
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
}
