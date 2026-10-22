use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

/// Message part stored as JSON — keeps us fork/branch-ready like OpenCode's sessions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text { text: String },
    /// Model chain-of-thought, persisted for replay but NEVER sent back to
    /// the provider (providers reject reasoning roles in history).
    Reasoning { text: String },
    ToolCall { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_id: String, output: String, is_error: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub role: String, // system | user | assistant
    pub parts: Vec<Part>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub parent_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct Db {
    conn: Mutex<Connection>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn new_id(prefix: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    now_ms().hash(&mut h);
    std::thread::current().id().hash(&mut h);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos.hash(&mut h);
    let mut buf = [0u8; 8];
    // Cheap per-process randomness for ids: hash + atomic counter.
    static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    n.hash(&mut h);
    h.write(&mut buf);
    format!("{prefix}_{:016x}", h.finish())
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                parent_id TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                role TEXT NOT NULL,
                parts TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, created_at);
            CREATE TABLE IF NOT EXISTS approvals (
                id TEXT PRIMARY KEY,
                tool TEXT NOT NULL,
                pattern TEXT NOT NULL,
                scope TEXT NOT NULL,   -- 'once' | 'session'
                created_at INTEGER NOT NULL
            );",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        // reuse schema via a temp file path? simplest: run the same batch
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                parent_id TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                parts TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, created_at);
            CREATE TABLE IF NOT EXISTS approvals (
                id TEXT PRIMARY KEY,
                tool TEXT NOT NULL,
                pattern TEXT NOT NULL,
                scope TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn create_session(&self, title: &str, parent_id: Option<&str>) -> Result<Session> {
        let id = new_id("ses");
        let now = now_ms();
        let s = Session {
            id: id.clone(),
            title: title.to_string(),
            parent_id: parent_id.map(str::to_string),
            created_at: now,
            updated_at: now,
        };
        self.conn.lock().unwrap().execute(
            "INSERT INTO sessions (id, title, parent_id, created_at, updated_at) VALUES (?1,?2,?3,?4,?5)",
            params![s.id, s.title, s.parent_id, s.created_at, s.updated_at],
        )?;
        Ok(s)
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id,title,parent_id,created_at,updated_at FROM sessions ORDER BY updated_at DESC")?;
        let rows = stmt.query_map([], |r| {
            Ok(Session {
                id: r.get(0)?,
                title: r.get(1)?,
                parent_id: r.get(2)?,
                created_at: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Rename a session (the GUI titles chats after their first message).
    pub fn set_session_title(&self, id: &str, title: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE sessions SET title=?2, updated_at=?3 WHERE id=?1",
            params![id, title, now_ms()],
        )?;
        Ok(())
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let conn = self.conn.lock().unwrap();
        let s = conn
            .query_row(
                "SELECT id,title,parent_id,created_at,updated_at FROM sessions WHERE id=?1",
                params![id],
                |r| {
                    Ok(Session {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        parent_id: r.get(2)?,
                        created_at: r.get(3)?,
                        updated_at: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(s)
    }

    pub fn add_message(&self, session_id: &str, role: &str, parts: &[Part]) -> Result<Message> {
        self.insert_message(session_id, role, parts, now_ms())
    }

    /// Insert with an explicit timestamp (used by fork/compaction to preserve order).
    fn insert_message(
        &self,
        session_id: &str,
        role: &str,
        parts: &[Part],
        created_at: i64,
    ) -> Result<Message> {
        let m = Message {
            id: new_id("msg"),
            session_id: session_id.to_string(),
            role: role.to_string(),
            parts: parts.to_vec(),
            created_at,
        };
        let conn = self.conn.lock().unwrap();
        let json = serde_json::to_string(&m.parts)?;
        conn.execute(
            "INSERT INTO messages (id, session_id, role, parts, created_at) VALUES (?1,?2,?3,?4,?5)",
            params![m.id, m.session_id, m.role, json, m.created_at],
        )?;
        conn.execute(
            "UPDATE sessions SET updated_at=?1 WHERE id=?2",
            params![std::cmp::max(m.created_at, now_ms()), session_id],
        )?;
        Ok(m)
    }

    /// Fork/branch: copy every message of `id` into a new child session.
    pub fn fork_session(&self, id: &str, title: &str) -> Result<Session> {
        let parent = self
            .get_session(id)?
            .with_context(|| format!("no session `{id}`"))?;
        let new = self.create_session(title, Some(&parent.id))?;
        for m in self.messages(&parent.id)? {
            self.insert_message(&new.id, &m.role, &m.parts, m.created_at)?;
        }
        Ok(new)
    }

    /// Compaction: delete every message not in `kept_ids` and insert
    /// `summary` as a system message positioned where the removed history
    /// began (timestamp minus 1ms so ordering survives).
    pub fn apply_compaction(&self, session_id: &str, kept_ids: &[String], summary: &str) -> Result<()> {
        let msgs = self.messages(session_id)?;
        let kept: std::collections::HashSet<&str> =
            kept_ids.iter().map(String::as_str).collect();
        let removed: Vec<&Message> = msgs
            .iter()
            .filter(|m| !kept.contains(m.id.as_str()))
            .collect();
        let first_at = removed.first().map(|m| m.created_at).unwrap_or_else(now_ms);
        {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare("DELETE FROM messages WHERE id=?1")?;
            for m in &removed {
                stmt.execute(params![m.id])?;
            }
        }
        self.insert_message(
            session_id,
            "system",
            &[Part::Text { text: summary.to_string() }],
            first_at.saturating_sub(1),
        )?;
        Ok(())
    }

    pub fn messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, role, parts, created_at FROM messages WHERE session_id=?1 ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            let id: String = r.get(0)?;
            let role: String = r.get(1)?;
            let parts_json: String = r.get(2)?;
            let created_at: i64 = r.get(3)?;
            Ok((id, role, parts_json, created_at))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, role, parts_json, created_at) = row?;
            let parts: Vec<Part> = serde_json::from_str(&parts_json).unwrap_or_default();
            out.push(Message { id, session_id: session_id.to_string(), role, parts, created_at });
        }
        Ok(out)
    }

    /// Session-scope remembered approvals ("always" during this session).
    pub fn add_approval(&self, tool: &str, pattern: &str, scope: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT OR IGNORE INTO approvals (id, tool, pattern, scope, created_at) VALUES (?1,?2,?3,?4,?5)",
            params![new_id("apr"), tool, pattern, scope, now_ms()],
        )?;
        Ok(())
    }

    pub fn has_approval(&self, tool: &str, pattern: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM approvals WHERE tool=?1 AND pattern=?2",
            params![tool, pattern],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("hello", None).unwrap();
        db.add_message(&s.id, "user", &[Part::Text { text: "hi".into() }]).unwrap();
        db.add_message(
            &s.id,
            "assistant",
            &[Part::ToolCall { id: "t1".into(), name: "bash".into(), input: serde_json::json!({"cmd":"ls"}) }],
        )
        .unwrap();
        let msgs = db.messages(&s.id).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        match &msgs[1].parts[0] {
            Part::ToolCall { name, .. } => assert_eq!(name, "bash"),
            _ => panic!("wrong part"),
        }
    }

    #[test]
    fn fork_copies_history_into_child_session() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("orig", None).unwrap();
        db.add_message(&s.id, "user", &[Part::Text { text: "hi".into() }]).unwrap();
        db.add_message(
            &s.id,
            "assistant",
            &[Part::ToolCall { id: "t1".into(), name: "bash".into(), input: serde_json::json!({"command":"ls"}) }],
        )
        .unwrap();
        db.add_message(
            &s.id,
            "tool",
            &[Part::ToolResult { tool_id: "t1".into(), output: "out".into(), is_error: false }],
        )
        .unwrap();

        let fork = db.fork_session(&s.id, "fork").unwrap();
        assert_ne!(fork.id, s.id);
        assert_eq!(fork.parent_id.as_deref(), Some(s.id.as_str()));
        let copy = db.messages(&fork.id).unwrap();
        assert_eq!(copy.len(), 3);
        assert_eq!(copy[0].role, "user");
        assert_eq!(copy[1].role, "assistant");
        assert_eq!(copy[2].role, "tool");
        // Order preserved (equal timestamps fall back to insertion order).
        assert!(copy[0].created_at <= copy[1].created_at);
        // Parent untouched.
        assert_eq!(db.messages(&s.id).unwrap().len(), 3);
        // Fork of a missing session fails.
        assert!(db.fork_session("ses_nope", "x").is_err());
    }

    #[test]
    fn compaction_keeps_tail_and_prepends_summary() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("c", None).unwrap();
        for i in 0..6 {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            db.add_message(&s.id, role, &[Part::Text { text: format!("m{i}") }]).unwrap();
        }
        let all = db.messages(&s.id).unwrap();
        let kept: Vec<String> = all[4..].iter().map(|m| m.id.clone()).collect();
        db.apply_compaction(&s.id, &kept, "[compacted] summary here").unwrap();

        let after = db.messages(&s.id).unwrap();
        assert_eq!(after.len(), 3);
        assert_eq!(after[0].role, "system");
        assert!(matches!(&after[0].parts[0], Part::Text { text } if text.contains("summary here")));
        // Summary sorts strictly before the kept tail.
        assert!(after[0].created_at < after[1].created_at);
        assert_eq!(after[1].id, kept[0]);
        assert_eq!(after[2].id, kept[1]);
        // Kept messages still readable in full.
        let tail = db.messages(&s.id).unwrap();
        assert!(matches!(&tail[1].parts[0], Part::Text { text } if text == "m4"));
    }

    #[test]
    fn set_session_title_renames_and_reorders() {
        let db = Db::open_in_memory().unwrap();
        let a = db.create_session("first", None).unwrap();
        let b = db.create_session("second", None).unwrap();
        db.set_session_title(&a.id, "renamed").unwrap();
        assert_eq!(db.get_session(&a.id).unwrap().unwrap().title, "renamed");
        // Renaming bumps updated_at, so the renamed chat sorts first.
        let list = db.list_sessions().unwrap();
        assert_eq!(list[0].id, a.id);
        assert_eq!(list[1].id, b.id);
        // Unknown ids are a no-op, not an error.
        db.set_session_title("ses_nope", "x").unwrap();
    }

    #[test]
    fn approvals_and_listing() {
        let db = Db::open_in_memory().unwrap();
        db.create_session("a", None).unwrap();
        db.create_session("b", None).unwrap();
        assert_eq!(db.list_sessions().unwrap().len(), 2);
        db.add_approval("bash", "git status", "session").unwrap();
        assert!(db.has_approval("bash", "git status").unwrap());
        assert!(!db.has_approval("bash", "rm -rf /").unwrap());
    }
}
