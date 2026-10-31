//! Process trees: putting a shell in its Job Object and killing a shell
//! with everything it started; waiting on a run's abort flag.

use std::process::Stdio;
use std::time::Duration;

#[cfg(windows)]
use super::winjob;

/// Put a just-started shell in its own Job Object (Windows; see `winjob`).
pub(crate) fn adopt_into_job(child: &tokio::process::Child) {
    #[cfg(windows)]
    if let (Some(pid), Some(handle)) = (child.id(), child.raw_handle()) {
        winjob::adopt(pid, handle);
    }
    #[cfg(not(windows))]
    let _ = child;
}

/// New (B-06): kill a shell and everything it started. Linux/macOS: signal
/// the shell's process group (see `bash`). Windows: end its Job Object, and
/// taskkill walks the tree too (a shell started before its job existed).
pub(crate) async fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(windows)]
    winjob::kill(pid);
    #[cfg(windows)]
    let mut cmd = {
        let mut c = tokio::process::Command::new("taskkill");
        c.args(["/T", "/F", "/PID", &pid.to_string()]);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = tokio::process::Command::new("kill");
        c.args(["-s", "KILL", "--", &format!("-{pid}")]);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let _ = cmd.status().await;
}

/// New (B-06): resolves once `abort` is set (checked every 100 ms).
pub(super) async fn wait_for_abort(abort: &std::sync::atomic::AtomicBool) {
    while !abort.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
