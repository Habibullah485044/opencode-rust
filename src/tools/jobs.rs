//! Background commands (dev servers, watchers): `bash {command, background:
//! true}` starts one and returns its id; `bash {job}` returns the output
//! since the last read and whether it still runs; `bash {job, stop: true}`
//! stops it and everything it started.

use super::process::{adopt_into_job, kill_tree};
use super::shell::{scrub_secrets, shell_command};
use super::truncate_middle;
#[cfg(windows)]
use super::winjob;
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};

/// Most output kept per job (older output is dropped).
const KEEP: usize = 1_000_000;

struct Job {
    command: String,
    /// Started from the desktop terminal panel (not by the agent).
    by_user: bool,
    cwd: PathBuf,
    pid: Option<u32>,
    out: Arc<Mutex<Vec<u8>>>,
    /// Bytes dropped from the front of `out` so far.
    dropped: Arc<Mutex<usize>>,
    read_to: usize,
    exit: Arc<Mutex<Option<String>>>,
}

fn jobs() -> &'static Mutex<HashMap<String, Job>> {
    static J: OnceLock<Mutex<HashMap<String, Job>>> = OnceLock::new();
    J.get_or_init(Default::default)
}

pub fn start(command: &str, workspace: &Path) -> Result<String> {
    let id = spawn(command, workspace, false)?;
    Ok(format!(
        "started {id} in the background: `{command}`. Read its output with bash {{\"job\": \"{id}\"}}; \
         stop it with bash {{\"job\": \"{id}\", \"stop\": true}}."
    ))
}

/// Start `command` in `workspace` and return its id. `by_user` marks a
/// command typed in the desktop terminal panel.
pub fn spawn(command: &str, workspace: &Path, by_user: bool) -> Result<String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let mut cmd = shell_command(command);
    cmd.current_dir(workspace).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    scrub_secrets(&mut cmd);
    // Its own process group, so `stop` (kill_tree) reaches what it started.
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().with_context(|| format!("starting `{command}`"))?;
    adopt_into_job(&child);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let id = if by_user { format!("term{n}") } else { format!("job{n}") };
    let out = Arc::new(Mutex::new(Vec::new()));
    let dropped = Arc::new(Mutex::new(0usize));
    let exit = Arc::new(Mutex::new(None));
    for pipe in [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let (out, dropped) = (out.clone(), dropped.clone());
        tokio::spawn(async move {
            let mut pipe = pipe;
            let mut buf = [0u8; 4096];
            while let Ok(n) = tokio::io::AsyncReadExt::read(&mut pipe, &mut buf).await {
                if n == 0 {
                    break;
                }
                let mut o = out.lock().unwrap_or_else(|e| e.into_inner());
                o.extend_from_slice(&buf[..n]);
                if o.len() > KEEP {
                    let cut = o.len() - KEEP;
                    o.drain(..cut);
                    *dropped.lock().unwrap_or_else(|e| e.into_inner()) += cut;
                }
            }
        });
    }
    let pid = child.id();
    let exit_slot = exit.clone();
    tokio::spawn(async move {
        let status = child.wait().await;
        *exit_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(match status {
            Ok(s) => format!("exited with code {}", s.code().unwrap_or(-1)),
            Err(e) => format!("failed: {e}"),
        });
    });
    let mut map = jobs().lock().unwrap_or_else(|e| e.into_inner());
    prune(&mut map);
    map.insert(
        id.clone(),
        Job {
            command: command.to_string(),
            by_user,
            cwd: workspace.to_path_buf(),
            pid,
            out,
            dropped,
            read_to: 0,
            exit,
        },
    );
    Ok(id)
}

/// Ended agent jobs kept for reading (each holds up to `KEEP` bytes).
const KEEP_ENDED: usize = 20;

/// Forget the oldest ended agent jobs beyond [`KEEP_ENDED`]: the map
/// lived as long as Habi and kept every finished job's output. Running
/// jobs and the user's terminal commands (cleared by the user) stay.
fn prune(map: &mut HashMap<String, Job>) {
    let num = |id: &str| id.trim_start_matches(|c: char| c.is_ascii_alphabetic()).parse::<u64>().unwrap_or(0);
    let mut ended: Vec<u64> = map
        .iter()
        .filter(|(_, j)| !j.by_user && j.exit.lock().unwrap_or_else(|e| e.into_inner()).is_some())
        .map(|(id, _)| num(id))
        .collect();
    if ended.len() <= KEEP_ENDED {
        return;
    }
    ended.sort_unstable();
    let cutoff = ended[ended.len() - KEEP_ENDED];
    map.retain(|id, j| j.by_user || j.exit.lock().unwrap_or_else(|e| e.into_inner()).is_none() || num(id) >= cutoff);
}

/// How many jobs are remembered (tests).
#[cfg(test)]
pub fn count() -> usize {
    jobs().lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// One job as the terminal panel shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct JobView {
    pub id: String,
    pub command: String,
    pub by_user: bool,
    pub cwd: PathBuf,
    /// `None` while it runs, else how it ended.
    pub exit: Option<String>,
    /// The end of its output (lossy UTF-8, whole lines).
    pub tail: String,
    /// All output bytes so far (to tell when something new came in).
    pub total: usize,
}

