//! Native pieces outside the webview: Windows dialogs, the close prompt,
//! notifications, the one-window lock and the fatal-error box.

use super::*;

/// Ask for a folder with the system's folder picker (Windows). Elsewhere
/// the page asks for a path and sends it with `add_project`.
#[cfg(windows)]
pub(super) fn pick_folder(window: &tao::window::Window) -> Option<std::path::PathBuf> {
    use tao::platform::windows::WindowExtWindows;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
    };
    // SAFETY: plain COM calls on the UI thread; the returned string is
    // copied and then freed with CoTaskMemFree as the API requires.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        dialog.Show(Some(HWND(window.hwnd() as _))).ok()?; // Err = cancelled
        let item = dialog.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        path.map(std::path::PathBuf::from)
    }
}

#[cfg(not(windows))]
pub(super) fn pick_folder(_window: &tao::window::Window) -> Option<std::path::PathBuf> {
    None
}

/// Files picked in the Windows open dialog (several allowed).
#[cfg(windows)]
pub(super) fn pick_files(window: &tao::window::Window) -> Vec<std::path::PathBuf> {
    use tao::platform::windows::WindowExtWindows;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM,
        SIGDN_FILESYSPATH,
    };
    let mut out = Vec::new();
    // SAFETY: plain COM calls on the UI thread; each returned string is
    // copied and then freed with CoTaskMemFree as the API requires.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let Ok(dialog) = CoCreateInstance::<_, IFileOpenDialog>(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) else {
            return out;
        };
        let Ok(options) = dialog.GetOptions() else { return out };
        if dialog.SetOptions(options | FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM).is_err() {
            return out;
        }
        if dialog.Show(Some(HWND(window.hwnd() as _))).is_err() {
            return out; // cancelled
        }
        let Ok(items) = dialog.GetResults() else { return out };
        let n = items.GetCount().unwrap_or(0);
        for i in 0..n {
            let Ok(item) = items.GetItemAt(i) else { continue };
            let Ok(name) = item.GetDisplayName(SIGDN_FILESYSPATH) else { continue };
            if let Ok(p) = name.to_string() {
                out.push(std::path::PathBuf::from(p));
            }
            CoTaskMemFree(Some(name.0 as *const _));
        }
    }
    out
}

#[cfg(not(windows))]
pub(super) fn pick_files(_window: &tao::window::Window) -> Vec<std::path::PathBuf> {
    Vec::new()
}

/// Closing stops every run: ask first while one is going.
#[cfg(windows)]
pub(super) fn confirm_close(window: &tao::window::Window, gs: &GuiState) -> bool {
    use tao::platform::windows::WindowExtWindows;
    use windows::core::w;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_ICONWARNING, MB_YESNO};
    if !gs.busy && gs.running.is_empty() {
        return true;
    }
    // SAFETY: a modal message box owned by our own window, on the UI thread.
    let answer = unsafe {
        MessageBoxW(
            Some(HWND(window.hwnd() as _)),
            w!("An agent is still working. Close anyway? Its run will stop."),
            w!("Habi"),
            MB_YESNO | MB_ICONWARNING,
        )
    };
    answer == IDYES
}

#[cfg(not(windows))]
pub(super) fn confirm_close(_window: &tao::window::Window, _gs: &GuiState) -> bool {
    true
}

/// A Windows notification (a tray balloon, shown as a toast on Windows 10
/// and 11). The tray icon is added on first use and removed by
/// `notify_cleanup` when the app exits.
#[cfg(windows)]
pub(super) fn notify(window: &tao::window::Window, title: &str, body: &str) {
    use tao::platform::windows::WindowExtWindows;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_MODIFY, NOTIFYICONDATAW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{LoadIconW, IDI_APPLICATION};
    fn fill(dst: &mut [u16], s: &str) {
        let n = dst.len() - 1;
        for (d, c) in dst.iter_mut().zip(s.encode_utf16().take(n)) {
            *d = c;
        }
    }
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: HWND(window.hwnd() as _),
        uID: 1,
        uFlags: NIF_ICON | NIF_TIP | NIF_INFO,
        dwInfoFlags: NIIF_INFO,
        ..Default::default()
    };
    // SAFETY: a stock system icon; no module handle involved.
    nid.hIcon = unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default();
    fill(&mut nid.szTip, "Habi");
    fill(&mut nid.szInfoTitle, title);
    fill(&mut nid.szInfo, body);
    let first = !TRAY_ADDED.swap(true, std::sync::atomic::Ordering::SeqCst);
    // SAFETY: `nid` is fully initialised and outlives the call.
    unsafe {
        let _ = Shell_NotifyIconW(if first { NIM_ADD } else { NIM_MODIFY }, &nid);
    }
}

#[cfg(windows)]
pub(super) static TRAY_ADDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(windows)]
pub(super) fn notify_cleanup(window: &tao::window::Window) {
    use tao::platform::windows::WindowExtWindows;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{Shell_NotifyIconW, NIM_DELETE, NOTIFYICONDATAW};
    if !TRAY_ADDED.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: HWND(window.hwnd() as _),
        uID: 1,
        ..Default::default()
    };
    // SAFETY: removes the icon `notify` added for this window.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

#[cfg(not(windows))]
pub(super) fn notify(_window: &tao::window::Window, _title: &str, _body: &str) {}

#[cfg(not(windows))]
pub(super) fn notify_cleanup(_window: &tao::window::Window) {}

/// New (F-13): hold an exclusive lock on `<state_dir>/gui.lock` while the
/// window is open. The OS drops it when the process ends, even after a crash.
/// Needs Rust 1.89 or newer (`File::try_lock`).
pub(super) fn single_instance_lock(state_dir: &std::path::Path) -> std::result::Result<std::fs::File, String> {
    let _ = std::fs::create_dir_all(state_dir);
    let path = state_dir.join("gui.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            Err("Another Habi window is already open. Use that one, or close it first.".to_string())
        }
        Err(std::fs::TryLockError::Error(e)) => Err(format!("cannot lock {}: {e}", path.display())),
    }
}

/// New (F-13): show a fatal error to a user who may have no console window.
pub fn show_fatal_dialog(title: &str, msg: &str) {
    gui_log(&format!("[gui] {title}: {msg}"));
    #[cfg(windows)]
    {
        use windows::core::HSTRING;
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
        // SAFETY: a plain Win32 call with valid wide strings that outlive it.
        unsafe {
            MessageBoxW(None, &HSTRING::from(msg), &HSTRING::from(title), MB_OK | MB_ICONERROR);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F-13: a second window on the same state directory is refused.
    #[test]
    fn only_one_window_per_state_dir() {
        let dir = std::env::temp_dir().join(format!("habi-lock-test-{}", std::process::id()));
        let first = single_instance_lock(&dir).expect("the first window gets the lock");
        assert!(single_instance_lock(&dir).is_err(), "a second window is refused");
        drop(first);
        assert!(single_instance_lock(&dir).is_ok(), "free again after the first closes");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
