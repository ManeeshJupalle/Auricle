//! SQLite persistence (rusqlite, WAL) with embedded versioned migrations.
//! Schema per architecture §4.7.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use auricle_core::{ChannelId, Error, Result};
use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior};
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
    // v2: egress ledger — one row per time user data left the machine to a
    // third party (or explicitly stayed local). Metadata only: provider,
    // host, rough size — never the audio, prompt, or answer itself.
    "CREATE TABLE egress(
        id INTEGER PRIMARY KEY,
        ts INT NOT NULL,
        session_id TEXT,
        destination TEXT NOT NULL,
        provider TEXT NOT NULL,
        host TEXT,
        kind TEXT NOT NULL,
        items INT,
        detail TEXT
     );
     CREATE INDEX idx_egress_ts ON egress(ts DESC);",
    // v3: running egress counts per (destination, counterparty).
    //
    // The dashboard's headline — "nothing has left this machine" — is a claim
    // about the whole ledger, so it cannot read a page. Deriving it with a
    // GROUP BY over `egress` is a full scan plus a temporary B-tree on every
    // dashboard load, and agent rows arrive fast enough (one per MCP tool
    // call) to make that seconds of work while holding the store mutex.
    // Maintained on insert instead, and seeded here from whatever is already
    // recorded so an upgrade keeps its history.
    "CREATE TABLE egress_totals(
        destination TEXT NOT NULL,
        who TEXT NOT NULL,
        count INT NOT NULL,
        PRIMARY KEY (destination, who)
     );
     INSERT INTO egress_totals(destination, who, count)
        SELECT destination, COALESCE(host, provider), COUNT(*)
        FROM egress GROUP BY destination, COALESCE(host, provider);",
    // v4: `asks`, until now created by the first `insert_ask`, so that
    // `delete_session` can rely on it. IF NOT EXISTS because a database that
    // has retained an ask already has the table.
    //
    // Then clear what earlier deletes left behind: they never touched `asks`,
    // and a summary could land after its session had gone. The IS NOT NULL
    // matters: `NULL NOT IN (empty set)` is true, so with no sessions left a
    // bare NOT IN would also take asks that never belonged to a meeting.
    "CREATE TABLE IF NOT EXISTS asks(
        id INTEGER PRIMARY KEY,
        session_id TEXT,
        question TEXT NOT NULL,
        screen_context TEXT,
        answer TEXT NOT NULL,
        provider TEXT NOT NULL,
        created_at INT NOT NULL
     );
     DELETE FROM asks WHERE session_id IS NOT NULL
        AND session_id NOT IN (SELECT id FROM sessions);
     DELETE FROM summaries WHERE session_id NOT IN (SELECT id FROM sessions);",
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

/// One page of sessions. `total` counts every session, not just this page.
#[derive(Debug, Clone, Serialize)]
pub struct SessionPage {
    pub sessions: Vec<SessionRow>,
    pub total: i64,
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

/// One transcript line matching a search, carrying enough context to be
/// read on its own: which meeting it came from and when inside it.
#[derive(Debug, Clone, Serialize)]
pub struct SegmentHit {
    pub session_id: String,
    pub session_title: String,
    pub started_at: i64,
    pub channel: u8,
    pub speaker: String,
    pub t_start_ms: i64,
    pub t_end_ms: i64,
    pub text: String,
}

/// One page of matching lines, and how many match in all.
#[derive(Debug, Clone, Serialize)]
pub struct SegmentPage {
    pub matches: Vec<SegmentHit>,
    pub total: i64,
}

/// One retained ask. `session_id` is null for an ask made outside a session.
#[derive(Debug, Clone, Serialize)]
pub struct AskRow {
    pub id: i64,
    pub session_id: Option<String>,
    pub question: String,
    pub screen_context: Option<String>,
    pub answer: String,
    pub provider: String,
    pub created_at: i64,
}

/// One egress-ledger row as returned to clients.
#[derive(Debug, Clone, Serialize)]
pub struct EgressRow {
    pub id: i64,
    pub ts: i64,
    pub session_id: Option<String>,
    /// `"cloud"` (Auricle sent it off the machine), `"local"` (stayed
    /// on-device), or `"agent"` (a local MCP client read it; where that
    /// client sent it next is not knowable from here).
    pub destination: String,
    pub provider: String,
    pub host: Option<String>,
    /// What was sent: `"audio"`, `"prompt"`, `"summary"`, or one of the MCP
    /// read kinds (see `mcp.rs`).
    pub kind: String,
    /// Rough size in provider-natural units (chars for text; null for audio).
    pub items: Option<i64>,
    pub detail: Option<String>,
}

/// One `(destination, counterparty)` tally over the whole ledger.
#[derive(Debug, Clone, Serialize)]
pub struct EgressTotal {
    pub destination: String,
    /// The cloud host, or — for `local` and `agent` rows, which have no
    /// host — the provider or agent client name.
    pub who: String,
    pub count: i64,
}

/// Input to [`Store::record_egress`]. Borrowed to avoid allocations on the
/// hot path; the ledger records metadata only, never the payload.
pub struct EgressEntry<'a> {
    pub ts: i64,
    pub session_id: Option<&'a str>,
    pub destination: &'a str,
    pub provider: &'a str,
    pub host: Option<&'a str>,
    pub kind: &'a str,
    pub items: Option<i64>,
    pub detail: Option<&'a str>,
}

pub fn channel_code(channel: ChannelId) -> u8 {
    match channel {
        ChannelId::Mic => 0,
        ChannelId::Loopback => 1,
    }
}

pub struct Store {
    /// Read-only, so a search or transcript fetch neither waits for the
    /// writer nor holds up `insert_segment` during capture. WAL lets the two
    /// run at once; the shared mutex this replaced did not.
    ///
    /// Declared before `conn` because fields drop in order: the last
    /// connection to close checkpoints and removes the WAL, and a read-only
    /// one cannot.
    read: Mutex<Connection>,
    conn: Mutex<Connection>,
    path: PathBuf,
}

fn db_err(e: rusqlite::Error) -> Error {
    Error::Io(std::io::Error::other(format!("sqlite: {e}")))
}

fn open_reader(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(db_err)
}

/// Put the database in WAL mode, which persists, so only its first open has
/// any work to do. That work upgrades a read lock to exclusive, and SQLite
/// fails such an upgrade at once rather than through the busy timeout, so
/// two openers racing on a new database need this retry.
fn enable_wal(conn: &Connection) -> Result<()> {
    let give_up = Instant::now() + Duration::from_secs(5);
    loop {
        match conn.pragma_update(None, "journal_mode", "WAL") {
            Err(e)
                if e.sqlite_error_code() == Some(ErrorCode::DatabaseBusy)
                    && Instant::now() < give_up =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result.map_err(db_err),
        }
    }
}

