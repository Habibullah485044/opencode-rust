//! Keep-best fix rounds: while a journal is open for a folder, the first
//! write to each file inside it saves what the file held before (or that it
//! did not exist). If a fix round leaves the checks no better, `restore`
//! puts every file back. Only `write` and `edit` are journalled; files a
//! shell command changes are not.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Open journals: root folder → (journal id → original contents by file).
type Journals = HashMap<PathBuf, (u64, HashMap<PathBuf, Option<Vec<u8>>>)>;

fn journals() -> &'static Mutex<Journals> {
    static J: OnceLock<Mutex<Journals>> = OnceLock::new();
    J.get_or_init(Default::default)
}

fn key(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Files the tools wrote, newest last (the post-run checks look here for
/// changed web pages).
fn writes() -> &'static Mutex<Vec<(PathBuf, std::time::SystemTime)>> {
    static W: OnceLock<Mutex<Vec<(PathBuf, std::time::SystemTime)>>> = OnceLock::new();
    W.get_or_init(Default::default)
}

/// Files under `root` the tools wrote at or after `since`.
pub fn written_since(root: &Path, since: std::time::SystemTime) -> Vec<PathBuf> {
    let w = writes().lock().unwrap_or_else(|e| e.into_inner());
    since_in(&w, &key(root), since)
}

/// [`written_since`] over a given write log: newest first, each file once.
fn since_in(w: &[(PathBuf, std::time::SystemTime)], root: &Path, since: std::time::SystemTime) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for (p, t) in w.iter().rev() {
        if *t >= since && p.starts_with(root) && !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}

/// Add a write to the log; past 2000 entries the oldest 1000 go.
fn record(w: &mut Vec<(PathBuf, std::time::SystemTime)>, file: PathBuf, at: std::time::SystemTime) {
    if w.len() >= 2000 {
        w.drain(..1000);
    }
    w.push((file, at));
}

/// Record the state of `file` before the tools change it, if a journal
/// covers it and the file was not recorded yet.
pub fn before_write(file: &Path) {
    let file = key(file);
    record(&mut writes().lock().unwrap_or_else(|e| e.into_inner()), file.clone(), std::time::SystemTime::now());
    let mut map = journals().lock().unwrap_or_else(|e| e.into_inner());
    for (root, (_, saved)) in map.iter_mut() {
        if file.starts_with(root) && !saved.contains_key(&file) {
            saved.insert(file.clone(), std::fs::read(&file).ok());
        }
    }
}

/// An open journal. Dropping it keeps the changes.
pub struct Journal {
    root: PathBuf,
    id: u64,
}

impl Journal {
    /// Start recording first writes under `root` (replaces an older journal
    /// for the same folder).
    pub fn begin(root: &Path) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let root = key(root);
        journals().lock().unwrap_or_else(|e| e.into_inner()).insert(root.clone(), (id, HashMap::new()));
        Self { root, id }
    }

    /// Files written since `begin`.
    pub fn changed(&self) -> usize {
        let map = journals().lock().unwrap_or_else(|e| e.into_inner());
        map.get(&self.root).filter(|(id, _)| *id == self.id).map_or(0, |(_, s)| s.len())
    }

    /// Put every journalled file back as it was. Returns how many.
    pub fn restore(self) -> usize {
        let saved = {
            let mut map = journals().lock().unwrap_or_else(|e| e.into_inner());
            match map.get(&self.root) {
                Some((id, _)) if *id == self.id => map.remove(&self.root).map(|(_, s)| s).unwrap_or_default(),
                _ => HashMap::new(),
            }
        };
        let mut n = 0;
        for (file, before) in saved {
            let ok = match before {
                Some(bytes) => std::fs::write(&file, bytes).is_ok(),
                None => !file.exists() || std::fs::remove_file(&file).is_ok(),
            };
            n += usize::from(ok);
        }
        n
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        let mut map = journals().lock().unwrap_or_else(|e| e.into_inner());
        if map.get(&self.root).is_some_and(|(id, _)| *id == self.id) {
            map.remove(&self.root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_puts_files_back_and_removes_new_ones() {
        let dir = std::env::temp_dir().join(format!("ocrs-keepbest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("a.txt");
        let new = dir.join("b.txt");
        std::fs::write(&old, "before").unwrap();

        let j = Journal::begin(&dir);
        before_write(&old);
        std::fs::write(&old, "worse").unwrap();
        before_write(&old); // a second write keeps the first original
        std::fs::write(&old, "worse again").unwrap();
        before_write(&new);
        std::fs::write(&new, "new").unwrap();
        before_write(&std::env::temp_dir().join("outside.txt")); // not under the root
        assert_eq!(j.changed(), 2);
        assert_eq!(j.restore(), 2);
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "before");
        assert!(!new.exists());

        let j = Journal::begin(&dir);
        before_write(&old);
        std::fs::write(&old, "better").unwrap();
        drop(j); // kept
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "better");
        before_write(&old); // no journal: nothing recorded
        assert!(journals().lock().unwrap().get(&key(&dir)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T-083: the write log is filtered by folder and time, names each
    /// file once (newest first) and keeps at most 2000 entries.
    #[test]
    fn the_write_log_filters_dedups_and_drains() {
        use std::time::{Duration, SystemTime};
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let at = |s: u64| t0 + Duration::from_secs(s);
        let (root, other) = (PathBuf::from("/ws"), PathBuf::from("/elsewhere"));
        let mut w = Vec::new();
        record(&mut w, root.join("old.txt"), at(0));
        record(&mut w, root.join("a.txt"), at(10));
        record(&mut w, other.join("x.txt"), at(11));
        record(&mut w, root.join("b.txt"), at(12));
        record(&mut w, root.join("a.txt"), at(13));
        assert_eq!(since_in(&w, &root, at(10)), [root.join("a.txt"), root.join("b.txt")]);
        assert_eq!(since_in(&w, &root, at(0)).len(), 3);
        assert_eq!(since_in(&w, &other, at(0)), [other.join("x.txt")]);
        assert!(since_in(&w, &root, at(14)).is_empty());
        for i in 0..2100 {
            record(&mut w, root.join(format!("f{i}")), at(100 + i));
        }
        assert!(w.len() <= 2000, "{}", w.len());
        assert_eq!(w.last().unwrap().0, root.join("f2099"), "the newest stay");
        assert!(!w.iter().any(|(p, _)| p.ends_with("old.txt")), "the oldest went");
    }

    /// T-083: `changed` counts the files of this journal only; a newer
    /// journal for the same folder takes over.
    #[test]
    fn changed_counts_first_writes_of_this_journal() {
        let dir = std::env::temp_dir().join(format!("ocrs-keepbest-changed-{}", std::process::id()));
        let j = Journal::begin(&dir);
        assert_eq!(j.changed(), 0);
        before_write(&dir.join("a.txt"));
        before_write(&dir.join("a.txt"));
        before_write(&dir.join("sub").join("b.txt"));
        assert_eq!(j.changed(), 2, "each file once");
        let newer = Journal::begin(&dir);
        assert_eq!(j.changed(), 0, "replaced by the newer journal");
        before_write(&dir.join("c.txt"));
        assert_eq!(newer.changed(), 1);
        assert!(written_since(&dir, std::time::SystemTime::UNIX_EPOCH).contains(&key(&dir.join("c.txt"))));
    }
}
