//! Attachments: files and pasted screenshots the user adds to a message.
//! They are copied into `<project>/.opencode/attachments/` (git-ignored), so
//! the agent's file tools can read them, and named in the message as
//! `[file: <path>]` / `[image: <path>]`. For a model that can see images,
//! the images of the latest user message are sent along (see
//! `provider::Client::stream`).

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

/// Largest file that is copied in.
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Largest image sent to the model (base64 makes it a third bigger).
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

pub fn dir(workspace: &Path) -> PathBuf {
    workspace.join(".opencode").join("attachments")
}

fn ready_dir(workspace: &Path) -> Result<PathBuf> {
    let d = dir(workspace);
    std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
    let ignore = d.join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(ignore, "*\n");
    }
    Ok(d)
}

/// A file name that is safe on every OS, prefixed with a time stamp.
/// `n` > 0 adds a number after the stamp, for a name already taken.
fn safe_name(name: &str, n: u32) -> String {
    let base = Path::new(name).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let clean: String = base
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .take(80)
        .collect();
    let clean = if clean.trim_matches(['.', '_']).is_empty() { "file".to_string() } else { clean };
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    if n == 0 {
        format!("{stamp}-{clean}")
    } else {
        format!("{stamp}-{n}-{clean}")
    }
}

/// Create a new attachment file for `name`. Two pastes of the same name in
/// the same millisecond must not overwrite each other: the file is created
/// only if it does not exist yet, else the next number is tried.
fn create_unique(workspace: &Path, name: &str) -> Result<(PathBuf, std::fs::File)> {
    let dir = ready_dir(workspace)?;
    for n in 0..1000 {
        let path = dir.join(safe_name(name, n));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(anyhow::Error::new(e).context(format!("creating {}", path.display()))),
        }
    }
    Err(anyhow!("no free attachment name for {name}"))
}

/// Save bytes (a pasted or dropped file) as an attachment.
pub fn save_bytes(workspace: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    use std::io::Write;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(anyhow!("{name} is larger than {} MB", MAX_BYTES / 1024 / 1024));
    }
    let (path, mut file) = create_unique(workspace, name)?;
    file.write_all(bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Copy a picked file in.
pub fn copy_in(workspace: &Path, src: &Path) -> Result<PathBuf> {
    let size = std::fs::metadata(src).with_context(|| format!("reading {}", src.display()))?.len();
    if size > MAX_BYTES {
        return Err(anyhow!("{} is larger than {} MB", src.display(), MAX_BYTES / 1024 / 1024));
    }
    let mut from = std::fs::File::open(src).with_context(|| format!("reading {}", src.display()))?;
    let (path, mut file) = create_unique(workspace, &src.to_string_lossy())?;
    std::io::copy(&mut from, &mut file).with_context(|| format!("copying {}", src.display()))?;
    Ok(path)
}

pub fn is_image(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    )
}

/// How an attachment is named in the message text.
pub fn marker(path: &Path) -> String {
    let kind = if is_image(path) { "image" } else { "file" };
    format!("[{kind}: {}]", path.display())
}

/// The image paths named in `text` (`[image: …]`).
pub fn image_paths(text: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("[image: ") {
        let after = &rest[i + 8..];
        let Some(j) = after.find(']') else { break };
        out.push(PathBuf::from(after[..j].trim()));
        rest = &after[j + 1..];
    }
    out
}

/// Is `path` (already canonical) in a folder Habi itself writes pictures
/// to? Any `[image: …]` marker in a tool result or message is otherwise
/// trusted text: `[image: ~/.ssh/id_rsa.png]` in a fetched page must not
/// send that file to the provider. The folders: a project's attachments
/// (`.opencode/attachments`), `check_page`'s screenshots
/// (`.opencode/scratch/shots`) and the desktop app's captures (the temp
/// folder's `opencode-rust-screenshots`).
fn in_picture_dir(path: &Path) -> bool {
    let Some(parent) = path.parent() else { return false };
    let names: Vec<String> = parent
        .components()
        .rev()
        .take(3)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        // Windows folder names ignore case.
        .map(|n| if cfg!(windows) { n.to_ascii_lowercase() } else { n })
        .collect();
    let ends = |tail: &[&str]| tail.iter().enumerate().all(|(i, n)| names.get(i).is_some_and(|x| x == n));
    if ends(&["attachments", ".opencode"]) || ends(&["shots", "scratch", ".opencode"]) {
        return true;
    }
    std::env::temp_dir()
        .join("opencode-rust-screenshots")
        .canonicalize()
        .is_ok_and(|shots| shots.as_path() == parent)
}