/// Finish a privacy delete on disk. `secure_delete` only zeroes the text as
/// its pages reach the main file, and until then the WAL still holds copies,
/// so a daemon that runs for days would keep them for days.
///
/// Best effort, and deliberately impatient: this holds the write connection,
/// and waiting out a long reader such as a backup would stall capture for the
/// whole busy timeout. Ordinary reads finish well inside the short wait; after
/// a long one the text leaves at a later checkpoint, at the latest on close.
fn flush_wal(conn: &Connection) {
    let _ = conn.busy_timeout(Duration::from_millis(100));
    let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
    // rusqlite's default, which every other write relies on.
    let _ = conn.busy_timeout(Duration::from_secs(5));
}

/// The schema version `conn` is at. Refuses a database written by a newer
/// build: this build's queries cannot be trusted against a schema it has
/// never seen, and writing to it could damage what a downgrade left behind.
fn schema_version(conn: &Connection, latest: usize) -> Result<usize> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(db_err)?;
    if version > latest as i64 {
        return Err(Error::Io(std::io::Error::other(format!(
            "database schema v{version} is newer than this build supports (v{latest}); \
             update Auricle to open it"
        ))));
    }
    Ok(version as usize)
}

/// Bring the schema up to date, all or nothing: every pending migration and
/// its `user_version` stamp commit together, so a kill at any point leaves
/// the database at the version it started from.
fn migrate(conn: &mut Connection, migrations: &[&str]) -> Result<()> {
    // Up to date is the common case and must not need the write lock:
    // `auricle export` opens alongside a daemon that is busy recording.
    if schema_version(conn, migrations.len())? == migrations.len() {
        return Ok(());
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_err)?;
    // Read again now the lock is held: another opener may have got here first.
    let version = schema_version(&tx, migrations.len())?;
    for (i, sql) in migrations.iter().enumerate().skip(version) {
        tx.execute_batch(sql).map_err(db_err)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)
            .map_err(db_err)?;
    }
    tx.commit().map_err(db_err)
}

