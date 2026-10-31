//! The Settings page (provider, system prompt, temperature, model list) and
//! the composer's autonomy selector.

use super::*;

/// Save the Settings page and build the client it describes. Returns the
/// config path, the new client and whether a key is set.
pub(super) fn save_settings(
    st: &server::AppState,
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    effort: &str,
) -> Result<(std::path::PathBuf, crate::provider::Client, bool)> {
    if base_url.trim().is_empty() || model.trim().is_empty() {
        anyhow::bail!("the provider address and the model are both needed");
    }
    let path = crate::settings::save_provider(base_url, model, effort)?;
    let typed = api_key.map(str::trim).filter(|k| !k.is_empty());
    if let Some(k) = typed {
        crate::settings::save_api_key(k, base_url)?;
    }
    let mut p = st.cfg.provider.clone();
    p.base_url = base_url.trim().to_string();
    p.model = model.trim().to_string();
    p.reasoning_effort = (!effort.trim().is_empty()).then(|| effort.trim().to_string());
    // Same precedence as `Config::load` (environment > typed/file > saved),
    // and the old key does not follow a changed address to another host.
    let origin = crate::config::key_origin(base_url);
    let mut candidates: Vec<(String, String)> = typed.map(|k| (k.to_string(), origin.clone())).into_iter().collect();
    if let Some(o) = &st.cfg.provider.api_key_origin {
        candidates.push((st.cfg.provider.api_key.clone(), o.clone()));
    }
    if let Some((k, o)) = crate::settings::load_api_key() {
        candidates.push((k, o.unwrap_or_else(|| crate::config::key_origin(&st.cfg.provider.base_url))));
    }
    let env_key = std::env::var("OPENCODE_RUST_API_KEY").ok();
    let (key, key_origin, withheld) = crate::config::choose_api_key(base_url, env_key.as_deref(), &candidates);
    p.api_key = key;
    p.api_key_origin = key_origin;
    p.key_withheld_for = withheld;
    let client = crate::provider::Client::from_cfg(p.base_url.trim_end_matches('/'), &p)?;
    Ok((path, client, !p.api_key.is_empty()))
}

impl App {
    /// Settings page: save the provider and switch the live client to it.
    pub(super) fn on_save_settings(
        &mut self,
        base_url: String,
        model: String,
        api_key: Option<String>,
        effort: String,
    ) -> bool {
        let text = match save_settings(&self.st, &base_url, &model, api_key.as_deref(), &effort) {
            Ok((path, mut client, has_key)) => {
                let st2 = self.st.clone();
                self.rt.spawn(async move {
                    let mut live = st2.client.lock().await;
                    // Keep the temperature set at runtime (Settings >
                    // Prompt & Temperature), whichever task runs first.
                    client.set_temperature(live.temperature());
                    *live = client;
                });
                self.gs.settings.base_url = base_url;
                self.gs.settings.model = model;
                self.gs.settings.reasoning_effort = effort;
                self.gs.settings.has_key = has_key;
                self.gs.settings.needs_setup = false;
                format!(
                    "Settings saved to {}. The new model is used from the next message; \
                     a few options (small model, limits) apply after a restart.",
                    path.display()
                )
            }
            Err(e) => format!("Settings not saved: {e:#}"),
        };
        self.gs.push(EntryKind::System, text);
        true
    }

    /// Settings → System prompt.
    pub(super) fn on_save_prompt(&mut self, text: String, replace: bool) -> bool {
        let note = match self.st.set_user_prompt(&text, replace) {
            Ok(_) => {
                self.gs.settings.system_prompt = text.trim().to_string();
                self.gs.settings.prompt_replaces = replace;
                match (text.trim().is_empty(), replace) {
                    (true, _) => "System prompt cleared: Habi's own instructions are used.".to_string(),
                    (false, true) => "Your system prompt is now the only one, with the tool guide after it; Habi's own extra steps (spec, final check, design review, ponytail review) are off. It applies from the next message.".to_string(),
                    (false, false) => "Your instructions are added after Habi's. They apply from the next message.".to_string(),
                }
            }
            Err(e) => format!("System prompt not saved: {e:#}"),
        };
        self.gs.push(EntryKind::System, note);
        true
    }

    /// Settings → Temperature.
    pub(super) fn on_save_temperature(&mut self, t: Option<f32>) -> bool {
        let note = match crate::settings::save_temperature(t) {
            Ok(_) => {
                let st2 = self.st.clone();
                self.rt.spawn(async move {
                    st2.client.lock().await.set_temperature(t);
                });
                self.gs.settings.temperature = t;
                match t {
                    None => "Temperature: the model's default (none sent).".to_string(),
                    Some(t) => format!("Temperature set to {t:.1}. It applies from the next message."),
                }
            }
            Err(e) => format!("Temperature not saved: {e:#}"),
        };
        self.gs.push(EntryKind::System, note);
        true
    }

    /// Settings page: ask the provider which models it has (the answer comes
    /// back as a `ModelInfo` event).
    pub(super) fn on_list_models(&self) {
        let st2 = self.st.clone();
        self.rt.spawn(async move {
            let client = st2.client.lock().await.clone();
            let available = client.list_models().await.unwrap_or_default();
            let _ = st2.events.send(ServerEvent::ModelInfo { current: client.model.clone(), available });
        });
    }

    /// Composer mode selector. The core owns the mode; the page just picks it.
    pub(super) fn on_mode(&mut self, mode: PermissionMode) -> bool {
        let granted = self.st.set_perm_mode(mode);
        self.gs.perm_mode = mode;
        // Switching to turbo answers whatever was blocking,
        // so no modal (shown or queued) may outlive the grant.
        for id in &granted {
            self.gs.finish_approval(id);
        }
        eprintln!(
            "[gui] permission mode -> {} ({} prompt(s) released)",
            mode.as_str(),
            granted.len()
        );
        self.gs.push(EntryKind::System, mode.note());
        true
    }
}
