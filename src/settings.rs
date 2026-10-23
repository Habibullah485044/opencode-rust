//! Settings the desktop app saves: the provider (address, model, thinking
//! effort) into the user config file, and the API key apart from it — on
//! Windows encrypted with DPAPI for the signed-in user, so it is not in a
//! plain text file and no other Windows account can read it.

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

/// Where the saved API key lives.
fn key_path() -> PathBuf {
    crate::config::default_state_dir().join("api_key.bin")
}

/// The API key saved from the Settings page, if any.
pub fn load_api_key() -> Option<String> {
    let bytes = std::fs::read(key_path()).ok()?;
    let plain = unprotect(&bytes).ok()?;
    String::from_utf8(plain).ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
}

/// Save (or, with an empty key, remove) the API key.
pub fn save_api_key(key: &str) -> Result<()> {
    let path = key_path();
    if key.trim().is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let sealed = protect(key.trim().as_bytes())?;
    std::fs::write(&path, sealed).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Write the provider fields into the user config file (`[provider]`),
/// keeping every other setting. The old file is kept as `config.toml.bak`.
/// `reasoning_effort` "" removes the setting.
pub fn save_provider(base_url: &str, model: &str, reasoning_effort: &str) -> Result<PathBuf> {
    let path = crate::config::default_config_path();
    save_provider_to(&path, base_url, model, reasoning_effort)?;
    Ok(path)
}

fn save_provider_to(path: &std::path::Path, base_url: &str, model: &str, reasoning_effort: &str) -> Result<()> {
    let path = path.to_path_buf();
    let mut table: toml::Table = match std::fs::read_to_string(&path) {
        Ok(text) => {
            let _ = std::fs::write(path.with_extension("toml.bak"), &text);
            text.parse().with_context(|| format!("{} is not valid TOML; fix it or delete it first", path.display()))?
        }
        Err(_) => toml::Table::new(),
    };
    let provider = table
        .entry("provider")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow!("[provider] in {} is not a table", path.display()))?;
    provider.insert("base_url".into(), toml::Value::String(base_url.trim().to_string()));
    provider.insert("model".into(), toml::Value::String(model.trim().to_string()));
    match reasoning_effort.trim() {
        "" => {
            provider.remove("reasoning_effort");
        }
        e => {
            provider.insert("reasoning_effort".into(), toml::Value::String(e.to_string()));
        }
    }
    // The key belongs in the encrypted store, not in this file.
    provider.remove("api_key");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, toml::to_string_pretty(&table)?).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(all(windows, feature = "gui"))]
fn protect(data: &[u8]) -> Result<Vec<u8>> {
    dpapi(data, true)
}

#[cfg(all(windows, feature = "gui"))]
fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    dpapi(data, false)
}

/// DPAPI round trip for the current Windows user.
#[cfg(all(windows, feature = "gui"))]
fn dpapi(data: &[u8], seal: bool) -> Result<Vec<u8>> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input` points at `data` for the call; Windows allocates
    // `output`, which is copied and then freed with LocalFree as documented.
    unsafe {
        if seal {
            CryptProtectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output)
        } else {
            CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output)
        }
        .map_err(|e| anyhow!("Windows could not {} the key: {e}", if seal { "encrypt" } else { "decrypt" }))?;
        let out = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(output.pbData as *mut _)));
        Ok(out)
    }
}

/// Without DPAPI the key is stored as is, readable only by this user (0600).
#[cfg(not(all(windows, feature = "gui")))]
fn protect(data: &[u8]) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}

#[cfg(not(all(windows, feature = "gui")))]
fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_the_provider_keeps_other_settings_and_drops_a_plain_key() {
        let dir = std::env::temp_dir().join(format!("ocrs-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[provider]
model = \"old\"
api_key = \"plain\"

[agent]
max_steps = 7
").unwrap();
        save_provider_to(&path, "https://openrouter.ai/api/v1", "qwen/qwen3-coder:free", "low").unwrap();
        let cfg: crate::config::Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.provider.model, "qwen/qwen3-coder:free");
        assert_eq!(cfg.provider.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(cfg.provider.api_key, "", "the key never stays in the file");
        assert_eq!(cfg.agent.max_steps, 7, "other settings are kept");
        assert!(path.with_extension("toml.bak").is_file());
        save_provider_to(&path, "https://openrouter.ai/api/v1", "m", "").unwrap();
        let cfg: crate::config::Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.provider.reasoning_effort, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sealed_key_opens_again_and_is_not_plain_text() {
        let sealed = protect(b"sk-or-secret").unwrap();
        assert_eq!(unprotect(&sealed).unwrap(), b"sk-or-secret");
        #[cfg(all(windows, feature = "gui"))]
        assert!(!sealed.windows(6).any(|w| w == b"secret"), "DPAPI output does not contain the key");
    }
}
