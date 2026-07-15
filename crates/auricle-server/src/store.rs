//! SQLite persistence (rusqlite, WAL) with embedded versioned migrations.
//! Schema per architecture §4.7.

use std::path::Path;
use std::sync::Mutex;

use auricle_core::{ChannelId, Error, Result};
use rusqlite::Connection;
use serde::Serialize;

/// Versioned migrations; `PRAGMA user_version` tracks the applied count.
/// Append-only — never edit an entry that has shipped.
const MIGRATIONS: &[&str] = &[
    // v1: initial schema (architecture §4.7)
    "CREATE TABLE sessions(
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        started_at INT NOT NULL,
        ended_at INT,
        stt_provider TEXT NOT NULL,
        llm_provider TEXT,
        meta TEXT NOT NULL DEFAULT '{}'
     );
     CREATE TABLE segments(
        id INTEGER PRIMARY KEY,
        session_id TEXT NOT NULL,
        channel INT NOT NULL,
        speaker TEXT NOT NULL,
        t_start_ms INT NOT NULL,
        t_end_ms INT NOT NULL,
        text TEXT NOT NULL,
        provider TEXT NOT NULL
     );
     CREATE INDEX idx_segments_session ON segments(session_id, t_start_ms);
     CREATE TABLE summaries(
        id INTEGER PRIMARY KEY,
        session_id TEXT NOT NULL,
        template TEXT NOT NULL,
        model TEXT NOT NULL,
        content TEXT NOT NULL,
        created_at INT NOT NULL
     );
     CREATE TABLE settings(
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
     );",
];

