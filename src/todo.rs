//! M5: per-session todo list. The agent can add, complete, list, and remove
//! tasks so it doesn't lose track during a long session. Stored in SQLite
//! (the session database), keyed by session id, so todos survive a restart.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

/// One todo item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub completed: bool,
}

/// Operation the model requests.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TodoOp {
    Add { content: String },
    List,
    Complete { id: String },
    Remove { id: String },
}

/// Result of a todo operation, returned to the model as a tool result.
#[derive(Debug, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TodoResult {
    Added { item: TodoItem },
    Listed { items: Vec<TodoItem> },
    Completed { item: TodoItem },
    Removed { id: String },
    NotFound { id: String },
    Failed { error: String },
}

fn next_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0)
        .hash(&mut h);
    std::thread::current().id().hash(&mut h);
    static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed).hash(&mut h);
    format!("todo_{:016x}", h.finish())
}

/// Per-session todo store in SQLite, so a restart keeps the list.
/// `new()`/`default()` keep it in memory; `open(path)` uses a database file.
pub struct TodoStore {
    conn: Mutex<Connection>,
}

const TODO_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS todos (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    content TEXT NOT NULL,
    completed INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_todos_session ON todos(session_id, created_at);";

impl Default for TodoStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TodoStore {
    /// In-memory store (tests, ephemeral runs).
    pub fn new() -> Self {
        let conn = Connection::open_in_memory().expect("in-memory SQLite");
        conn.execute_batch(TODO_SCHEMA).expect("todo table");
        Self { conn: Mutex::new(conn) }
    }

    /// Store kept in the database file at `path` (the same file as `Db`).
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(TODO_SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn exec(&self, session_id: &str, op: TodoOp) -> TodoResult {
        let conn = self.conn.lock().unwrap();
        exec_sql(&conn, session_id, op).unwrap_or_else(|e| TodoResult::Failed { error: e.to_string() })
    }


    /// Snapshot all todos for a session (used by `/todos` command or display).
    pub fn list_all(&self, session_id: &str) -> Vec<TodoItem> {
        let conn = self.conn.lock().unwrap();
        list_sql(&conn, session_id).unwrap_or_default()
    }
}

fn row_item(r: &rusqlite::Row) -> rusqlite::Result<TodoItem> {
    Ok(TodoItem { id: r.get(0)?, content: r.get(1)?, completed: r.get::<_, i64>(2)? != 0 })
}

fn list_sql(conn: &Connection, session_id: &str) -> rusqlite::Result<Vec<TodoItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, content, completed FROM todos WHERE session_id=?1 ORDER BY created_at, rowid",
    )?;
    let rows = stmt.query_map(params![session_id], row_item)?;
    rows.collect()
}

/// The real id for what the model wrote: the id itself, a list position
/// ("1", "#2": models often count instead of copying the id), or a unique
/// start of an id. `None` = no such todo.
fn resolve_id(conn: &Connection, session_id: &str, id: &str) -> rusqlite::Result<Option<String>> {
    let items = list_sql(conn, session_id)?;
    let id = id.trim();
    if items.iter().any(|i| i.id == id) {
        return Ok(Some(id.to_string()));
    }
    if let Ok(n) = id.trim_start_matches('#').parse::<usize>() {
        return Ok(n.checked_sub(1).and_then(|i| items.get(i)).map(|i| i.id.clone()));
    }
    let mut starts = items.iter().filter(|i| !id.is_empty() && i.id.starts_with(id));
    Ok(match (starts.next(), starts.next()) {
        (Some(one), None) => Some(one.id.clone()),
        _ => None,
    })
}

