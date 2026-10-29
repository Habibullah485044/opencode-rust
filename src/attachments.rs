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
fn safe_name(name: &str) -> String {
    let base = Path::new(name).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let clean: String = base
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .take(80)
        .collect();
    let clean = if clean.trim_matches(['.', '_']).is_empty() { "file".to_string() } else { clean };
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    format!("{stamp}-{clean}")
}

/// Save bytes (a pasted or dropped file) as an attachment.
pub fn save_bytes(workspace: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    if bytes.len() as u64 > MAX_BYTES {
        return Err(anyhow!("{name} is larger than {} MB", MAX_BYTES / 1024 / 1024));
    }
    let path = ready_dir(workspace)?.join(safe_name(name));
    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Copy a picked file in.
pub fn copy_in(workspace: &Path, src: &Path) -> Result<PathBuf> {
    let size = std::fs::metadata(src).with_context(|| format!("reading {}", src.display()))?.len();
    if size > MAX_BYTES {
        return Err(anyhow!("{} is larger than {} MB", src.display(), MAX_BYTES / 1024 / 1024));
    }
    let path = ready_dir(workspace)?.join(safe_name(&src.to_string_lossy()));
    std::fs::copy(src, &path).with_context(|| format!("copying {}", src.display()))?;
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

/// `data:` URL of an image file for the model, or `None` (missing, too big).
pub fn data_url(path: &Path) -> Option<String> {
    use base64::Engine;
    let size = std::fs::metadata(path).ok()?.len();
    if size == 0 || size > MAX_IMAGE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let mime = match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/png",
    };
    Some(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

/// Can `model` take images? `setting` is `provider.images`: "on", "off",
/// or "auto" (guess from the model name).
pub fn model_sees_images(setting: &str, model: &str) -> bool {
    match setting.trim() {
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
}