#[derive(Debug, Clone, Serialize)]
pub struct SessionRow {
    pub id: String,
    pub title: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub stt_provider: String,
    pub meta: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct SummaryRow {
    pub id: i64,
    pub template: String,
    pub model: String,
    pub content: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SegmentRow {
    pub channel: u8,
    pub speaker: String,
    pub t_start_ms: i64,
    pub t_end_ms: i64,
    pub text: String,
    pub provider: String,
}

pub fn channel_code(channel: ChannelId) -> u8 {
    match channel {
        ChannelId::Mic => 0,
        ChannelId::Loopback => 1,
    }
}

pub struct Store {
    conn: Mutex<Connection>,
}

fn db_err(e: rusqlite::Error) -> Error {
    Error::Io(std::io::Error::other(format!("sqlite: {e}")))
}

impl Store {
    /// Open (creating if needed) with WAL mode and apply pending migrations.
    ///
    /// Deliberately does NOT run crash recovery: readers like `auricle
    /// export` open this database while a daemon may be mid-recording, and
    /// recovery would close the live session out from under it. The engine
    /// calls [`recover_interrupted`](Self::recover_interrupted) itself.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).map_err(db_err)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(db_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(db_err)?;
        let store = Store {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(db_err)?;
        for (i, sql) in MIGRATIONS.iter().enumerate() {
            if version <= i as i64 {
                conn.execute_batch(sql).map_err(db_err)?;
                conn.pragma_update(None, "user_version", (i + 1) as i64)
                    .map_err(db_err)?;
            }
        }
        Ok(())
    }

    /// Crash recovery: a session with no `ended_at` was interrupted (the
    /// engine always sets it on clean stop). Close it at the time of its
    /// last persisted segment (or its start) and flag it in meta. Called by
    /// the engine at startup — only the daemon owns the lifecycle, so only
    /// it may declare an open session dead.
    pub(crate) fn recover_interrupted(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "UPDATE sessions SET
                ended_at = COALESCE(
                    (SELECT started_at + MAX(t_end_ms)/1000 FROM segments
                      WHERE segments.session_id = sessions.id),
                    started_at),
                meta = json_set(meta, '$.interrupted', json('true'))
             WHERE ended_at IS NULL;",
        )
        .map_err(db_err)?;
        Ok(())
    }

    pub fn create_session(
        &self,
        id: &str,
        title: &str,
        started_at: i64,
        stt_provider: &str,
        meta: &serde_json::Value,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions(id, title, started_at, stt_provider, meta)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, title, started_at, stt_provider, meta.to_string()],
        )
        .map_err(db_err)?;
        Ok(())
    }

    pub fn end_session(&self, id: &str, ended_at: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET ended_at = ?2 WHERE id = ?1",
            rusqlite::params![id, ended_at],
        )
        .map_err(db_err)?;
        Ok(())
    }

    pub fn update_session_meta(&self, id: &str, meta: &serde_json::Value) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET meta = ?2 WHERE id = ?1",
            rusqlite::params![id, meta.to_string()],
        )
        .map_err(db_err)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_segment(
        &self,
        session_id: &str,
        channel: ChannelId,
        speaker: &str,
        t_start_ms: i64,
        t_end_ms: i64,
        text: &str,
        provider: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO segments(session_id, channel, speaker, t_start_ms, t_end_ms, text, provider)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                session_id,
                channel_code(channel),
                speaker,
                t_start_ms,
                t_end_ms,
                text,
                provider
            ],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// List sessions, newest first. With `query`, only sessions whose title
    /// OR transcript text contains it (case-insensitive SQLite LIKE).
    pub fn list_sessions(&self, query: Option<&str>) -> Result<Vec<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        let rows = match query.map(str::trim).filter(|q| !q.is_empty()) {
            None => {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, title, started_at, ended_at, stt_provider, meta
                         FROM sessions ORDER BY started_at DESC",
                    )
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map([], row_to_session)
                    .map_err(db_err)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(db_err)?;
                rows
            }
            Some(q) => {
                let pattern = format!(
                    "%{}%",
                    q.replace('\\', "\\\\")
                        .replace('%', "\\%")
                        .replace('_', "\\_")
                );
                let mut stmt = conn
                    .prepare(
                        "SELECT id, title, started_at, ended_at, stt_provider, meta
                         FROM sessions
                         WHERE title LIKE ?1 ESCAPE '\\'
                            OR EXISTS (SELECT 1 FROM segments
                                        WHERE segments.session_id = sessions.id
                                          AND segments.text LIKE ?1 ESCAPE '\\')
                         ORDER BY started_at DESC",
                    )
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map([&pattern], row_to_session)
                    .map_err(db_err)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(db_err)?;
                rows
            }
        };
        Ok(rows)
    }

    /// Rename a session. Returns false when the id does not exist.
    pub fn rename_session(&self, id: &str, title: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute(
                "UPDATE sessions SET title = ?2 WHERE id = ?1",
                rusqlite::params![id, title],
            )
            .map_err(db_err)?;
        Ok(n > 0)
    }

    /// Delete a session and its segments/summaries. Returns false when the
    /// id does not exist. (Retained audio files are the caller's job — the
    /// store only owns database rows.)
    pub fn delete_session(&self, id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute("DELETE FROM segments WHERE session_id = ?1", [id])
            .map_err(db_err)?;
        tx.execute("DELETE FROM summaries WHERE session_id = ?1", [id])
            .map_err(db_err)?;
        let n = tx
            .execute("DELETE FROM sessions WHERE id = ?1", [id])
            .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok(n > 0)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, title, started_at, ended_at, stt_provider, meta
                 FROM sessions WHERE id = ?1",
            )
            .map_err(db_err)?;
        let mut rows = stmt
            .query_map([id], row_to_session)
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        Ok(rows.pop())
    }

    pub fn get_segments(&self, session_id: &str) -> Result<Vec<SegmentRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT channel, speaker, t_start_ms, t_end_ms, text, provider
                 FROM segments WHERE session_id = ?1 ORDER BY t_start_ms, id",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([session_id], |r| {
                Ok(SegmentRow {
                    channel: r.get(0)?,
                    speaker: r.get(1)?,
                    t_start_ms: r.get(2)?,
                    t_end_ms: r.get(3)?,
                    text: r.get(4)?,
                    provider: r.get(5)?,
                })
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        Ok(rows)
    }

    pub fn insert_summary(
        &self,
        session_id: &str,
        template: &str,
        model: &str,
        content: &str,
        created_at: i64,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO summaries(session_id, template, model, content, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![session_id, template, model, content, created_at],
        )
        .map_err(db_err)?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get_summaries(&self, session_id: &str) -> Result<Vec<SummaryRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, template, model, content, created_at
                 FROM summaries WHERE session_id = ?1 ORDER BY created_at, id",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([session_id], |r| {
                Ok(SummaryRow {
                    id: r.get(0)?,
                    template: r.get(1)?,
                    model: r.get(2)?,
                    content: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        Ok(rows)
    }

    /// Persist one ask (question + optional screen context + answer).
    ///
    /// Deliberately NOT part of [`MIGRATIONS`]: the `asks` table exists
    /// only after the first persisted ask, and the caller may call this
    /// only when `copilot.retain_context = true`. With retention off
    /// (the default) nothing question- or screen-derived ever reaches
    /// this database — the table is never even created.
    pub fn insert_ask(
        &self,
        session_id: Option<&str>,
        question: &str,
        screen_context: Option<&str>,
        answer: &str,
        provider: &str,
        created_at: i64,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS asks(
                id INTEGER PRIMARY KEY,
                session_id TEXT,
                question TEXT NOT NULL,
                screen_context TEXT,
                answer TEXT NOT NULL,
                provider TEXT NOT NULL,
                created_at INT NOT NULL
             );",
        )
        .map_err(db_err)?;
        conn.execute(
            "INSERT INTO asks(session_id, question, screen_context, answer, provider, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                session_id,
                question,
                screen_context,
                answer,
                provider,
                created_at
            ],
        )
        .map_err(db_err)?;
        Ok(conn.last_insert_rowid())
    }

    /// Test/verification surface: does the `asks` table exist, and how
    /// many rows does it hold? `None` = the table was never created.
    pub fn asks_count(&self) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='asks'",
                [],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        if exists == 0 {
            return Ok(None);
        }
        conn.query_row("SELECT COUNT(*) FROM asks", [], |r| r.get(0))
            .map(Some)
            .map_err(db_err)
    }

    pub fn set_setting(&self, key: &str, value: &serde_json::Value) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value.to_string()],
        )
        .map_err(db_err)?;
        Ok(())
    }

    pub fn all_settings(&self) -> Result<serde_json::Map<String, serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT key, value FROM settings ORDER BY key")
            .map_err(db_err)?;
        let mut out = serde_json::Map::new();
        let rows = stmt
            .query_map([], |r| {
                let k: String = r.get(0)?;
                let v: String = r.get(1)?;
                Ok((k, v))
            })
            .map_err(db_err)?;
        for row in rows {
            let (k, v) = row.map_err(db_err)?;
            let value = serde_json::from_str(&v).unwrap_or(serde_json::Value::String(v));
            out.insert(k, value);
        }
        Ok(out)
    }
}

