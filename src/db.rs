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
    pub role: String, // system | user | assistant | tool
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
    /// The project (folder) this chat works in; `None` = an older chat made
    /// before projects existed (it runs in the folder the app started in).
    #[serde(default)]
    pub project_id: Option<String>,
    /// Shown at the top of the sidebar.
    #[serde(default)]
    pub pinned: bool,
    /// Hidden from the sidebar; still in History.
    #[serde(default)]
    pub archived: bool,
}

/// A project: a folder whose chats run their tools, git snapshots and
/// project instructions (AGENTS.md, skills) inside it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub id: String,
    pub name: String,
    /// The folder, as given (shown to the user and used as the working dir).
    pub root: String,
    pub created_at: i64,
}

/// One sidebar row: a chat and whether anything was ever said in it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRow {
    #[serde(flatten)]
    pub session: Session,
    pub has_messages: bool,
}

pub struct Db {
    conn: Mutex<Connection>,
    /// The project new top-level chats are filed under (the folder the app
    /// started in). Child chats (subagents, forks) take their parent's.
    default_project: Mutex<Option<String>>,
}

/// Columns of `sessions` in the order `session_from_row` reads them.
const SESSION_COLS: &str = "id,title,parent_id,created_at,updated_at,project_id,pinned,archived";

fn session_from_row(r: &rusqlite::Row) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get(0)?,
        title: r.get(1)?,
        parent_id: r.get(2)?,
        created_at: r.get(3)?,
        updated_at: r.get(4)?,
        project_id: r.get(5)?,
        pinned: r.get::<_, i64>(6)? != 0,
        archived: r.get::<_, i64>(7)? != 0,
    })
}

fn project_from_row(r: &rusqlite::Row) -> rusqlite::Result<Project> {
    Ok(Project { id: r.get(0)?, name: r.get(1)?, root: r.get(2)?, created_at: r.get(3)? })
}

/// Projects table, and the project/pin/archive columns on `sessions`
/// (added in place to databases from older builds).
fn migrate_projects(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS projects (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            root TEXT NOT NULL,
            root_key TEXT NOT NULL UNIQUE,
            created_at INTEGER NOT NULL
        );",
    )?;
    let cols: Vec<String> = {
        let mut stmt = conn.prepare("PRAGMA table_info(sessions)")?;
        let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
        names.collect::<std::result::Result<_, _>>()?
    };
    for (col, def) in [
        ("project_id", "TEXT"),
        ("pinned", "INTEGER NOT NULL DEFAULT 0"),
        ("archived", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !cols.iter().any(|c| c == col) {
            conn.execute_batch(&format!("ALTER TABLE sessions ADD COLUMN {col} {def};"))?;
        }
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn new_id(prefix: &str) -> String {
    use std::hash::{BuildHasher, Hash, Hasher};
    // `RandomState` is seeded randomly per process, so two processes that
    // start in the same instant still get different ids; time plus a counter
    // keeps ids unique inside one process. Ids are unique, not secret.
    static SEED: std::sync::OnceLock<std::collections::hash_map::RandomState> =
        std::sync::OnceLock::new();
    static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut h = SEED.get_or_init(std::collections::hash_map::RandomState::new).build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    nanos.hash(&mut h);
    CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed).hash(&mut h);
    format!("{prefix}_{:016x}", h.finish())
}

/// Approvals table (S-07): one row per (workspace, tool, pattern).
const APPROVALS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS approvals (
    id TEXT PRIMARY KEY,
    workspace TEXT NOT NULL,
    tool TEXT NOT NULL,
    pattern TEXT NOT NULL,
    scope TEXT NOT NULL,   -- 'once' | 'session'
    created_at INTEGER NOT NULL,
    UNIQUE (workspace, tool, pattern)
);";

/// Provider token counts, one row per session per UTC day (`/usage`).
/// `session_id` is "" for model calls outside a chat (planning, edits).
/// Missions: one per chat (a new one replaces the old), and its steps in
/// order. A step's `result` is its receipt: what the worker said it did,
/// the files it changed and what the checks said.
const MISSION_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS missions (
    session_id TEXT PRIMARY KEY,
    goal TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS mission_steps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    pos REAL NOT NULL,
    title TEXT NOT NULL,
    status TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    result TEXT NOT NULL DEFAULT '',
    child_id TEXT
);
CREATE INDEX IF NOT EXISTS idx_mission_steps ON mission_steps(session_id, pos);";

/// A chat's mission.
#[derive(Debug, Clone, PartialEq)]
pub struct Mission {
    pub goal: String,
    /// planning, running, paused, done, stopped
    pub status: String,
    pub steps: Vec<MissionStep>,
}

/// One step of a mission.
#[derive(Debug, Clone, PartialEq)]
pub struct MissionStep {
    pub id: i64,
    pub title: String,
    /// pending, working, done, failed
    pub status: String,
    pub attempts: i64,
    pub result: String,
    pub child_id: Option<String>,
}

const USAGE_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS usage (
    session_id TEXT NOT NULL,
    day INTEGER NOT NULL,
    requests INTEGER NOT NULL DEFAULT 0,
    input INTEGER NOT NULL DEFAULT 0,
    cached INTEGER NOT NULL DEFAULT 0,
    cache_write INTEGER NOT NULL DEFAULT 0,
    output INTEGER NOT NULL DEFAULT 0,
    cost_micros INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, day)
);";