/// Every job, oldest first, with the last `max` bytes of its output.
/// The agent's read position (`output`) is not moved.
pub fn list(max: usize) -> Vec<JobView> {
    let map = jobs().lock().unwrap_or_else(|e| e.into_inner());
    let mut v: Vec<JobView> = map
        .iter()
        .map(|(id, j)| {
            let o = j.out.lock().unwrap_or_else(|e| e.into_inner());
            let dropped = *j.dropped.lock().unwrap_or_else(|e| e.into_inner());
            let mut start = o.len().saturating_sub(max);
            // Start on a line, so the panel never shows half a line.
            if start > 0 {
                if let Some(nl) = o[start..].iter().position(|&b| b == b'\n') {
                    start += nl + 1;
                }
            }
            JobView {
                id: id.clone(),
                command: j.command.clone(),
                by_user: j.by_user,
                cwd: j.cwd.clone(),
                exit: j.exit.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                tail: String::from_utf8_lossy(&o[start..]).into_owned(),
                total: dropped + o.len(),
            }
        })
        .collect();
    let num = |id: &str| id.trim_start_matches(|c: char| c.is_ascii_alphabetic()).parse::<u64>().unwrap_or(0);
    v.sort_by_key(|j| num(&j.id));
    v
}

/// True while any job is still running. Cheap: no output is copied (the
/// GUI asks on every event-loop wake).
pub fn any_running() -> bool {
    let map = jobs().lock().unwrap_or_else(|e| e.into_inner());
    map.values().any(|j| j.exit.lock().unwrap_or_else(|e| e.into_inner()).is_none())
}

/// Forget the ended commands the user typed (the terminal's Clear).
pub fn clear_user_jobs() {
    let mut map = jobs().lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|_, j| !(j.by_user && j.exit.lock().unwrap_or_else(|e| e.into_inner()).is_some()));
}

pub fn output(id: &str) -> Result<String> {
    let mut map = jobs().lock().unwrap_or_else(|e| e.into_inner());
    let job = map.get_mut(id).ok_or_else(|| anyhow!("no background job `{id}` (ids look like job1)"))?;
    let o = job.out.lock().unwrap_or_else(|e| e.into_inner());
    let dropped = *job.dropped.lock().unwrap_or_else(|e| e.into_inner());
    let start = job.read_to.saturating_sub(dropped).min(o.len());
    let new = String::from_utf8_lossy(&o[start..]).into_owned();
    job.read_to = dropped + o.len();
    drop(o);
    let state = job.exit.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| "still running".into());
    let body = if new.trim().is_empty() { "(no new output)".to_string() } else { truncate_middle(&new, 3_000, 9_000) };
    Ok(format!("{id} `{}` — {state}\n{body}", job.command))
}

pub async fn stop(id: &str) -> Result<String> {
    let pid = {
        let map = jobs().lock().unwrap_or_else(|e| e.into_inner());
        let job = map.get(id).ok_or_else(|| anyhow!("no background job `{id}`"))?;
        if job.exit.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
            return Ok(format!("{id} had already ended"));
        }
        job.pid
    };
    kill_tree(pid).await;
    Ok(format!("stopped {id}"))
}

/// Stop every job still running, with everything it started. Called on
/// exit: dropping a job only kills its shell, so a dev server it started
/// (`python -m http.server`) kept running after Habi closed.
pub async fn stop_all() {
    let pids: Vec<Option<u32>> = {
        let map = jobs().lock().unwrap_or_else(|e| e.into_inner());
        map.values().filter(|j| j.exit.lock().unwrap_or_else(|e| e.into_inner()).is_none()).map(|j| j.pid).collect()
    };
    for pid in pids {
        kill_tree(pid).await;
    }
}

/// [`stop_all`] for code that cannot wait on the runtime (the GUI's
/// event loop runs inside it and never returns).
pub fn stop_all_now() {
    let pids: Vec<u32> = {
        let map = jobs().lock().unwrap_or_else(|e| e.into_inner());
        map.values()
            .filter(|j| j.exit.lock().unwrap_or_else(|e| e.into_inner()).is_none())
            .filter_map(|j| j.pid)
            .collect()
    };
    for pid in pids {
        #[cfg(windows)]
        winjob::kill(pid);
        #[cfg(windows)]
        let mut cmd = {
            let mut c = std::process::Command::new("taskkill");
            c.args(["/T", "/F", "/PID", &pid.to_string()]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = std::process::Command::new("kill");
            c.args(["-s", "KILL", "--", &format!("-{pid}")]);
            c
        };
        let _ = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
}

#[cfg(test)]
mod prune_tests {
    use super::*;

    fn job(by_user: bool, ended: bool) -> Job {
        Job {
            command: "x".into(),
            by_user,
            cwd: PathBuf::from("."),
            pid: None,
            out: Default::default(),
            dropped: Default::default(),
            read_to: 0,
            exit: Arc::new(Mutex::new(ended.then(|| "exited with code 0".to_string()))),
        }
    }

    #[test]
    fn prune_keeps_running_user_and_the_newest_ended_jobs() {
        let mut map = HashMap::new();
        // job1..job25 ended, job26 still running, term27 the user's.
        for n in 1..=25 {
            map.insert(format!("job{n}"), job(false, true));
        }
        map.insert("job26".into(), job(false, false));
        map.insert("term27".into(), job(true, true));
        prune(&mut map);
        assert_eq!(map.len(), 22);
        assert!(map.contains_key("job26") && map.contains_key("term27"));
        for n in 1..=5 {
            assert!(!map.contains_key(&format!("job{n}")), "job{n} is among the oldest");
        }
        for n in 6..=25 {
            assert!(map.contains_key(&format!("job{n}")), "job{n} is among the newest {KEEP_ENDED}");
        }
        // Already within the limit: nothing goes.
        prune(&mut map);
        assert_eq!(map.len(), 22);
    }
}
