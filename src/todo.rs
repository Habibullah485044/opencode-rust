//! M5: per-session todo list. The agent can add, complete, list, and remove
//! tasks so it doesn't lose track during a long session. Stored in memory,
//! keyed by session id. (A future phase could persist these to SQLite.)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

/// In-memory per-session todo store.
pub struct TodoStore {
    todos: Mutex<HashMap<String, Vec<TodoItem>>>,
}

impl Default for TodoStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TodoStore {
    pub fn new() -> Self {
        Self {
            todos: Mutex::new(HashMap::new()),
        }
    }

    pub fn exec(&self, session_id: &str, op: TodoOp) -> TodoResult {
        let mut guard = self.todos.lock().unwrap();
        let items = guard.entry(session_id.to_string()).or_default();
        match op {
            TodoOp::Add { content } => {
                let item = TodoItem {
                    id: next_id(),
                    content,
                    completed: false,
                };
                items.push(item.clone());
                TodoResult::Added { item }
            }
            TodoOp::List => TodoResult::Listed {
                items: items.clone(),
            },
            TodoOp::Complete { id } => match items.iter_mut().find(|i| i.id == id) {
                Some(item) => {
                    item.completed = true;
                    TodoResult::Completed { item: item.clone() }
                }
                None => TodoResult::NotFound { id },
            },
            TodoOp::Remove { id } => {
                let pos = items.iter().position(|i| i.id == id);
                match pos {
                    Some(i) => {
                        items.remove(i);
                        TodoResult::Removed { id }
                    }
                    None => TodoResult::NotFound { id },
                }
            }
        }
    }

    /// Snapshot all todos for a session (used by `/todos` command or display).
    pub fn list_all(&self, session_id: &str) -> Vec<TodoItem> {
        self.todos
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