/// The image type from the file's first bytes (PNG, JPEG, GIF, WebP).
fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// `data:` URL of an image file for the model, or `None`: missing, too
/// big, not a picture Habi saved (see `in_picture_dir`), or not really an
/// image (the extension and the first bytes must both say so).
pub fn data_url(path: &Path) -> Option<String> {
    use base64::Engine;
    use std::io::Read;
    // Canonical first: a link in the attachments folder to another file
    // is judged by where it leads.
    let path = path.canonicalize().ok()?;
    if !is_image(&path) || !in_picture_dir(&path) {
        return None;
    }
    // Read at most one byte past the limit instead of trusting the size
    // the metadata reported a moment ago.
    let mut bytes = Vec::new();
    std::fs::File::open(&path).ok()?.take(MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_IMAGE_BYTES {
        return None;
    }
    let mime = sniff_mime(&bytes)?;
    Some(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

/// Can `model` take images? `setting` is `provider.images`: "on", "off",
/// or "auto" (guess from the model name).
pub fn model_sees_images(setting: &str, model: &str) -> bool {
    match setting.trim().to_ascii_lowercase().as_str() {
        "on" => true,
        "off" => false,
        _ => {
            let m = model.to_ascii_lowercase();
            ["vl", "vision", "gpt-4o", "gpt-4.1", "gpt-5", "gpt-6", "gpt-7", "claude", "gemini", "llava", "pixtral", "gemma-3", "omni"]
                .iter()
                .any(|k| m.contains(k))
        }
    }
}

/// Turn the last user message of an OpenAI-style request into text + image
/// parts when it names images. Earlier messages keep only the text: images
/// are not resent with every later request.
pub fn attach_images(body: &mut serde_json::Value) {
    let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else { return };
    // Screenshots from the latest tool results (`check_page` with look,
    // `browser_screenshot`): a tool message cannot carry an image, so they
    // follow as a user note. Only the latest results: older pictures are not
    // resent with every request.
    let last_assistant = msgs.iter().rposition(|m| m["role"] == "assistant");
    let shots: Vec<String> = match last_assistant {
        Some(i) => msgs[i + 1..]
            .iter()
            .filter(|m| m["role"] == "tool")
            .filter_map(|m| m["content"].as_str())
            .flat_map(image_paths)
            .filter_map(|p| data_url(&p))
            .collect(),
        None => Vec::new(),
    };
    if !shots.is_empty() {
        let mut parts = vec![serde_json::json!({
            "type": "text",
            "text": "[app] The screenshots from the tool results above, so you can see them:"
        })];
        parts.extend(shots.into_iter().map(|url| serde_json::json!({ "type": "image_url", "image_url": { "url": url } })));
        msgs.push(serde_json::json!({ "role": "user", "content": parts }));
        return;
    }
    // The user's own message: not an `[app]` note (a server without a system
    // role gets the instructions as one at the end, found by the audit).
    let Some(last) = msgs
        .iter_mut()
        .rev()
        .find(|m| m["role"] == "user" && !m["content"].as_str().is_some_and(|t| t.starts_with("[app]")))
    else {
        return;
    };
    let Some(text) = last["content"].as_str().map(str::to_string) else { return };
    let images: Vec<String> = image_paths(&text).iter().filter_map(|p| data_url(p)).collect();
    if images.is_empty() {
        return;
    }
    let mut parts = vec![serde_json::json!({ "type": "text", "text": text })];
    for url in images {
        parts.push(serde_json::json!({ "type": "image_url", "image_url": { "url": url } }));
    }
    last["content"] = serde_json::Value::Array(parts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachments_are_copied_in_named_and_sent_as_images() {
        let ws = std::env::temp_dir().join(format!("ocrs-attach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        // A 1x1 PNG.
        let png: &[u8] = &[137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 31, 21, 196, 137];
        let shot = save_bytes(&ws, "my shot?.png", png).unwrap();
        assert!(shot.starts_with(dir(&ws)) && shot.file_name().unwrap().to_string_lossy().ends_with("my_shot_.png"));
        assert_eq!(std::fs::read_to_string(dir(&ws).join(".gitignore")).unwrap(), "*\n");
        let m = marker(&shot);
        assert!(m.starts_with("[image: "));
        let text = format!("why is this broken?\n{m}\n[file: {}]", ws.join("notes.txt").display());
        assert_eq!(image_paths(&text), vec![shot.clone()]);

        let mut body = serde_json::json!({"messages": [
            {"role": "user", "content": format!("old {m}")},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": text}
        ]});
        attach_images(&mut body);
        let last = &body["messages"][2]["content"];
        assert_eq!(last[1]["type"], "image_url");
        assert!(last[1]["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,iVBOR"));
        assert!(body["messages"][0]["content"].is_string(), "older messages stay text");

        assert!(model_sees_images("auto", "nvidia/nemotron-3.5-super-vl:free"));
        assert!(model_sees_images("auto", "gpt-6-sol"), "the gpt-6 models take images");
        assert!(!model_sees_images("auto", "deepseek/deepseek-chat"));
        assert!(!model_sees_images("off", "gpt-4o"));

        // A screenshot named in the latest tool result reaches the model as
        // an image in a user note (a tool message cannot carry one).
        let mut body = serde_json::json!({"messages": [
            {"role": "user", "content": "build the page"},
            {"role": "assistant", "content": null, "tool_calls": [{"id": "c1"}]},
            {"role": "tool", "tool_call_id": "c1", "content": format!("check_page: ok\n[image: {}]", shot.display())}
        ]});
        attach_images(&mut body);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4, "a note with the picture follows the tool result");
        assert_eq!(msgs[3]["role"], "user");
        assert_eq!(msgs[3]["content"][1]["type"], "image_url");
        assert!(msgs[2]["content"].is_string(), "the tool result itself stays text");
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// T-079: only the file name is kept, made safe, capped at 80 chars.
    #[test]
    fn safe_names() {
        let rest = |n: &str| safe_name(n, 0).split_once('-').unwrap().1.to_string();
        assert_eq!(rest("some/dir\\deep/notes.txt"), "notes.txt");
        assert_eq!(rest("héllo wörld.png"), "héllo_wörld.png", "letters of any script stay");
        assert_eq!(rest("..."), "file");
        assert_eq!(rest("a?*:"), "a___");
        let long = format!("{}.txt", "x".repeat(200));
        assert_eq!(rest(&long).chars().count(), 80);
        let numbered = safe_name("a.txt", 3);
        assert!(numbered.ends_with("-3-a.txt"), "{numbered}");
    }

    /// T-033: `provider.images` ignores letter case.
    #[test]
    fn the_images_setting_ignores_case() {
        assert!(model_sees_images("ON", "deepseek/deepseek-chat"));
        assert!(model_sees_images(" On ", "deepseek/deepseek-chat"));
        assert!(!model_sees_images("OFF", "gpt-4o"));
    }

    /// T-036: the same name saved twice at once gives two files.
    #[test]
    fn the_same_name_twice_gives_two_files() {
        let ws = std::env::temp_dir().join(format!("ocrs-attach-twice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        let paths: Vec<PathBuf> = (0..20).map(|i| save_bytes(&ws, "paste.png", &[i]).unwrap()).collect();
        let unique: std::collections::HashSet<&PathBuf> = paths.iter().collect();
        assert_eq!(unique.len(), paths.len(), "{paths:?}");
        for (i, p) in paths.iter().enumerate() {
            assert_eq!(std::fs::read(p).unwrap(), [i as u8], "nothing was overwritten");
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// An `[image: …]` marker is text anyone can write (a fetched page, a
    /// file the agent read): only pictures in Habi's own folders, that
    /// really are images, are sent to the provider.
    #[test]
    fn only_real_images_from_habis_folders_are_sent() {
        let ws = std::env::temp_dir().join(format!("ocrs-attach-only-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        let png: &[u8] = &[137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82];
        let inside = save_bytes(&ws, "ok.png", png).unwrap();
        assert!(data_url(&inside).unwrap().starts_with("data:image/png;base64,"));
        // A real PNG anywhere else (a secret named .png, say).
        let outside = ws.join("secret.png");
        std::fs::write(&outside, png).unwrap();
        assert!(data_url(&outside).is_none(), "outside the picture folders");
        // `..` back out of the attachments folder does not count either.
        let sneaky = dir(&ws).join("..").join("..").join("secret.png");
        assert!(data_url(&sneaky).is_none());
        // Text named .png in the attachments folder: not an image.
        let fake = save_bytes(&ws, "key.png", b"-----BEGIN OPENSSH PRIVATE KEY-----").unwrap();
        assert!(data_url(&fake).is_none(), "the first bytes must be an image");
        // A non-image extension, even with image bytes.
        let txt = save_bytes(&ws, "x.txt", png).unwrap();
        assert!(data_url(&txt).is_none());
        // check_page's screenshots are allowed.
        let shots = ws.join(".opencode").join("scratch").join("shots");
        std::fs::create_dir_all(&shots).unwrap();
        std::fs::write(shots.join("desktop.png"), png).unwrap();
        assert!(data_url(&shots.join("desktop.png")).is_some());
        assert_eq!(sniff_mime(b"GIF89a.."), Some("image/gif"));
        assert_eq!(sniff_mime(b"RIFF....WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_mime(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        let _ = std::fs::remove_dir_all(&ws);
    }
}