/// Summed token counts (`/usage`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub requests: u64,
    pub input: u64,
    pub cached: u64,
    pub cache_write: u64,
    pub output: u64,
    /// Summed cost in millionths of a US dollar (0 = unknown).
    #[serde(default)]
    pub cost_micros: u64,
}

/// A usage table from a build before costs were kept has no `cost_micros`.
fn migrate_usage(conn: &Connection) -> Result<()> {
    conn.execute_batch(USAGE_SCHEMA)?;
    let has_cost = {
        let mut stmt = conn.prepare("PRAGMA table_info(usage)")?;
        let names: Vec<String> = stmt.query_map([], |r| r.get::<_, String>(1))?.flatten().collect();
        names.iter().any(|c| c == "cost_micros")
    };
    if !has_cost {
        conn.execute_batch("ALTER TABLE usage ADD COLUMN cost_micros INTEGER NOT NULL DEFAULT 0;")?;
    }
    Ok(())
}

/// Bring the approvals table to the S-07 layout. A table from an older
/// build has no `workspace` column: it is renamed to `approvals_unscoped`
/// and never read again, so approvals saved before this change no longer
/// apply anywhere (the user is simply asked again).
fn migrate_approvals(conn: &Connection) -> Result<()> {
    let has_workspace_column = {
        let mut stmt = conn.prepare("PRAGMA table_info(approvals)")?;
        let names = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        // No rows means the table does not exist yet.
        names.is_empty() || names.iter().any(|n| n == "workspace")
    };
    if !has_workspace_column {
        conn.execute_batch(
            "DROP TABLE IF EXISTS approvals_unscoped;
             ALTER TABLE approvals RENAME TO approvals_unscoped;",
        )?;
    }
    conn.execute_batch(APPROVALS_SCHEMA)?;
    Ok(())
}