fn exec_sql(conn: &Connection, session_id: &str, op: TodoOp) -> rusqlite::Result<TodoResult> {
    let find = |id: &str| {
        conn.query_row(
            "SELECT id, content, completed FROM todos WHERE session_id=?1 AND id=?2",
            params![session_id, id],
            row_item,
        )
        .optional()
    };
    Ok(match op {
        TodoOp::Add { content } => {
            let item = TodoItem { id: next_id(), content, completed: false };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            conn.execute(
                "INSERT INTO todos (id, session_id, content, completed, created_at) VALUES (?1,?2,?3,0,?4)",
                params![item.id, session_id, item.content, now],
            )?;
            TodoResult::Added { item }
        }
        TodoOp::List => TodoResult::Listed { items: list_sql(conn, session_id)? },
        TodoOp::Complete { id } => {
            let Some(id) = resolve_id(conn, session_id, &id)? else {
                return Ok(TodoResult::NotFound { id });
            };
            conn.execute(
                "UPDATE todos SET completed=1 WHERE session_id=?1 AND id=?2",
                params![session_id, id],
            )?;
            match find(&id)? {
                Some(item) => TodoResult::Completed { item },
                None => TodoResult::NotFound { id },
            }
        }
        TodoOp::Remove { id } => {
            let Some(id) = resolve_id(conn, session_id, &id)? else {
                return Ok(TodoResult::NotFound { id });
            };
            let n = conn.execute("DELETE FROM todos WHERE session_id=?1 AND id=?2", params![session_id, id])?;
            if n > 0 { TodoResult::Removed { id } } else { TodoResult::NotFound { id } }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_or_id_start_finds_the_todo() {
        let store = TodoStore::new();
        let sid = "s";
        let mut ids = Vec::new();
        for c in ["one", "two"] {
            let TodoResult::Added { item } = store.exec(sid, TodoOp::Add { content: c.into() }) else { panic!() };
            ids.push(item.id);
        }
        let TodoResult::Completed { item } = store.exec(sid, TodoOp::Complete { id: "2".into() }) else {
            panic!("position 2")
        };
        assert_eq!(item.content, "two");
        let TodoResult::Removed { id } = store.exec(sid, TodoOp::Remove { id: "#1".into() }) else { panic!("#1") };
        assert_eq!(id, ids[0]);
        assert!(matches!(store.exec(sid, TodoOp::Complete { id: "5".into() }), TodoResult::NotFound { .. }));
        let start: String = ids[1].chars().take(4).collect();
        assert!(matches!(store.exec(sid, TodoOp::Complete { id: start }), TodoResult::Completed { .. }));
    }

    #[test]
    fn add_list_complete_remove() {
        let store = TodoStore::new();
        let sid = "ses_1";

        let TodoResult::Added { item } = store.exec(sid, TodoOp::Add { content: "write code".into() })
        else { panic!("expected added") };
        assert!(!item.completed);
        assert_eq!(item.content, "write code");

        let TodoResult::Listed { items } = store.exec(sid, TodoOp::List) else { panic!("expected listed") };
        assert_eq!(items.len(), 1);

        let TodoResult::Completed { item } = store.exec(sid, TodoOp::Complete { id: item.id.clone() }) else {
            panic!("expected completed")
        };
        assert!(item.completed);

        let TodoResult::Listed { items } = store.exec(sid, TodoOp::List) else { panic!("expected listed") };
        assert_eq!(items.len(), 1);
        assert!(items[0].completed);

        let TodoResult::Removed { id } = store.exec(sid, TodoOp::Remove { id: item.id.clone() }) else {
            panic!("expected removed")
        };
        assert_eq!(id, item.id);

        let TodoResult::Listed { items } = store.exec(sid, TodoOp::List) else { panic!("expected listed") };
        assert!(items.is_empty());

        let TodoResult::NotFound { id } = store.exec(sid, TodoOp::Complete { id: "nope".into() }) else {
            panic!("expected not found")
        };
        assert_eq!(id, "nope");
    }

    #[test]
    fn sessions_are_isolated() {
        let store = TodoStore::new();
        store.exec("a", TodoOp::Add { content: "a1".into() });
        store.exec("b", TodoOp::Add { content: "b1".into() });
        let TodoResult::Listed { items } = store.exec("a", TodoOp::List) else { panic!() };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].content, "a1");
    }

    #[test]
    fn todos_survive_a_restart() {
        let path = std::env::temp_dir().join(format!("ocrs-todos-{}.db", std::process::id()));
        {
            let store = TodoStore::open(&path).unwrap();
            store.exec("s1", TodoOp::Add { content: "write tests".into() });
        }
        let store = TodoStore::open(&path).unwrap();
        let items = store.list_all("s1");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].content, "write tests");
        assert!(store.list_all("s2").is_empty(), "sessions stay separate");
        drop(store);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
