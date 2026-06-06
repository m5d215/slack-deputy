//! SQLite store. The single `messages` table is the canonical inbound sink —
//! only Slack-originated events flow through it. WAL mode so the daemon (writer)
//! and the consumer CLI (reader / status updater) can share one file.
//!
//! `meta` is a tiny kv table for daemon state that must survive restarts
//! (e.g. the learned self-post bot_id used for echo suppression).

use rusqlite::{Connection, OptionalExtension, params};
use std::sync::Mutex;

/// Default DB location: `$XDG_CONFIG_HOME/slack-deputy/slack-deputy.db`, falling
/// back to `~/.config/slack-deputy/slack-deputy.db`. A fixed absolute path so the
/// daemon (writer) and the consumer CLI (reader) agree regardless of cwd — the
/// CLI runs from subagent shells whose cwd resets between calls.
fn default_path() -> String {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.config")))
        .unwrap_or_else(|| ".".to_string());
    format!("{base}/slack-deputy/slack-deputy.db")
}

/// The DB path to use: `SLACK_DEPUTY_DB` if set, else [`default_path`]. The parent
/// directory is created so the path is usable as-is on first run.
pub fn resolve_path() -> String {
    let path = std::env::var("SLACK_DEPUTY_DB").unwrap_or_else(|_| default_path());
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    path
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS messages (
    pk         INTEGER PRIMARY KEY AUTOINCREMENT,
    kind       TEXT NOT NULL,                    -- message | reaction | confirmation
    channel    TEXT,
    thread_ts  TEXT,                             -- partition key
    ts         TEXT NOT NULL,                    -- Slack ts (FIFO order)
    body       TEXT NOT NULL,                    -- event payload JSON (untrusted)
    status     TEXT NOT NULL DEFAULT 'pending',  -- pending | dispatched | done | awaiting_human | ambient
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_messages_pending ON messages(status, ts);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// A row summary for `tail` (no created_at/ts; just what's worth watching).
/// Serde so the daemon can ship it to a `tail` client over HTTP.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Row {
    pub pk: i64,
    pub kind: String,
    pub channel: Option<String>,
    pub status: String,
    pub body: String,
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        // WAL lets the consumer CLI read / update status while the daemon writes.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=3000;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert an inbound event as a `pending` row. Returns the new pk.
    pub fn insert(
        &self,
        kind: &str,
        channel: Option<&str>,
        thread_ts: Option<&str>,
        ts: &str,
        body: &str,
        created_at: &str,
    ) -> rusqlite::Result<i64> {
        self.insert_with_status(kind, channel, thread_ts, ts, body, "pending", created_at)
    }

    /// Insert an inbound event with an explicit status. `ambient` rows are stored
    /// as pull-only context and are never dispatched — `claim_next` only claims
    /// `pending` — so the consumer pays no per-event cost for them.
    pub fn insert_with_status(
        &self,
        kind: &str,
        channel: Option<&str>,
        thread_ts: Option<&str>,
        ts: &str,
        body: &str,
        status: &str,
        created_at: &str,
    ) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "INSERT INTO messages (kind, channel, thread_ts, ts, body, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![kind, channel, thread_ts, ts, body, status, created_at],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// True if we've already captured this thread (its root or any reply is
    /// stored, ambient or not). Capture scope uses this to keep following a
    /// thread as *context* once it's on our radar.
    pub fn thread_tracked(&self, thread_ts: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM messages WHERE ts = ?1 OR thread_ts = ?1 LIMIT 1",
                params![thread_ts],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// True if this thread already holds a *directed* (non-`ambient`) row. Capture
    /// scope uses this so a reply becomes dispatchable only when a directed event
    /// put the thread on our radar — watching a channel (ambient) must not
    /// transitively dispatch every reply in its threads.
    pub fn thread_has_directed(&self, thread_ts: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM messages
                 WHERE (ts = ?1 OR thread_ts = ?1) AND status != 'ambient' LIMIT 1",
                params![thread_ts],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    // --- queue verbs (CLI, SQLite-direct) ---

    /// Atomically claim the oldest pending row whose partition has no in-flight
    /// (dispatched) sibling. Marks it `dispatched`. Returns (pk, thread_ts).
    /// Body is intentionally NOT returned — the dispatcher never reads it.
    pub fn claim_next(&self) -> rusqlite::Result<Option<(i64, Option<String>)>> {
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        let tx = conn.transaction()?;
        let row: Option<(i64, Option<String>)> = tx
            .query_row(
                "SELECT pk, thread_ts FROM messages
                 WHERE status = 'pending'
                   AND (thread_ts IS NULL
                        OR thread_ts NOT IN (SELECT thread_ts FROM messages
                                             WHERE status = 'dispatched' AND thread_ts IS NOT NULL))
                 ORDER BY ts ASC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((pk, _)) = &row {
            tx.execute("UPDATE messages SET status = 'dispatched' WHERE pk = ?1", params![pk])?;
        }
        tx.commit()?;
        Ok(row)
    }

    /// Highest pk (0 if empty). For `tail` to start from "now".
    pub fn max_pk(&self) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.query_row("SELECT COALESCE(MAX(pk), 0) FROM messages", [], |r| r.get(0))
    }

    /// Rows with pk > after, oldest first. For `tail` to follow new captures.
    pub fn rows_after(&self, after: i64) -> rusqlite::Result<Vec<Row>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT pk, kind, channel, status, body FROM messages WHERE pk > ?1 ORDER BY pk ASC",
        )?;
        let rows = stmt.query_map(params![after], |r| {
            Ok(Row {
                pk: r.get(0)?,
                kind: r.get(1)?,
                channel: r.get(2)?,
                status: r.get(3)?,
                body: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn get_body(&self, pk: i64) -> rusqlite::Result<Option<String>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.query_row("SELECT body FROM messages WHERE pk = ?1", params![pk], |r| r.get(0))
            .optional()
    }

    pub fn set_status(&self, pk: i64, status: &str) -> rusqlite::Result<usize> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute("UPDATE messages SET status = ?2 WHERE pk = ?1", params![pk, status])
    }

    // --- meta kv ---

    pub fn meta_get(&self, key: &str) -> rusqlite::Result<Option<String>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0))
            .optional()
    }

    pub fn meta_set(&self, key: &str, value: &str) -> rusqlite::Result<usize> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
    }
}