/// The key "always allow" approvals are stored under: the workspace's
/// canonical path (symlinks and `..` resolved; lower-case on Windows), so
/// two spellings of one folder share their approvals.
pub fn workspace_key(workspace: &Path) -> String {
    let p = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let s = p.to_string_lossy().into_owned();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
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
        // SQLite ignores REFERENCES / ON DELETE CASCADE unless this is on.
        conn.pragma_update(None, "foreign_keys", "ON")?;
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
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, created_at);",
        )?;
        migrate_approvals(&conn)?;
        migrate_usage(&conn)?;
        conn.execute_batch(MISSION_SCHEMA)?;
        migrate_projects(&conn)?;
        Ok(Self { conn: Mutex::new(conn), default_project: Mutex::new(None) })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        // Same rules and schema as the file database.
        conn.pragma_update(None, "foreign_keys", "ON")?;
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
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, created_at);",
        )?;
        migrate_approvals(&conn)?;
        migrate_usage(&conn)?;
        conn.execute_batch(MISSION_SCHEMA)?;
        migrate_projects(&conn)?;
        Ok(Self { conn: Mutex::new(conn), default_project: Mutex::new(None) })
    }

    /// A new chat. A child chat (subagent) joins its parent's project; a
    /// top-level chat joins the default project (see `set_default_project`).
    pub fn create_session(&self, title: &str, parent_id: Option<&str>) -> Result<Session> {
        let project = match parent_id {
            Some(p) => self.get_session(p)?.and_then(|s| s.project_id),
            None => self.default_project.lock().unwrap().clone(),
        };
        self.create_session_in(title, parent_id, project.as_deref())
    }

    /// A new chat in a given project (`None` = no project).
    pub fn create_session_in(
        &self,
        title: &str,
        parent_id: Option<&str>,
        project_id: Option<&str>,
    ) -> Result<Session> {
        let now = now_ms();
        let s = Session {
            id: new_id("ses"),
            title: title.to_string(),
            parent_id: parent_id.map(str::to_string),
            created_at: now,
            updated_at: now,
            project_id: project_id.map(str::to_string),
            pinned: false,
            archived: false,
        };
        self.conn.lock().unwrap().execute(
            "INSERT INTO sessions (id, title, parent_id, created_at, updated_at, project_id)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![s.id, s.title, s.parent_id, s.created_at, s.updated_at, s.project_id],
        )?;
        Ok(s)
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("SELECT {SESSION_COLS} FROM sessions ORDER BY updated_at DESC"))?;
        let rows = stmt.query_map([], session_from_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Every chat with whether it has any message (empty chats are hidden
    /// in the sidebar), newest first.
    pub fn list_session_rows(&self) -> Result<Vec<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        let cols: Vec<String> = SESSION_COLS.split(',').map(|c| format!("s.{c}")).collect();
        let mut stmt = conn.prepare(&format!(
            "SELECT {}, EXISTS(SELECT 1 FROM messages m WHERE m.session_id = s.id)
             FROM sessions s ORDER BY s.updated_at DESC",
            cols.join(",")
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok(SessionRow { session: session_from_row(r)?, has_messages: r.get::<_, i64>(8)? != 0 })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// File new top-level chats under this project from now on.
    pub fn set_default_project(&self, project_id: Option<&str>) {
        *self.default_project.lock().unwrap() = project_id.map(str::to_string);
    }

    /// The project for folder `root` (matched by its canonical path),
    /// created and named after the folder when it is new.
    pub fn upsert_project(&self, root: &Path) -> Result<Project> {
        let key = workspace_key(root);
        let conn = self.conn.lock().unwrap();
        let found = conn
            .query_row(
                "SELECT id,name,root,created_at FROM projects WHERE root_key=?1",
                params![key],
                project_from_row,
            )
            .optional()?;
        if let Some(p) = found {
            return Ok(p);
        }
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| root.display().to_string());
        let p = Project { id: new_id("prj"), name, root: root.display().to_string(), created_at: now_ms() };
        conn.execute(
            "INSERT INTO projects (id, name, root, root_key, created_at) VALUES (?1,?2,?3,?4,?5)",
            params![p.id, p.name, p.root, key, p.created_at],
        )?;
        Ok(p)
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id,name,root,created_at FROM projects ORDER BY lower(name)")?;
        let rows = stmt.query_map([], project_from_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn get_project(&self, id: &str) -> Result<Option<Project>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT id,name,root,created_at FROM projects WHERE id=?1", params![id], project_from_row)
            .optional()?)
    }

    /// Take a project off the list. Its chats stay, without a project.
    pub fn remove_project(&self, id: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("UPDATE sessions SET project_id=NULL WHERE project_id=?1", params![id])?;
        tx.execute("DELETE FROM projects WHERE id=?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_session_project(&self, id: &str, project_id: Option<&str>) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("UPDATE sessions SET project_id=?2 WHERE id=?1", params![id, project_id])?;
        Ok(())
    }

    /// File every top-level chat that has no project under `project_id`
    /// (chats made before projects existed). Returns how many moved.
    pub fn adopt_unfiled_sessions(&self, project_id: &str) -> Result<usize> {
        Ok(self.conn.lock().unwrap().execute(
            "UPDATE sessions SET project_id=?1 WHERE project_id IS NULL AND parent_id IS NULL",
            params![project_id],
        )?)
    }

    pub fn set_session_pinned(&self, id: &str, pinned: bool) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("UPDATE sessions SET pinned=?2 WHERE id=?1", params![id, pinned as i64])?;
        Ok(())
    }

    /// Archiving also unpins: an archived chat lives only in History.
    pub fn set_session_archived(&self, id: &str, archived: bool) -> Result<()> {
        let sql = if archived {
            "UPDATE sessions SET archived=1, pinned=0 WHERE id=?1"
        } else {
            "UPDATE sessions SET archived=0 WHERE id=?1"
        };
        self.conn.lock().unwrap().execute(sql, params![id])?;
        Ok(())
    }

    /// Rename a session (the GUI titles chats after their first message).
    pub fn set_session_title(&self, id: &str, title: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE sessions SET title=?2, updated_at=?3 WHERE id=?1",
            params![id, title, now_ms()],
        )?;
        Ok(())
    }

    /// Start a mission in chat `session_id` with these steps (replaces any
    /// earlier mission of the chat).
    pub fn mission_start(&self, session_id: &str, goal: &str, steps: &[String]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let now = now_ms();
        tx.execute("DELETE FROM mission_steps WHERE session_id=?1", params![session_id])?;
        tx.execute(
            "INSERT INTO missions (session_id, goal, status, created_at, updated_at) VALUES (?1, ?2, 'running', ?3, ?3)
             ON CONFLICT (session_id) DO UPDATE SET goal=?2, status='running', created_at=?3, updated_at=?3",
            params![session_id, goal, now],
        )?;
        for (i, t) in steps.iter().enumerate() {
            tx.execute(
                "INSERT INTO mission_steps (session_id, pos, title, status) VALUES (?1, ?2, ?3, 'pending')",
                params![session_id, i as f64 + 1.0, t],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The chat's mission with its steps in order, if it has one.
    pub fn mission(&self, session_id: &str) -> Result<Option<Mission>> {
        let conn = self.conn.lock().unwrap();
        let head = conn
            .query_row(
                "SELECT goal, status FROM missions WHERE session_id=?1",
                params![session_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((goal, status)) = head else { return Ok(None) };
        let mut stmt = conn.prepare(
            "SELECT id, title, status, attempts, result, child_id FROM mission_steps WHERE session_id=?1 ORDER BY pos, id",
        )?;
        let steps = stmt
            .query_map(params![session_id], |r| {
                Ok(MissionStep {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    status: r.get(2)?,
                    attempts: r.get(3)?,
                    result: r.get(4)?,
                    child_id: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(Mission { goal, status, steps }))
    }

    pub fn mission_set_status(&self, session_id: &str, status: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE missions SET status=?2, updated_at=?3 WHERE session_id=?1",
            params![session_id, status, now_ms()],
        )?;
        Ok(())
    }

    /// Save a step's state and receipt.
    pub fn mission_step_update(&self, id: i64, status: &str, attempts: i64, result: &str, child_id: Option<&str>) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE mission_steps SET status=?2, attempts=?3, result=?4, child_id=COALESCE(?5, child_id) WHERE id=?1",
            params![id, status, attempts, result, child_id],
        )?;
        Ok(())
    }

    /// Insert a new pending step right after step `after`.
    pub fn mission_step_insert_after(&self, session_id: &str, after: i64, title: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let pos: f64 = conn.query_row("SELECT pos FROM mission_steps WHERE id=?1", params![after], |r| r.get(0))?;
        let next: Option<f64> = conn
            .query_row(
                "SELECT MIN(pos) FROM mission_steps WHERE session_id=?1 AND pos > ?2",
                params![session_id, pos],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let new_pos = next.map_or(pos + 1.0, |n| (pos + n) / 2.0);
        conn.execute(
            "INSERT INTO mission_steps (session_id, pos, title, status) VALUES (?1, ?2, ?3, 'pending')",
            params![session_id, new_pos, title],
        )?;
        Ok(())
    }

    /// Add one reply's token counts to today's row for `session_id`.
    pub fn add_usage(&self, session_id: &str, u: &crate::provider::Usage) -> Result<()> {
        let day = now_ms() / 86_400_000;
        self.conn.lock().unwrap().execute(
            "INSERT INTO usage (session_id, day, requests, input, cached, cache_write, output, cost_micros)
             VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (session_id, day) DO UPDATE SET
               requests = requests + 1, input = input + ?3, cached = cached + ?4,
               cache_write = cache_write + ?5, output = output + ?6, cost_micros = cost_micros + ?7",
            params![
                session_id,
                day,
                u.input as i64,
                u.cached as i64,
                u.cache_write as i64,
                u.output as i64,
                u.cost_micros as i64
            ],
        )?;
        Ok(())
    }

    /// Summed usage. `session`: one session or all; `days`: only the last
    /// N UTC days (today included) or all time.
    pub fn usage_totals(&self, session: Option<&str>, days: Option<i64>) -> Result<UsageTotals> {
        let from = days.map(|d| now_ms() / 86_400_000 - (d - 1)).unwrap_or(i64::MIN);
        let conn = self.conn.lock().unwrap();
        let t = conn.query_row(
            "SELECT COALESCE(SUM(requests),0), COALESCE(SUM(input),0), COALESCE(SUM(cached),0),
                    COALESCE(SUM(cache_write),0), COALESCE(SUM(output),0), COALESCE(SUM(cost_micros),0)
             FROM usage WHERE day >= ?1 AND (?2 IS NULL OR session_id = ?2)",
            params![from, session],
            |r| {
                Ok(UsageTotals {
                    requests: r.get::<_, i64>(0)? as u64,
                    input: r.get::<_, i64>(1)? as u64,
                    cached: r.get::<_, i64>(2)? as u64,
                    cache_write: r.get::<_, i64>(3)? as u64,
                    output: r.get::<_, i64>(4)? as u64,
                    cost_micros: r.get::<_, i64>(5)? as u64,
                })
            },
        )?;
        Ok(t)
    }

    /// New (F-12): delete a session and its messages in one transaction.
    /// False when the id is unknown.
    pub fn delete_session(&self, id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM messages WHERE session_id=?1", params![id])?;
        let n = tx.execute("DELETE FROM sessions WHERE id=?1", params![id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let conn = self.conn.lock().unwrap();
        let s = conn
            .query_row(&format!("SELECT {SESSION_COLS} FROM sessions WHERE id=?1"), params![id], session_from_row)
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
        let mut conn = self.conn.lock().unwrap();
        let json = serde_json::to_string(&m.parts)?;
        // The message and the session's timestamp change together or not at all.
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO messages (id, session_id, role, parts, created_at) VALUES (?1,?2,?3,?4,?5)",
            params![m.id, m.session_id, m.role, json, m.created_at],
        )?;
        tx.execute(
            "UPDATE sessions SET updated_at=?1 WHERE id=?2",
            params![std::cmp::max(m.created_at, now_ms()), session_id],
        )?;
        tx.commit()?;
        Ok(m)
    }

    /// Fork/branch: copy every message of `id` into a new child session.
    pub fn fork_session(&self, id: &str, title: &str) -> Result<Session> {
        let parent = self
            .get_session(id)?
            .with_context(|| format!("no session `{id}`"))?;
        let msgs = self.messages(&parent.id)?;
        let now = now_ms();
        let new = Session {
            id: new_id("ses"),
            title: title.to_string(),
            parent_id: Some(parent.id.clone()),
            created_at: now,
            updated_at: now,
            project_id: parent.project_id.clone(),
            pinned: false,
            archived: false,
        };
        // One transaction: a failed fork leaves no half-copied session.
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO sessions (id, title, parent_id, created_at, updated_at, project_id)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![new.id, new.title, new.parent_id, new.created_at, new.updated_at, new.project_id],
        )?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO messages (id, session_id, role, parts, created_at) VALUES (?1,?2,?3,?4,?5)",
            )?;
            for m in &msgs {
                let json = serde_json::to_string(&m.parts)?;
                ins.execute(params![new_id("msg"), new.id, m.role, json, m.created_at])?;
            }
        }
        tx.commit()?;
        Ok(new)
    }

    /// Compaction: delete every message not in `kept_ids` and insert
    /// `summary` as a system message positioned where the removed history
    /// began (timestamp minus 1ms so ordering survives).
    /// Compaction: delete exactly the messages in `summarized_ids` and insert
    /// `summary` as one system message just before the oldest of them
    /// (timestamp minus 1 ms, so it sorts first). Everything happens in one
    /// transaction. Messages that arrived while the summary was being
    /// written are not in `summarized_ids`, so they are kept.
    pub fn replace_with_summary(
        &self,
        session_id: &str,
        summarized_ids: &[String],
        summary: &str,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut first_at: Option<i64> = None;
        {
            let mut find =
                tx.prepare("SELECT created_at FROM messages WHERE id=?1 AND session_id=?2")?;
            let mut del = tx.prepare("DELETE FROM messages WHERE id=?1 AND session_id=?2")?;
            for id in summarized_ids {
                let at: Option<i64> =
                    find.query_row(params![id, session_id], |r| r.get(0)).optional()?;
                if let Some(at) = at {
                    first_at = Some(first_at.map_or(at, |f| f.min(at)));
                    del.execute(params![id, session_id])?;
                }
            }
        }
        let Some(first_at) = first_at else {
            // Nothing left to replace (another compaction ran): change nothing.
            anyhow::bail!("the messages to compact no longer exist — nothing changed");
        };
        let parts = serde_json::to_string(&[Part::Text { text: summary.to_string() }])?;
        tx.execute(
            "INSERT INTO messages (id, session_id, role, parts, created_at) VALUES (?1,?2,'system',?3,?4)",
            params![new_id("msg"), session_id, parts, first_at.saturating_sub(1)],
        )?;
        tx.execute("UPDATE sessions SET updated_at=?1 WHERE id=?2", params![now_ms(), session_id])?;
        tx.commit()?;
        Ok(())
    }

    /// Delete message `from_id` and every later message of the chat (rewind).
    /// Returns how many went.
    pub fn delete_messages_from(&self, session_id: &str, from_id: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let (t, row): (i64, i64) = conn.query_row(
            "SELECT created_at, rowid FROM messages WHERE id=?1 AND session_id=?2",
            params![from_id, session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let n = conn.execute(
            "DELETE FROM messages WHERE session_id=?1 AND (created_at > ?2 OR (created_at = ?2 AND rowid >= ?3))",
            params![session_id, t, row],
        )?;
        Ok(n)
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
            // A row we cannot read is shown, never silently emptied.
            let parts: Vec<Part> = serde_json::from_str(&parts_json).unwrap_or_else(|e| {
                vec![Part::Text { text: format!("[unreadable message parts: {e}]") }]
            });
            out.push(Message { id, session_id: session_id.to_string(), role, parts, created_at });
        }
        Ok(out)
    }

    /// Remember an "always allow" answer for one workspace. `workspace` is
    /// [`workspace_key`] of the workspace. Saving the same approval twice
    /// keeps one row (UNIQUE constraint + INSERT OR IGNORE).
    pub fn add_approval(&self, workspace: &str, tool: &str, pattern: &str, scope: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT OR IGNORE INTO approvals (id, workspace, tool, pattern, scope, created_at) \
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![new_id("apr"), workspace, tool, pattern, scope, now_ms()],
        )?;
        Ok(())
    }

    /// True when this workspace has an "always allow" for (tool, pattern).
    /// Approvals saved for other workspaces never count.
    pub fn has_approval(&self, workspace: &str, tool: &str, pattern: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM approvals WHERE workspace=?1 AND tool=?2 AND pattern=?3",
            params![workspace, tool, pattern],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }
}

#[cfg(test)]
mod mission_tests {
    use super::*;

    #[test]
    fn missions_keep_steps_in_order_and_take_inserted_steps() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("m", None).unwrap();
        assert!(db.mission(&s.id).unwrap().is_none());
        db.mission_start(&s.id, "ship it", &["one".into(), "two".into()]).unwrap();
        let m = db.mission(&s.id).unwrap().unwrap();
        assert_eq!((m.goal.as_str(), m.status.as_str()), ("ship it", "running"));
        let first = m.steps[0].id;
        db.mission_step_insert_after(&s.id, first, "one and a half").unwrap();
        db.mission_step_update(first, "done", 1, "did one", Some("child1")).unwrap();
        let m = db.mission(&s.id).unwrap().unwrap();
        let titles: Vec<&str> = m.steps.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["one", "one and a half", "two"]);
        assert_eq!((m.steps[0].status.as_str(), m.steps[0].result.as_str()), ("done", "did one"));
        assert_eq!(m.steps[0].child_id.as_deref(), Some("child1"));
        let last = m.steps[2].id;
        db.mission_step_insert_after(&s.id, last, "four").unwrap();
        assert_eq!(db.mission(&s.id).unwrap().unwrap().steps[3].title, "four");
        db.mission_set_status(&s.id, "paused").unwrap();
        assert_eq!(db.mission(&s.id).unwrap().unwrap().status, "paused");
        // A new mission replaces the old one.
        db.mission_start(&s.id, "again", &["x".into()]).unwrap();
        assert_eq!(db.mission(&s.id).unwrap().unwrap().steps.len(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Projects: one row per folder, new chats go to the default project,
    /// children follow their parent, and pin/archive/move/remove work.
    #[test]
    fn projects_and_chat_organization() {
        let db = Db::open_in_memory().unwrap();
        let dir = std::env::temp_dir();
        let p = db.upsert_project(&dir).unwrap();
        assert_eq!(db.upsert_project(&dir).unwrap(), p, "the same folder is one project");
        assert!(db.create_session("loose", None).unwrap().project_id.is_none(), "no default yet");
        db.set_default_project(Some(&p.id));
        let top = db.create_session("top", None).unwrap();
        assert_eq!(top.project_id.as_deref(), Some(p.id.as_str()));
        let child = db.create_session("sub:worker", Some(&top.id)).unwrap();
        assert_eq!(child.project_id, top.project_id, "a subagent works in its parent's project");

        db.add_message(&top.id, "user", &[Part::Text { text: "hi".into() }]).unwrap();
        let rows = db.list_session_rows().unwrap();
        let row = |id: &str| rows.iter().find(|r| r.session.id == id).unwrap().clone();
        assert!(row(&top.id).has_messages && !row(&child.id).has_messages);

        db.set_session_pinned(&top.id, true).unwrap();
        assert!(db.get_session(&top.id).unwrap().unwrap().pinned);
        db.set_session_archived(&top.id, true).unwrap();
        let s = db.get_session(&top.id).unwrap().unwrap();
        assert!(s.archived && !s.pinned, "archiving also unpins");
        db.set_session_project(&top.id, None).unwrap();
        assert!(db.get_session(&top.id).unwrap().unwrap().project_id.is_none());

        let loose = db.create_session_in("loose 2", None, None).unwrap();
        assert!(db.adopt_unfiled_sessions(&p.id).unwrap() >= 2, "both unfiled top-level chats move");
        assert_eq!(db.get_session(&loose.id).unwrap().unwrap().project_id.as_deref(), Some(p.id.as_str()));

        db.remove_project(&p.id).unwrap();
        assert!(db.list_projects().unwrap().is_empty());
        assert!(db.get_session(&child.id).unwrap().unwrap().project_id.is_none(), "chats stay, unfiled");
    }

    /// A database from before projects gets the new columns in place and
    /// keeps its chats.
    #[test]
    fn old_database_gets_project_columns() {
        let path = std::env::temp_dir().join(format!("ocrs-old-db-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT NOT NULL, parent_id TEXT,
                   created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
                 INSERT INTO sessions VALUES ('ses_old', 'old chat', NULL, 1, 2);",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let s = db.get_session("ses_old").unwrap().unwrap();
        assert_eq!(s.title, "old chat");
        assert!(s.project_id.is_none() && !s.pinned && !s.archived);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

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
        let summarized: Vec<String> = all[..4].iter().map(|m| m.id.clone()).collect();
        db.replace_with_summary(&s.id, &summarized, "[compacted] summary here").unwrap();

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
        db.add_approval("/ws/one", "bash", "git status", "session").unwrap();
        assert!(db.has_approval("/ws/one", "bash", "git status").unwrap());
        assert!(!db.has_approval("/ws/one", "bash", "rm -rf /").unwrap());
    }

    #[test]
    fn approvals_are_scoped_to_one_workspace() {
        let db = Db::open_in_memory().unwrap();
        db.add_approval("/ws/one", "write", "notes.txt", "session").unwrap();
        assert!(db.has_approval("/ws/one", "write", "notes.txt").unwrap());
        assert!(
            !db.has_approval("/ws/two", "write", "notes.txt").unwrap(),
            "another workspace must not inherit the approval"
        );
    }

    #[test]
    fn duplicate_approvals_keep_one_row() {
        let db = Db::open_in_memory().unwrap();
        for _ in 0..3 {
            db.add_approval("/ws/one", "bash", "cargo test", "session").unwrap();
        }
        let n: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM approvals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn old_unscoped_approvals_stop_applying() {
        let path = std::env::temp_dir().join(format!(
            "ocrs-approvals-{}-{}.db",
            std::process::id(),
            now_ms()
        ));
        {
            // A database written by the previous version (no workspace column).
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE approvals (id TEXT PRIMARY KEY, tool TEXT NOT NULL, \
                 pattern TEXT NOT NULL, scope TEXT NOT NULL, created_at INTEGER NOT NULL);
                 INSERT INTO approvals VALUES ('apr_1', 'bash', 'ls', 'session', 0);",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert!(!db.has_approval("/any", "bash", "ls").unwrap(), "old grant is not used");
        db.add_approval("/any", "bash", "ls", "session").unwrap();
        assert!(db.has_approval("/any", "bash", "ls").unwrap());
        drop(db);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn compaction_keeps_messages_that_arrive_meanwhile() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("c", None).unwrap();
        for i in 0..4 {
            db.add_message(&s.id, "user", &[Part::Text { text: format!("m{i}") }]).unwrap();
        }
        let summarized: Vec<String> =
            db.messages(&s.id).unwrap()[..2].iter().map(|m| m.id.clone()).collect();
        // A new message lands while the summary is being written.
        db.add_message(&s.id, "user", &[Part::Text { text: "late".into() }]).unwrap();
        db.replace_with_summary(&s.id, &summarized, "[context compacted] s").unwrap();
        let texts: Vec<String> = db
            .messages(&s.id)
            .unwrap()
            .iter()
            .map(|m| match &m.parts[0] {
                Part::Text { text } => text.clone(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(texts, ["[context compacted] s", "m2", "m3", "late"]);
    }

    #[test]
    fn compaction_of_vanished_messages_changes_nothing() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("c", None).unwrap();
        db.add_message(&s.id, "user", &[Part::Text { text: "hi".into() }]).unwrap();
        assert!(db.replace_with_summary(&s.id, &["msg_gone".to_string()], "x").is_err());
        assert_eq!(db.messages(&s.id).unwrap().len(), 1);
    }

    #[test]
    fn workspace_key_is_the_same_for_two_spellings() {
        let dir = std::env::temp_dir();
        assert_eq!(workspace_key(&dir), workspace_key(&dir.join(".")));
    }

    #[test]
    fn messages_need_an_existing_session() {
        let db = Db::open_in_memory().unwrap();
        let err = db.add_message("ses_missing", "user", &[Part::Text { text: "hi".into() }]);
        assert!(err.is_err(), "foreign keys are enforced");
    }

    #[test]
    fn unreadable_parts_are_shown_not_dropped() {
        let db = Db::open_in_memory().unwrap();
        let s = db.create_session("c", None).unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO messages (id, session_id, role, parts, created_at) VALUES ('m1', ?1, 'user', 'not json', 1)",
                params![s.id],
            )
            .unwrap();
        let msgs = db.messages(&s.id).unwrap();
        assert!(
            matches!(&msgs[0].parts[0], Part::Text { text } if text.starts_with("[unreadable")),
            "{:?}",
            msgs[0].parts
        );
    }

    #[test]
    fn in_memory_schema_matches_the_file_schema() {
        let path = std::env::temp_dir().join(format!("ocrs-schema-{}-{}.db", std::process::id(), now_ms()));
        let file = Db::open(&path).unwrap();
        let mem = Db::open_in_memory().unwrap();
        let sql = |db: &Db| -> String {
            db.conn
                .lock()
                .unwrap()
                .query_row("SELECT sql FROM sqlite_master WHERE name='messages'", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(sql(&file), sql(&mem));
        drop(file);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn ids_are_unique_and_well_formed() {
        let ids: std::collections::HashSet<String> = (0..10_000).map(|_| new_id("ses")).collect();
        assert_eq!(ids.len(), 10_000);
        let one = new_id("msg");
        assert!(one.starts_with("msg_") && one.len() == 20, "{one}");
    }
}