fn row_to_egress(r: &rusqlite::Row) -> rusqlite::Result<EgressRow> {
    Ok(EgressRow {
        id: r.get(0)?,
        ts: r.get(1)?,
        session_id: r.get(2)?,
        destination: r.get(3)?,
        provider: r.get(4)?,
        host: r.get(5)?,
        kind: r.get(6)?,
        items: r.get(7)?,
        detail: r.get(8)?,
    })
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
        let mut conn = Connection::open(path).map_err(db_err)?;
        enable_wal(&conn)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(db_err)?;
        // Delete is a privacy action: freed pages are zeroed rather than left
        // holding the text. Set before migrating, which deletes rows too.
        conn.pragma_update(None, "secure_delete", "ON")
            .map_err(db_err)?;
        migrate(&mut conn, MIGRATIONS)?;
        // Only now: a read-only connection cannot create the file itself.
        let read = open_reader(path)?;
        Ok(Store {
            read: Mutex::new(read),
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
        })
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

    /// Set one top-level field of a session's meta. One statement, so writers
    /// of different fields can't undo each other, as a read, change and write
    /// back would: the post-stop loss record and a rename both land here.
    pub fn set_session_meta(&self, id: &str, key: &str, value: &serde_json::Value) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET meta = json_set(meta, '$.' || ?2, json(?3)) WHERE id = ?1",
            rusqlite::params![id, key, value.to_string()],
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
    /// OR transcript text contains it (case-insensitive SQLite LIKE, which
    /// folds ASCII only).
    pub fn list_sessions(&self, query: Option<&str>) -> Result<Vec<SessionRow>> {
        let conn = self.read.lock().unwrap();
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
                    .query_map([like_pattern(q)], row_to_session)
                    .map_err(db_err)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(db_err)?;
                rows
            }
        };
        Ok(rows)
    }

    /// One page of sessions, newest first, with the total across all pages.
    pub fn list_sessions_page(&self, limit: i64, offset: i64) -> Result<SessionPage> {
        let mut conn = self.read.lock().unwrap();
        // One snapshot, so `total` describes the same sessions as the page.
        let tx = conn.transaction().map_err(db_err)?;
        let total = tx
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .map_err(db_err)?;
        // `id` breaks ties, or a row could repeat or vanish across pages.
        let mut stmt = tx
            .prepare(
                "SELECT id, title, started_at, ended_at, stt_provider, meta
                 FROM sessions ORDER BY started_at DESC, id DESC LIMIT ?1 OFFSET ?2",
            )
            .map_err(db_err)?;
        let sessions = stmt
            .query_map([limit, offset], row_to_session)
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        Ok(SessionPage { sessions, total })
    }

    /// One page of `list_sessions(query)`, with the total across all pages.
    /// Search results are filtered before they are paged, so `total` counts
    /// the matches rather than every session.
    pub fn find_sessions_page(
        &self,
        query: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<SessionPage> {
        if query.is_none_or(|q| q.trim().is_empty()) {
            return self.list_sessions_page(limit, offset);
        }
        let rows = self.list_sessions(query)?;
        let total = rows.len() as i64;
        let sessions = rows
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
        Ok(SessionPage { sessions, total })
    }

    /// Sessions that ended before `cutoff` (unix seconds), oldest first, as
    /// `(id, ended_at)`: the candidates for a retention sweep. A session with
    /// no `ended_at` is still recording, or awaits crash recovery, and is
    /// never one of them however old it is.
    ///
    /// A stamp is only as good as the clock that wrote it, but rowids go up in
    /// the order sessions were recorded whatever that clock said. So a session
    /// is taken to be no older than anything recorded before it, and one stamped
    /// long ago on a slow clock doesn't expire the day the clock is put right.
    /// Stamps later than `now` vouch for nothing: one from a fast clock would
    /// otherwise keep everything recorded after it until that date came round.
    pub fn sessions_older_than(&self, cutoff: i64, now: i64) -> Result<Vec<(String, i64)>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, ended_at FROM (
                    SELECT id, ended_at,
                           MAX(CASE WHEN ended_at <= ?2 THEN ended_at END)
                               OVER (ORDER BY rowid) AS newest_so_far
                    FROM sessions WHERE ended_at IS NOT NULL)
                 WHERE ended_at < ?1 AND newest_so_far < ?1
                 ORDER BY ended_at",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([cutoff, now], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        Ok(rows)
    }

    /// When the newest finished session ended, by its stamp.
    pub fn latest_session_end(&self) -> Result<Option<i64>> {
        let conn = self.read.lock().unwrap();
        conn.query_row("SELECT MAX(ended_at) FROM sessions", [], |r| r.get(0))
            .map_err(db_err)
    }

    /// Transcript lines matching `query`, newest meeting first.
    ///
    /// The line-level counterpart to `list_sessions(Some(q))`, which answers
    /// "which meetings mention this" and leaves the caller to pull whole
    /// transcripts to find out where. An MCP client has a bounded context
    /// window, so it needs the matching lines themselves.
    pub fn search_segments(&self, query: &str, limit: i64) -> Result<Vec<SegmentHit>> {
        let Some(q) = Some(query.trim()).filter(|q| !q.is_empty()) else {
            return Ok(Vec::new());
        };
        segment_hits(&self.read.lock().unwrap(), q, limit, 0)
    }

    /// One page of [`search_segments`](Self::search_segments), for a person
    /// paging through results and told how many there are.
    pub fn search_segments_page(
        &self,
        query: &str,
        limit: i64,
        offset: i64,
    ) -> Result<SegmentPage> {
        let Some(q) = Some(query.trim()).filter(|q| !q.is_empty()) else {
            return Ok(SegmentPage {
                matches: Vec::new(),
                total: 0,
            });
        };
        let conn = self.read.lock().unwrap();
        let total = conn
            .query_row(
                "SELECT COUNT(*) FROM segments WHERE text LIKE ?1 ESCAPE '\\'",
                [like_pattern(q)],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        Ok(SegmentPage {
            matches: segment_hits(&conn, q, limit, offset)?,
            total,
        })
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

    /// Apply a generated title only while the session is still auto-titled.
    /// One statement, so no user rename can land between a check and the
    /// write. Returns false when the session is gone or the user named it.
    pub fn rename_auto_titled(&self, id: &str, title: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute(
                "UPDATE sessions SET title = ?2
                  WHERE id = ?1 AND json_extract(meta, '$.auto_title') = 1",
                rusqlite::params![id, title],
            )
            .map_err(db_err)?;
        Ok(n > 0)
    }

    /// Delete a session and its segments, summaries and asks. Returns false
    /// when the id does not exist. (Retained audio files are the caller's
    /// job — the store only owns database rows.)
    ///
    /// Waits on the write connection and flushes the WAL, so call it off the
    /// async runtime.
    pub fn delete_session(&self, id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute("DELETE FROM segments WHERE session_id = ?1", [id])
            .map_err(db_err)?;
        tx.execute("DELETE FROM summaries WHERE session_id = ?1", [id])
            .map_err(db_err)?;
        tx.execute("DELETE FROM asks WHERE session_id = ?1", [id])
            .map_err(db_err)?;
        let n = tx
            .execute("DELETE FROM sessions WHERE id = ?1", [id])
            .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        if n > 0 {
            flush_wal(&conn);
        }
        Ok(n > 0)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRow>> {
        let conn = self.read.lock().unwrap();
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
        let conn = self.read.lock().unwrap();
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

    /// Persist a summary and return its id. Summarizing is a long LLM call
    /// and the session can be deleted during it: then this writes nothing
    /// and returns 0, which is never a real id.
    pub fn insert_summary(
        &self,
        session_id: &str,
        template: &str,
        model: &str,
        content: &str,
        created_at: i64,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute(
                "INSERT INTO summaries(session_id, template, model, content, created_at)
                 SELECT ?1, ?2, ?3, ?4, ?5
                 WHERE EXISTS (SELECT 1 FROM sessions WHERE id = ?1)",
                rusqlite::params![session_id, template, model, content, created_at],
            )
            .map_err(db_err)?;
        Ok(if n > 0 { conn.last_insert_rowid() } else { 0 })
    }

    pub fn get_summaries(&self, session_id: &str) -> Result<Vec<SummaryRow>> {
        let conn = self.read.lock().unwrap();
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
    /// The caller may call this only when `copilot.retain_context = true`.
    /// With retention off (the default) nothing question- or screen-derived
    /// ever reaches this database.
    ///
    /// Returns the new id, or 0 having written nothing when the session was
    /// deleted while the answer streamed: screen text must not outlive it.
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
        let n = conn
            .execute(
                "INSERT INTO asks(session_id, question, screen_context, answer, provider, created_at)
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6
                 WHERE ?1 IS NULL OR EXISTS (SELECT 1 FROM sessions WHERE id = ?1)",
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
        Ok(if n > 0 { conn.last_insert_rowid() } else { 0 })
    }

    /// Retained asks, newest first, optionally scoped to one session.
    pub fn list_asks(
        &self,
        session_id: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AskRow>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, question, screen_context, answer, provider, created_at
                 FROM asks WHERE ?1 IS NULL OR session_id = ?1
                 ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(rusqlite::params![session_id, limit, offset], |r| {
                Ok(AskRow {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    question: r.get(2)?,
                    screen_context: r.get(3)?,
                    answer: r.get(4)?,
                    provider: r.get(5)?,
                    created_at: r.get(6)?,
                })
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        Ok(rows)
    }

    /// Delete one retained ask. Returns false when the id does not exist.
    pub fn delete_ask(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .execute("DELETE FROM asks WHERE id = ?1", [id])
            .map_err(db_err)?;
        flush_wal(&conn);
        Ok(n > 0)
    }

    /// Write a consistent copy of the database to `path`, which must not
    /// exist. On a connection of its own, closed afterwards: this reads the
    /// whole database, and neither recording nor a read should wait for that.
    pub fn backup_to(&self, path: &Path) -> Result<()> {
        let target = path
            .to_str()
            .ok_or_else(|| Error::Io(std::io::Error::other("backup path is not valid UTF-8")))?;
        // SQLite would write into an existing empty file. `create_new` refuses
        // any existing file, and makes the cleanup below safe: what it removes
        // is only ever the file made here.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let copied = open_reader(&self.path)
            .and_then(|conn| conn.execute("VACUUM INTO ?1", [target]).map_err(db_err));
        if let Err(e) = copied {
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
        Ok(())
    }

    /// Test/verification surface: how many asks are persisted. Zero is the
    /// state `retain_context = false` must leave things in.
    pub fn asks_count(&self) -> Result<i64> {
        let conn = self.read.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM asks", [], |r| r.get(0))
            .map_err(db_err)
    }

    /// Append one egress-ledger row. Metadata only — callers never pass the
    /// audio, prompt, or answer, just where it went and roughly how much.
    /// Append one ledger row and bump its running total, atomically — a row
    /// the headline does not count would be worse than no row at all.
    pub fn record_egress(&self, e: &EgressEntry) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute(
            "INSERT INTO egress(ts, session_id, destination, provider, host, kind, items, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                e.ts,
                e.session_id,
                e.destination,
                e.provider,
                e.host,
                e.kind,
                e.items,
                e.detail,
            ],
        )
        .map_err(db_err)?;
        // `who` mirrors the v3 backfill's COALESCE(host, provider): the cloud
        // host where there is one, else the provider or agent client name.
        tx.execute(
            "INSERT INTO egress_totals(destination, who, count) VALUES (?1, ?2, 1)
             ON CONFLICT(destination, who) DO UPDATE SET count = count + 1",
            rusqlite::params![e.destination, e.host.unwrap_or(e.provider)],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok(())
    }

    /// Most-recent egress rows, optionally scoped to one session.
    pub fn list_egress(
        &self,
        session_id: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<EgressRow>> {
        let conn = self.read.lock().unwrap();
        let cols = "id, ts, session_id, destination, provider, host, kind, items, detail";
        let rows = match session_id {
            Some(id) => {
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT {cols} FROM egress WHERE session_id = ?1
                         ORDER BY ts DESC, id DESC LIMIT ?2 OFFSET ?3"
                    ))
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map(rusqlite::params![id, limit, offset], row_to_egress)
                    .map_err(db_err)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(db_err)?;
                rows
            }
            None => {
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT {cols} FROM egress ORDER BY ts DESC, id DESC LIMIT ?1 OFFSET ?2"
                    ))
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map([limit, offset], row_to_egress)
                    .map_err(db_err)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(db_err)?;
                rows
            }
        };
        Ok(rows)
    }

    /// Whole-ledger counts, grouped by destination and by who was on the
    /// other end — the cloud host, or the agent's client name.
    ///
    /// The headline ("nothing has left this machine") must be a fact about
    /// the ledger, not about whichever page of it the UI happened to fetch —
    /// agent reads arrive fast enough to push every cloud row off a page.
    ///
    /// Reads the running totals maintained by [`Self::record_egress`], so the
    /// cost is one row per distinct counterparty rather than one per ledger
    /// entry. Deriving this with a GROUP BY was measured at 1.3 s over a
    /// million rows, all of it holding the store mutex.
    pub fn egress_totals(&self) -> Result<Vec<EgressTotal>> {
        let conn = self.read.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT destination, who, count FROM egress_totals")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(EgressTotal {
                    destination: r.get(0)?,
                    who: r.get(1)?,
                    count: r.get(2)?,
                })
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        Ok(rows)
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
        let conn = self.read.lock().unwrap();
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

fn segment_hits(
    conn: &Connection,
    query: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<SegmentHit>> {
    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.title, s.started_at, g.channel, g.speaker,
                    g.t_start_ms, g.t_end_ms, g.text
             FROM segments g JOIN sessions s ON s.id = g.session_id
             WHERE g.text LIKE ?1 ESCAPE '\\'
             ORDER BY s.started_at DESC, g.t_start_ms, g.id
             LIMIT ?2 OFFSET ?3",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map(rusqlite::params![like_pattern(query), limit, offset], |r| {
            Ok(SegmentHit {
                session_id: r.get(0)?,
                session_title: r.get(1)?,
                started_at: r.get(2)?,
                channel: r.get(3)?,
                speaker: r.get(4)?,
                t_start_ms: r.get(5)?,
                t_end_ms: r.get(6)?,
                text: r.get(7)?,
            })
        })
        .map_err(db_err)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(db_err)?;
    Ok(rows)
}

/// Wrap a user string as a LIKE contains-pattern, escaping the wildcards so
/// a literal `%` or `_` in the query matches itself. Pairs with
/// `ESCAPE '\'` in the statement.
fn like_pattern(q: &str) -> String {
    format!(
        "%{}%",
        q.replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    )
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
    fn segment_search_returns_the_matching_lines() {
        let path = temp_db("search-segments");
        let store = Store::open(&path).unwrap();
        store
            .create_session("s1", "Weekly sync", 100, "deepgram", &serde_json::json!({}))
            .unwrap();
        store
            .insert_segment("s1", ChannelId::Mic, "You", 0, 900, "budget approved", "d")
            .unwrap();
        store
            .insert_segment("s1", ChannelId::Mic, "You", 5_000, 6_000, "unrelated", "d")
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
                1_500,
                2_000,
                "the budget again",
                "d",
            )
            .unwrap();

        // Newest session first, and the line carries its own context.
        let hits = store.search_segments("budget", 20).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].session_id, "s2");
        assert_eq!(hits[0].session_title, "1:1 with Sam");
        assert_eq!(hits[0].speaker, "Them");
        assert_eq!(hits[0].t_start_ms, 1_500);
        assert_eq!(hits[1].session_id, "s1");
        assert_eq!(hits[1].text, "budget approved");

        // Case-insensitive, and the limit is honoured.
        assert_eq!(store.search_segments("BUDGET", 1).unwrap().len(), 1);
        // Blank query is not "match everything".
        assert!(store.search_segments("   ", 20).unwrap().is_empty());
        // LIKE wildcards in user input stay literal.
        assert!(store.search_segments("%", 20).unwrap().is_empty());
        assert!(store.search_segments("_", 20).unwrap().is_empty());
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
    fn asks_persist_only_when_inserted_and_survive_reopen() {
        let path = temp_db("asks");
        let store = Store::open(&path).unwrap();
        // With retain_context off, insert_ask is never called, and opening
        // (and migrating) the store must not put anything there either.
        assert_eq!(store.asks_count().unwrap(), 0);
        store
            .create_session("s1", "t", 100, "deepgram", &serde_json::json!({}))
            .unwrap();

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
        assert_eq!(store.asks_count().unwrap(), 2);

        // Reopen: still there, migrations still clean.
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.asks_count().unwrap(), 2);
    }

    #[test]
    fn egress_totals_track_every_row_and_survive_reopen() {
        let path = temp_db("egress-totals");
        let store = Store::open(&path).unwrap();
        let entry = |destination, provider, host| EgressEntry {
            ts: 1_700_000_000,
            session_id: None,
            destination,
            provider,
            host,
            kind: "prompt",
            items: Some(10),
            detail: None,
        };
        store
            .record_egress(&entry("cloud", "groq", Some("api.groq.com")))
            .unwrap();
        store
            .record_egress(&entry("cloud", "groq", Some("api.groq.com")))
            .unwrap();
        store
            .record_egress(&entry("local", "whisper-local", None))
            .unwrap();
        // No host: the counterparty is the provider (an agent's client name).
        store
            .record_egress(&entry("agent", "claude-code", None))
            .unwrap();

        let totals = |s: &Store| {
            let mut t = s.egress_totals().unwrap();
            t.sort_by(|a, b| (&a.destination, &a.who).cmp(&(&b.destination, &b.who)));
            t.into_iter()
                .map(|t| (t.destination, t.who, t.count))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            totals(&store),
            vec![
                ("agent".into(), "claude-code".into(), 1),
                ("cloud".into(), "api.groq.com".into(), 2),
                ("local".into(), "whisper-local".into(), 1),
            ]
        );
        // The totals agree with the rows themselves, which is the property
        // that makes the headline trustworthy.
        assert_eq!(store.list_egress(None, 100, 0).unwrap().len(), 4);

        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(totals(&store).len(), 3);
    }

    #[test]
    fn egress_totals_are_backfilled_for_a_ledger_that_predates_them() {
        let path = temp_db("egress-backfill");
        // Recreate a v2 database: the ledger exists, the totals table does not.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute_batch(MIGRATIONS[1]).unwrap();
            conn.pragma_update(None, "user_version", 2i64).unwrap();
            conn.execute_batch(
                "INSERT INTO egress(ts, destination, provider, host, kind)
                 VALUES (1, 'cloud', 'groq', 'api.groq.com', 'prompt'),
                        (2, 'cloud', 'groq', 'api.groq.com', 'summary'),
                        (3, 'local', 'whisper-local', NULL, 'audio');",
            )
            .unwrap();
        }
        // Opening applies v3, which must seed itself from that history rather
        // than starting from zero — an upgrade must not erase the record.
        let store = Store::open(&path).unwrap();
        let mut totals = store.egress_totals().unwrap();
        totals.sort_by(|a, b| a.who.cmp(&b.who));
        assert_eq!(totals.len(), 2);
        assert_eq!(
            (totals[0].who.as_str(), totals[0].count),
            ("api.groq.com", 2)
        );
        assert_eq!(
            (totals[1].who.as_str(), totals[1].count),
            ("whisper-local", 1)
        );
    }

    fn user_version(path: &Path) -> i64 {
        rusqlite::Connection::open(path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    /// A database as the previous release left it: schema v3, no `asks` table.
    fn v3_db(path: &Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        for sql in &MIGRATIONS[..3] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 3i64).unwrap();
        conn
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store
            .conn
            .lock()
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    }

    fn on_disk(path: &Path, needle: &[u8]) -> bool {
        ["", "-wal"].iter().any(|suffix| {
            let mut file = path.as_os_str().to_owned();
            file.push(suffix);
            std::fs::read(file).is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
        })
    }

    #[test]
    fn a_failed_migration_rolls_back_and_the_next_open_recovers() {
        let path = temp_db("migrate-failed");
        {
            let mut conn = rusqlite::Connection::open(&path).unwrap();
            // v1 lands, then the next one dies after its first statement: the
            // half-applied shape a kill mid-migration would leave behind.
            let interrupted = [MIGRATIONS[0], "CREATE TABLE egress(id INT); NOT SQL;"];
            assert!(migrate(&mut conn, &interrupted).is_err());
            let objects: i64 = conn
                .query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))
                .unwrap();
            assert_eq!(objects, 0, "nothing from the failed run may survive");
        }
        assert_eq!(user_version(&path), 0);

        // Before the fix this open died on "table already exists".
        let store = Store::open(&path).unwrap();
        assert!(store.list_sessions(None).unwrap().is_empty());
        drop(store);
        assert_eq!(user_version(&path), MIGRATIONS.len() as i64);
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused() {
        let path = temp_db("downgrade");
        drop(Store::open(&path).unwrap());
        let newer = MIGRATIONS.len() as i64 + 1;
        rusqlite::Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", newer)
            .unwrap();

        let err = Store::open(&path)
            .err()
            .expect("a newer schema must refuse");
        assert!(err.to_string().contains("newer"), "unclear error: {err}");
        assert_eq!(user_version(&path), newer, "must be left untouched");
    }

    #[test]
    fn concurrent_openers_of_a_fresh_database_all_succeed() {
        let path = temp_db("migrate-race");
        let openers: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || Store::open(&path).map(drop))
            })
            .collect();
        for opener in openers {
            opener.join().unwrap().unwrap();
        }
        assert_eq!(user_version(&path), MIGRATIONS.len() as i64);
    }

    #[test]
    fn opening_a_current_database_needs_no_write_lock() {
        // `auricle export` opens the database while a daemon is writing to it.
        let path = temp_db("open-under-write");
        drop(Store::open(&path).unwrap());
        let daemon = rusqlite::Connection::open(&path).unwrap();
        daemon.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        Store::open(&path).unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "open waited on the write lock"
        );
        daemon.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn delete_session_takes_its_asks_with_it() {
        let path = temp_db("delete-asks");
        let store = Store::open(&path).unwrap();
        for id in ["gone", "kept"] {
            store
                .create_session(id, "t", 100, "deepgram", &serde_json::json!({}))
                .unwrap();
        }
        store
            .insert_ask(Some("gone"), "q", Some("ocr"), "a", "ollama", 1)
            .unwrap();
        store
            .insert_ask(Some("kept"), "q", None, "a", "ollama", 2)
            .unwrap();
        store.insert_ask(None, "q", None, "a", "ollama", 3).unwrap();

        assert!(store.delete_session("gone").unwrap());
        let gone = "SELECT COUNT(*) FROM asks WHERE session_id = 'gone'";
        assert_eq!(count(&store, gone), 0, "screen text outlived its session");
        assert_eq!(store.asks_count().unwrap(), 2);
    }

    #[test]
    fn upgrading_keeps_asks_retained_under_the_lazy_table() {
        let path = temp_db("asks-upgrade");
        {
            let conn = v3_db(&path);
            conn.execute_batch(
                "CREATE TABLE asks(
                    id INTEGER PRIMARY KEY,
                    session_id TEXT,
                    question TEXT NOT NULL,
                    screen_context TEXT,
                    answer TEXT NOT NULL,
                    provider TEXT NOT NULL,
                    created_at INT NOT NULL
                 );
                 INSERT INTO sessions(id, title, started_at, stt_provider)
                    VALUES ('s1', 't', 1, 'd');
                 INSERT INTO asks(session_id, question, answer, provider, created_at)
                    VALUES ('s1', 'q', 'a', 'p', 1);",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.asks_count().unwrap(), 1);
    }

    #[test]
    fn upgrading_sweeps_up_what_earlier_deletes_left_behind() {
        let path = temp_db("asks-sweep");
        {
            let conn = v3_db(&path);
            // No sessions at all: the case where `NOT IN` would also take the
            // session-less ask, which was never tied to a deleted meeting.
            conn.execute_batch(
                "CREATE TABLE asks(
                    id INTEGER PRIMARY KEY,
                    session_id TEXT,
                    question TEXT NOT NULL,
                    screen_context TEXT,
                    answer TEXT NOT NULL,
                    provider TEXT NOT NULL,
                    created_at INT NOT NULL
                 );
                 INSERT INTO asks(session_id, question, answer, provider, created_at)
                    VALUES ('deleted', 'q', 'a', 'p', 1), (NULL, 'kept', 'a', 'p', 2);
                 INSERT INTO summaries(session_id, template, model, content, created_at)
                    VALUES ('deleted', 't', 'm', 'c', 1);",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.asks_count().unwrap(), 1);
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM asks WHERE question = 'kept'"),
            1
        );
        assert_eq!(count(&store, "SELECT COUNT(*) FROM summaries"), 0);
    }

    #[test]
    fn deleted_text_is_gone_from_the_files_on_disk() {
        let path = temp_db("secure-delete");
        let store = Store::open(&path).unwrap();
        store
            .create_session("s1", "t", 100, "deepgram", &serde_json::json!({}))
            .unwrap();
        store
            .insert_segment("s1", ChannelId::Mic, "You", 0, 1, "line sentinel", "d")
            .unwrap();
        store
            .insert_summary("s1", "minutes", "m", "summary sentinel", 1)
            .unwrap();
        store
            .insert_ask(
                Some("s1"),
                "question sentinel",
                Some("screen sentinel"),
                "answer sentinel",
                "ollama",
                1,
            )
            .unwrap();
        // Land it in the main file, where freed pages would otherwise keep it.
        store
            .conn
            .lock()
            .unwrap()
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();

        assert!(on_disk(&path, b"sentinel"), "precondition: text on disk");

        // Read while the store is open: a daemon runs for days between closes.
        store.delete_session("s1").unwrap();
        assert!(
            !on_disk(&path, b"sentinel"),
            "deleted text left in the file"
        );
    }

    #[test]
    fn a_delete_during_a_long_read_does_not_stall_the_writer() {
        let path = temp_db("delete-under-read");
        let store = Store::open(&path).unwrap();
        store
            .create_session("s1", "t", 100, "deepgram", &serde_json::json!({}))
            .unwrap();

        // A backup looks like this: a read that stays open for minutes.
        let long_read = rusqlite::Connection::open(&path).unwrap();
        long_read
            .execute_batch("BEGIN; SELECT COUNT(*) FROM sessions;")
            .unwrap();
        let started = std::time::Instant::now();
        assert!(store.delete_session("s1").unwrap());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "capture would have waited {:?} on this delete",
            started.elapsed()
        );

        // The flush's short patience must not stick to the writer, or writes
        // would start failing under ordinary contention.
        let blocker = rusqlite::Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(500));
            blocker.execute_batch("ROLLBACK").unwrap();
        });
        store
            .create_session("s2", "t", 100, "deepgram", &serde_json::json!({}))
            .unwrap();
        release.join().unwrap();
    }

    #[test]
    fn a_summary_finishing_after_a_delete_writes_nothing() {
        let path = temp_db("late-summary");
        let store = Store::open(&path).unwrap();
        store
            .create_session("s1", "t", 100, "deepgram", &serde_json::json!({}))
            .unwrap();
        assert!(store.insert_summary("s1", "m", "m", "c", 1).unwrap() > 0);
        store.delete_session("s1").unwrap();

        assert_eq!(store.insert_summary("s1", "m", "m", "late", 2).unwrap(), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM summaries"), 0);
    }

    #[test]
    fn an_ask_finishing_after_a_delete_writes_nothing() {
        let path = temp_db("late-ask");
        let store = Store::open(&path).unwrap();
        let late = store
            .insert_ask(Some("deleted"), "q", Some("ocr"), "a", "ollama", 1)
            .unwrap();
        assert_eq!(late, 0);
        assert_eq!(store.asks_count().unwrap(), 0);
        // A session-less ask has nothing to have been deleted.
        assert!(store.insert_ask(None, "q", None, "a", "ollama", 2).unwrap() > 0);
    }

    #[test]
    fn transcript_search_is_a_literal_case_insensitive_substring_match() {
        let path = temp_db("search-literal");
        let store = Store::open(&path).unwrap();
        let lines = [
            "budget approved for Q3",
            "the BUDGET again",
            "she said \"ship it\" twice",
            "100% done_now",
            "path C:\\temp\\new",
            "AND OR NOT NEAR(a b)",
            "foo*bar ^start col:value",
            "(paren) {brace}",
            "it's a 'quote'; -- drop",
            "Müller spoke",
            "a_b a%b",
        ];
        for (i, line) in lines.iter().enumerate() {
            let id = format!("s{i}");
            store
                .create_session(&id, "~", i as i64, "d", &serde_json::json!({}))
                .unwrap();
            store
                .insert_segment(&id, ChannelId::Mic, "You", 0, 1, line, "d")
                .unwrap();
        }
        let queries = [
            // Whole word, inside a word, across a word boundary, mixed case.
            "budget",
            "udge",
            "get app",
            "BuDgEt",
            "Müller",
            "ü",
            "ll",
            // Punctuation and operator words are plain text.
            "\"",
            "\"\"",
            "\"\"\"",
            "\"ship it\"",
            "ship\" it",
            "AND",
            "OR",
            "NOT",
            "NEAR(a",
            "NEAR(a b)",
            "foo*",
            "^start",
            "col:value",
            "(paren)",
            "{brace}",
            "*",
            "^",
            ":",
            "(",
            ")",
            "{",
            "}",
            "-",
            "+",
            "--",
            "'quote'",
            "; --",
            // LIKE wildcards likewise.
            "%",
            "_",
            "%%%",
            "___",
            "100%",
            "e_n",
            "done_now",
            "a%b",
            "a_b",
            "C:\\temp",
            "\\",
            "\\\\\\",
            "xyzzy",
        ];
        for q in queries {
            let needle = q.to_lowercase();
            let mut expected: Vec<String> = lines
                .iter()
                .filter(|l| l.to_lowercase().contains(&needle))
                .map(|l| l.to_string())
                .collect();
            expected.sort();
            let mut found: Vec<String> = store
                .search_segments(q, 100)
                .unwrap_or_else(|e| panic!("query {q:?}: {e}"))
                .into_iter()
                .map(|hit| hit.text)
                .collect();
            found.sort();
            assert_eq!(found, expected, "search_segments for {q:?}");
            assert_eq!(
                store.list_sessions(Some(q)).unwrap().len(),
                expected.len(),
                "list_sessions for {q:?}"
            );
        }
    }

    #[test]
    fn hostile_queries_never_reach_the_caller_as_errors() {
        let path = temp_db("search-hostile");
        let store = Store::open(&path).unwrap();
        store
            .create_session("s1", "t", 100, "deepgram", &serde_json::json!({}))
            .unwrap();
        store
            .insert_segment("s1", ChannelId::Mic, "You", 0, 1, "budget", "d")
            .unwrap();
        let long = "budget ".repeat(5_000);
        for q in ["bud\0get", "\0\0\0", "\"\0\"", long.as_str()] {
            store.search_segments(q, 10).unwrap();
            store.list_sessions(Some(q)).unwrap();
        }
    }

    #[test]
    fn reads_proceed_while_the_write_connection_is_held() {
        let path = temp_db("read-while-write");
        let store = std::sync::Arc::new(Store::open(&path).unwrap());
        store
            .create_session("s1", "standup", 100, "deepgram", &serde_json::json!({}))
            .unwrap();
        store
            .insert_segment("s1", ChannelId::Mic, "You", 0, 1, "budget approved", "d")
            .unwrap();

        // The state a slow write leaves things in: mutex held, transaction open.
        let writer = store.conn.lock().unwrap();
        writer
            .execute_batch(
                "BEGIN IMMEDIATE;
                 INSERT INTO sessions(id, title, started_at, stt_provider)
                    VALUES ('uncommitted', 't', 200, 'd');",
            )
            .unwrap();

        let (done, finished) = std::sync::mpsc::channel();
        let reader = std::sync::Arc::clone(&store);
        std::thread::spawn(move || {
            let reads = (
                reader.list_sessions(None).unwrap().len(),
                reader.list_sessions(Some("budget")).unwrap().len(),
                reader.search_segments("budget", 10).unwrap().len(),
                reader.search_segments("bu", 10).unwrap().len(),
                reader.get_session("s1").unwrap().is_some(),
                reader.get_segments("s1").unwrap().len(),
                reader.list_sessions_page(10, 0).unwrap().total,
            );
            done.send(reads).unwrap();
        });
        let reads = finished
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a read blocked behind the write connection");
        // Sees the last commit, and nothing of the transaction still open.
        assert_eq!(reads, (1, 1, 1, 1, true, 1, 1));

        writer.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn writes_proceed_while_the_read_connection_is_held() {
        let path = temp_db("write-while-read");
        let store = std::sync::Arc::new(Store::open(&path).unwrap());
        let held = store.read.lock().unwrap();

        let (done, finished) = std::sync::mpsc::channel();
        let writer = std::sync::Arc::clone(&store);
        std::thread::spawn(move || {
            let written = writer.create_session("s1", "t", 1, "d", &serde_json::json!({}));
            done.send(written.is_ok()).unwrap();
        });
        let written = finished
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a write blocked behind the read connection");
        assert!(written);
        drop(held);
    }

    #[test]
    fn the_read_connection_cannot_write() {
        let path = temp_db("read-only");
        let store = Store::open(&path).unwrap();
        let read = store.read.lock().unwrap();
        assert!(read
            .execute("DELETE FROM settings", [])
            .is_err_and(|e| e.to_string().contains("readonly")));
    }

    #[test]
    fn closing_the_store_checkpoints_and_removes_the_wal() {
        // Only the last connection to close can do this, and a read-only one
        // cannot, so the writer has to be the one that closes last.
        let path = temp_db("wal-close");
        let store = Store::open(&path).unwrap();
        store
            .create_session("s1", "t", 100, "deepgram", &serde_json::json!({}))
            .unwrap();
        // A connection holds no lock until it has read something.
        assert_eq!(store.list_sessions(None).unwrap().len(), 1);
        drop(store);
        let mut wal = path.clone().into_os_string();
        wal.push("-wal");
        assert!(!std::path::Path::new(&wal).exists(), "WAL left behind");
    }

    #[test]
    fn list_asks_is_newest_first_paged_and_scoped_to_a_session() {
        let path = temp_db("list-asks");
        let store = Store::open(&path).unwrap();
        for id in ["s1", "s2"] {
            store
                .create_session(id, "t", 100, "deepgram", &serde_json::json!({}))
                .unwrap();
        }
        store
            .insert_ask(Some("s1"), "first", Some("ocr"), "a1", "ollama", 10)
            .unwrap();
        store
            .insert_ask(Some("s1"), "second", None, "a2", "groq", 20)
            .unwrap();
        store
            .insert_ask(Some("s2"), "other", None, "a3", "groq", 30)
            .unwrap();
        store
            .insert_ask(None, "session-less", None, "a4", "groq", 40)
            .unwrap();

        let questions =
            |rows: Vec<AskRow>| rows.into_iter().map(|r| r.question).collect::<Vec<_>>();
        assert_eq!(
            questions(store.list_asks(None, 10, 0).unwrap()),
            ["session-less", "other", "second", "first"]
        );
        assert_eq!(
            questions(store.list_asks(Some("s1"), 10, 0).unwrap()),
            ["second", "first"]
        );
        assert_eq!(
            questions(store.list_asks(None, 2, 1).unwrap()),
            ["other", "second"]
        );
        assert!(store.list_asks(Some("nope"), 10, 0).unwrap().is_empty());

        let row = store.list_asks(Some("s1"), 1, 1).unwrap().remove(0);
        assert!(row.id > 0);
        assert_eq!(row.session_id.as_deref(), Some("s1"));
        assert_eq!(row.screen_context.as_deref(), Some("ocr"));
        assert_eq!(row.answer, "a1");
        assert_eq!(row.provider, "ollama");
        assert_eq!(row.created_at, 10);
    }

    #[test]
    fn delete_ask_removes_exactly_one_row() {
        let path = temp_db("delete-ask");
        let store = Store::open(&path).unwrap();
        let gone = store
            .insert_ask(None, "gone", None, "a", "groq", 1)
            .unwrap();
        let kept = store
            .insert_ask(None, "kept", None, "a", "groq", 2)
            .unwrap();

        assert!(store.delete_ask(gone).unwrap());
        assert!(!store.delete_ask(gone).unwrap(), "already gone");
        let rows = store.list_asks(None, 10, 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, kept);
    }

    #[test]
    fn list_sessions_page_reports_the_total_beyond_the_page() {
        let path = temp_db("sessions-page");
        let store = Store::open(&path).unwrap();
        // Tied start times: a page boundary must still be deterministic.
        for id in ["a", "b", "c", "d", "e"] {
            store
                .create_session(id, "t", 100, "deepgram", &serde_json::json!({}))
                .unwrap();
        }
        let ids = |offset| {
            let page = store.list_sessions_page(2, offset).unwrap();
            assert_eq!(page.total, 5);
            page.sessions.into_iter().map(|s| s.id).collect::<Vec<_>>()
        };
        assert_eq!(ids(0), ["e", "d"]);
        assert_eq!(ids(2), ["c", "b"]);
        assert_eq!(ids(4), ["a"]);
        assert!(ids(6).is_empty(), "past the end still knows the total");
        // The unpaged list is unchanged.
        assert_eq!(store.list_sessions(None).unwrap().len(), 5);
    }

    #[test]
    fn a_search_pages_and_counts_only_what_it_matched() {
        let path = temp_db("find-page");
        let store = Store::open(&path).unwrap();
        for (id, title, started) in [("a", "Sync", 1), ("b", "Sync", 2), ("c", "Retro", 3)] {
            store
                .create_session(id, title, started, "deepgram", &serde_json::json!({}))
                .unwrap();
        }
        let ids = |query, limit, offset| {
            let page = store.find_sessions_page(query, limit, offset).unwrap();
            let ids: Vec<_> = page.sessions.into_iter().map(|s| s.id).collect();
            (ids, page.total)
        };
        assert_eq!(ids(Some("sync"), 1, 0), (vec!["b".to_string()], 2));
        assert_eq!(ids(Some("sync"), 1, 1), (vec!["a".to_string()], 2));
        assert_eq!(ids(Some("sync"), 5, 5), (Vec::new(), 2));
        // Blank is no search at all.
        assert_eq!(ids(Some("  "), 1, 0), (vec!["c".to_string()], 3));
        assert_eq!(ids(None, 1, 2), (vec!["a".to_string()], 3));
    }

    #[test]
    fn sessions_older_than_skips_recent_and_in_progress_sessions() {
        let path = temp_db("older-than");
        let store = Store::open(&path).unwrap();
        for (id, started, ended) in [
            ("recording", 10, None),
            ("older", 50, Some(60)),
            ("old", 100, Some(200)),
            ("at-cutoff", 800, Some(1_000)),
            ("newer", 900, Some(1_500)),
        ] {
            store
                .create_session(id, "t", started, "deepgram", &serde_json::json!({}))
                .unwrap();
            if let Some(ended) = ended {
                store.end_session(id, ended).unwrap();
            }
        }
        assert_eq!(
            store.sessions_older_than(1_000, 2_000).unwrap(),
            [("older".to_string(), 60), ("old".to_string(), 200)]
        );
    }

    /// A store holding `(id, ended_at)` sessions, recorded in that order.
    fn recorded(name: &str, sessions: &[(&str, i64)]) -> Store {
        let store = Store::open(&temp_db(name)).unwrap();
        for (id, ended) in sessions {
            store
                .create_session(id, "t", ended - 10, "deepgram", &serde_json::json!({}))
                .unwrap();
            store.end_session(id, *ended).unwrap();
        }
        store
    }

    fn older_ids(store: &Store, cutoff: i64, now: i64) -> Vec<String> {
        let rows = store.sessions_older_than(cutoff, now).unwrap();
        rows.into_iter().map(|(id, _)| id).collect()
    }

    #[test]
    fn a_session_cannot_be_older_than_one_recorded_before_it() {
        let store = recorded(
            "older-than-order",
            &[("first", 100), ("kept", 1_500), ("slow-clock", 200)],
        );
        // "slow-clock" went in after a session that ended at 1,500, whatever
        // its own stamp says, so it is no older than that one.
        assert_eq!(older_ids(&store, 1_000, 2_000), ["first"]);
        // Once that session has expired, so has everything recorded before it
        // came along to vouch for it. Oldest stamp first, as ever.
        assert_eq!(
            older_ids(&store, 1_600, 2_000),
            ["first", "slow-clock", "kept"]
        );
    }

    #[test]
    fn a_stamp_from_the_future_vouches_for_nothing() {
        // Recorded on a fast clock. Taken at its word, it would keep everything
        // recorded after it until long after the user asked for it to go.
        let store = recorded(
            "older-than-future",
            &[("fast-clock", 9_999_999), ("old", 200)],
        );
        assert_eq!(older_ids(&store, 1_000, 2_000), ["old"]);
    }

    #[test]
    fn backup_is_a_working_copy_and_never_overwrites() {
        let path = temp_db("backup-source");
        let target = temp_db("backup-target");
        let store = Store::open(&path).unwrap();
        store
            .create_session("s1", "t", 100, "deepgram", &serde_json::json!({}))
            .unwrap();
        store
            .insert_segment("s1", ChannelId::Mic, "You", 0, 1, "budget approved", "d")
            .unwrap();

        store.backup_to(&target).unwrap();

        // A second backup, and an existing empty file SQLite itself would
        // accept, are both refused, with the existing file left as it was.
        let before = std::fs::read(&target).unwrap();
        let err = store.backup_to(&target).unwrap_err();
        assert!(err.to_string().contains("exist"), "unclear error: {err}");
        assert_eq!(std::fs::read(&target).unwrap(), before);
        let empty = temp_db("backup-empty");
        std::fs::write(&empty, b"").unwrap();
        assert!(store.backup_to(&empty).is_err());

        // The copy opens as a current database.
        let copy = Store::open(&target).unwrap();
        assert_eq!(copy.get_segments("s1").unwrap().len(), 1);
        assert_eq!(copy.search_segments("approved", 10).unwrap().len(), 1);
        // Independent of the source from here on.
        store.delete_session("s1").unwrap();
        assert_eq!(copy.get_segments("s1").unwrap().len(), 1);
    }

    #[test]
    fn a_backup_runs_without_the_write_connection() {
        let path = temp_db("backup-held");
        let target = temp_db("backup-held-target");
        let store = std::sync::Arc::new(Store::open(&path).unwrap());
        let held = store.conn.lock().unwrap();

        let (done, finished) = std::sync::mpsc::channel();
        let backup = std::sync::Arc::clone(&store);
        std::thread::spawn(move || done.send(backup.backup_to(&target).is_ok()).unwrap());
        let backed_up = finished
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a backup blocked capture's write connection");
        assert!(backed_up);
        drop(held);
    }

    #[test]
    fn a_backup_does_not_hold_up_reads() {
        let path = temp_db("backup-reading");
        let target = temp_db("backup-reading-target");
        let store = std::sync::Arc::new(Store::open(&path).unwrap());
        // A read in progress, for as long as the backup takes.
        let held = store.read.lock().unwrap();

        let (done, finished) = std::sync::mpsc::channel();
        let backup = std::sync::Arc::clone(&store);
        std::thread::spawn(move || done.send(backup.backup_to(&target).is_ok()).unwrap());
        let backed_up = finished
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a backup queued behind the read connection");
        assert!(backed_up);
        drop(held);
    }

    #[test]
    fn meta_writers_on_different_fields_cannot_undo_each_other() {
        let path = temp_db("meta-race");
        let store = std::sync::Arc::new(Store::open(&path).unwrap());
        store
            .create_session("s", "t", 100, "deepgram", &serde_json::json!({}))
            .unwrap();

        let writers: Vec<_> = ["losses", "rename"]
            .into_iter()
            .map(|who| {
                let store = std::sync::Arc::clone(&store);
                std::thread::spawn(move || {
                    for i in 0..150 {
                        let key = format!("{who}{i}");
                        store
                            .set_session_meta("s", &key, &serde_json::json!(i))
                            .unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }

        let meta = store.get_session("s").unwrap().unwrap().meta;
        let kept = meta.as_object().unwrap().len();
        assert_eq!(kept, 300, "{} writes were lost", 300 - kept);
    }

    #[test]
    fn a_failed_backup_leaves_no_partial_file() {
        let path = temp_db("backup-failed");
        let mut store = Store::open(&path).unwrap();
        // Nothing there for the backup's connection to open.
        store.path = path.with_extension("gone");
        let target = temp_db("backup-failed-target");
        assert!(store.backup_to(&target).is_err());
        assert!(!target.exists(), "a retry would hit 'already exists'");
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