fn row_to_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    let meta_text: String = r.get(5)?;
    Ok(SessionRow {
        id: r.get(0)?,
        title: r.get(1)?,
        started_at: r.get(2)?,
        ended_at: r.get(3)?,
        stt_provider: r.get(4)?,
        meta: serde_json::from_str(&meta_text).unwrap_or(serde_json::json!({})),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("auricle-store-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn migrations_are_idempotent_across_reopens() {
        let path = temp_db("migrate");
        {
            let store = Store::open(&path).unwrap();
            store
                .create_session("s1", "t", 100, "whisper-local", &serde_json::json!({}))
                .unwrap();
            store.end_session("s1", 160).unwrap();
        }
        // Second open must not re-run v1 (CREATE TABLE would fail).
        let store = Store::open(&path).unwrap();
        assert_eq!(store.list_sessions(None).unwrap().len(), 1);
    }

    #[test]
    fn session_and_segment_crud_roundtrip() {
        let path = temp_db("crud");
        let store = Store::open(&path).unwrap();
        store
            .create_session(
                "s1",
                "standup",
                1_700_000_000,
                "deepgram",
                &serde_json::json!({"devices": {"mic": "default"}}),
            )
            .unwrap();
        store
            .insert_segment("s1", ChannelId::Mic, "You", 1000, 2500, "hello", "deepgram")
            .unwrap();
        store
            .insert_segment(
                "s1",
                ChannelId::Loopback,
                "Them",
                500,
                900,
                "hi there",
                "deepgram",
            )
            .unwrap();
        store.end_session("s1", 1_700_000_060).unwrap();

        let s = store.get_session("s1").unwrap().unwrap();
        assert_eq!(s.title, "standup");
        assert_eq!(s.ended_at, Some(1_700_000_060));
        assert_eq!(s.meta["devices"]["mic"], "default");

        let segs = store.get_segments("s1").unwrap();
        assert_eq!(segs.len(), 2);
        // Ordered by t_start_ms across channels.
        assert_eq!(segs[0].speaker, "Them");
        assert_eq!(segs[0].channel, 1);
        assert_eq!(segs[1].speaker, "You");
        assert_eq!(segs[1].channel, 0);

        assert!(store.get_session("nope").unwrap().is_none());
    }

    #[test]
    fn crash_recovery_marks_open_sessions_ended() {
        let path = temp_db("recover");
        {
            let store = Store::open(&path).unwrap();
            store
                .create_session(
                    "dead",
                    "crashed",
                    1000,
                    "whisper-local",
                    &serde_json::json!({}),
                )
                .unwrap();
            store
                .insert_segment("dead", ChannelId::Mic, "You", 0, 42_000, "last words", "w")
                .unwrap();
            // No end_session: simulates a hard kill mid-session.
        }
        // Reopen as the engine does: recovery is explicit.
        let store = Store::open(&path).unwrap();
        store.recover_interrupted().unwrap();
        let s = store.get_session("dead").unwrap().unwrap();
        assert_eq!(
            s.ended_at,
            Some(1000 + 42),
            "ended at last segment's end time"
        );
        assert_eq!(s.meta["interrupted"], true);
        // Segments written before the crash are readable.
        assert_eq!(store.get_segments("dead").unwrap().len(), 1);
    }

    #[test]
    fn read_only_open_never_touches_a_live_session() {
        // `auricle export` opens the DB while a daemon may be recording; a
        // plain open must not close or flag the open session.
        let path = temp_db("readonly-open");
        let daemon = Store::open(&path).unwrap();
        daemon
            .create_session(
                "live",
                "in progress",
                1000,
                "deepgram",
                &serde_json::json!({}),
            )
            .unwrap();

        let reader = Store::open(&path).unwrap();
        let s = reader.get_session("live").unwrap().unwrap();
        assert_eq!(s.ended_at, None, "reader must not end the live session");
        assert_eq!(s.meta.get("interrupted"), None, "no interrupted flag");
    }

    #[test]
    fn search_matches_title_and_transcript() {
        let path = temp_db("search");
        let store = Store::open(&path).unwrap();
        store
            .create_session("s1", "Weekly sync", 100, "deepgram", &serde_json::json!({}))
            .unwrap();
        store
            .insert_segment("s1", ChannelId::Mic, "You", 0, 1000, "budget approved", "d")
            .unwrap();
        store
            .create_session(
                "s2",
                "1:1 with Sam",
                200,
                "deepgram",
                &serde_json::json!({}),
            )
            .unwrap();
        store
            .insert_segment(
                "s2",
                ChannelId::Loopback,
                "Them",
                0,
                1000,
                "promotion cycle",
                "d",
            )
            .unwrap();

        // Title match, case-insensitive.
        let hits = store.list_sessions(Some("weekly")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "s1");
        // Transcript-content match.
        let hits = store.list_sessions(Some("promotion")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "s2");
        // No match; blank query = all, newest first.
        assert!(store.list_sessions(Some("zzz")).unwrap().is_empty());
        assert_eq!(store.list_sessions(Some("  ")).unwrap().len(), 2);
        // LIKE wildcards in user input are literals, not wildcards.
        assert!(store.list_sessions(Some("%")).unwrap().is_empty());
    }

    #[test]
    fn rename_and_delete_sessions() {
        let path = temp_db("rename-delete");
        let store = Store::open(&path).unwrap();
        store
            .create_session("s1", "old name", 100, "deepgram", &serde_json::json!({}))
            .unwrap();
        store
            .insert_segment("s1", ChannelId::Mic, "You", 0, 1000, "text", "d")
            .unwrap();
        store
            .insert_summary("s1", "minutes", "m", "content", 1)
            .unwrap();

        assert!(store.rename_session("s1", "new name").unwrap());
        assert_eq!(store.get_session("s1").unwrap().unwrap().title, "new name");
        assert!(!store.rename_session("nope", "x").unwrap());

        assert!(store.delete_session("s1").unwrap());
        assert!(store.get_session("s1").unwrap().is_none());
        assert!(store.get_segments("s1").unwrap().is_empty());
        assert!(store.get_summaries("s1").unwrap().is_empty());
        assert!(!store.delete_session("s1").unwrap());
    }

    #[test]
    fn summaries_roundtrip_in_creation_order() {
        let path = temp_db("summaries");
        let store = Store::open(&path).unwrap();
        store
            .create_session("s1", "t", 100, "deepgram", &serde_json::json!({}))
            .unwrap();
        store
            .insert_summary("s1", "minutes", "llama-3.3-70b", "## Minutes\n- a", 200)
            .unwrap();
        store
            .insert_summary("s1", "action-items", "llama-3.3-70b", "- [ ] ship", 300)
            .unwrap();

        let rows = store.get_summaries("s1").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].template, "minutes");
        assert_eq!(rows[1].template, "action-items");
        assert!(rows[0].id > 0);
        assert!(store.get_summaries("other").unwrap().is_empty());
    }

    #[test]
    fn asks_table_does_not_exist_until_the_first_persisted_ask() {
        let path = temp_db("asks");
        let store = Store::open(&path).unwrap();
        // Opening (and migrating) the store must not create the table:
        // with retain_context off, insert_ask is never called and the
        // schema itself carries no trace of the copilot.
        assert_eq!(store.asks_count().unwrap(), None);

        let id = store
            .insert_ask(
                Some("s1"),
                "what's on screen?",
                Some("{\"window_title\":\"Jira\"}"),
                "A sprint board.",
                "ollama",
                1_700_000_000,
            )
            .unwrap();
        assert!(id > 0);
        let id2 = store
            .insert_ask(
                None,
                "and without a session?",
                None,
                "Works.",
                "groq",
                1_700_000_001,
            )
            .unwrap();
        assert!(id2 > id);
        assert_eq!(store.asks_count().unwrap(), Some(2));

        // Reopen: still there, migrations still clean.
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.asks_count().unwrap(), Some(2));
    }

    #[test]
    fn settings_upsert_and_read_back() {
        let path = temp_db("settings");
        let store = Store::open(&path).unwrap();
        store
            .set_setting("default_provider", &serde_json::json!("deepgram"))
            .unwrap();
        store
            .set_setting("retain_raw_audio", &serde_json::json!(true))
            .unwrap();
        store
            .set_setting("default_provider", &serde_json::json!("whisper-local"))
            .unwrap();

        let all = store.all_settings().unwrap();
        assert_eq!(all["default_provider"], "whisper-local");
        assert_eq!(all["retain_raw_audio"], true);
    }
}
