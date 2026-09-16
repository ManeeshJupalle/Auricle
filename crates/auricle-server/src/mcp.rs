//! MCP (Model Context Protocol) server: a read-only view of the transcript
//! for local agents — Claude Code, Cursor, and anything else that speaks MCP.
//!
//! Mounted at `/mcp` (Streamable HTTP) and gated on the `mcp_enabled`
//! setting: off until the user turns it on under Settings. The gate lives in
//! `api.rs` and answers 404 while disabled, so a disabled endpoint looks
//! exactly like an absent one to anything probing the port.
//!
//! **Read-only by construction.** The four tools read the transcript ring and
//! the database. None starts or stops a recording, calls an LLM, or captures
//! the screen — the agent gets the meeting, nothing else. Keep it that way:
//! the moment a tool here has a side effect, the security story in
//! `docs/API.md` stops being true.
//!
//! Every call is written to the egress ledger with destination `agent`. The
//! ledger's usual rule — classify by endpoint host — cannot work here: an MCP
//! client is a local process, but a local process may be the front end of a
//! cloud model. So the ledger records what it actually knows (something local
//! read the transcript) rather than guessing at `local` or `cloud`.
//!
//! That write happens **before** anything is disclosed, and a failure fails
//! the call. Elsewhere the ledger is best-effort, because by the time we log
//! an egress it has already happened and dropping the row is the lesser
//! harm. Here the order is ours to choose, and letting a read through with no
//! row would leave the ledger quietly incomplete — which is precisely the
//! thing that would make the whole surface not worth offering.

use std::sync::Arc;

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ErrorData as McpError, Implementation, ServerCapabilities, ServerConfig};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{tool, tool_handler, tool_router, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::AppState;

/// Cap on transcript text returned by one `get_session`. A two-hour meeting
/// runs well past this, and returning it whole would swamp the context window
/// of the agent that asked. Past the cap the transcript is cut at a segment
/// boundary and the response says so — never a silent truncation.
const MAX_TRANSCRIPT_CHARS: usize = 40_000;

/// Result count for `search` when the caller does not say.
const SEARCH_LIMIT_DEFAULT: i64 = 20;
/// Ceiling on `search` results, whatever the caller asks for.
const SEARCH_LIMIT_MAX: i64 = 200;

/// What an MCP client sees of Auricle. Cheap to clone: `AppState` is Arcs.
#[derive(Clone)]
pub struct AuricleMcp {
    state: AppState,
}

// ========================================================================
// Tool parameters and results
// ========================================================================

#[derive(Debug, Serialize, JsonSchema)]
pub struct Line {
    /// "You" (the microphone) or "Them" (system audio).
    speaker: String,
    /// Milliseconds from the start of the recording.
    t_start_ms: u64,
    text: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LiveTranscript {
    /// The session recording *right now*, or null when none is.
    ///
    /// This is not a label for `lines`. The window is a span of wall-clock
    /// time, not a slice of one recording: stopping a meeting does not clear
    /// it, so a caller can see a null `session_id` beside the previous
    /// meeting's words, or a new session's id beside words spoken before it
    /// started. Use `auricle.get_session` when you need lines that provably
    /// belong to one meeting.
    session_id: Option<String>,
    /// Width of the rolling window, from `[copilot] transcript_window_min`.
    window_minutes: u64,
    /// Oldest first. Empty means nothing was said in the window, which is not
    /// the same as nothing recording.
    lines: Vec<Line>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListSessionsParams {
    /// Optional filter. Matches a session whose title or transcript contains
    /// this text. Omit to list everything, newest first.
    #[serde(default)]
    query: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionSummary {
    id: String,
    title: String,
    /// Unix seconds.
    started_at: i64,
    /// Unix seconds; null while the session is still recording.
    ended_at: Option<i64>,
    stt_provider: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SessionList {
    sessions: Vec<SessionSummary>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetSessionParams {
    /// Session id from `auricle.list_sessions` or `auricle.search`.
    id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Session {
    id: String,
    title: String,
    started_at: i64,
    ended_at: Option<i64>,
    stt_provider: String,
    lines: Vec<Line>,
    /// True when the transcript was cut at `MAX_TRANSCRIPT_CHARS`. The tail
    /// is missing, not the head: `lines` starts at the beginning of the
    /// meeting. Use `auricle.search` to reach a specific later moment.
    truncated: bool,
    /// Summaries already generated for this session, if any. Auricle does not
    /// generate one on your behalf — this tool never calls an LLM.
    summaries: Vec<Summary>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Summary {
    /// "minutes", "action_items", "standup", "one_on_one".
    template: String,
    model: String,
    content: String,
    created_at: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Text to find. Matched as a substring, case-insensitively.
    query: String,
    /// Maximum lines to return. Defaults to 20, capped at 200.
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SearchHit {
    session_id: String,
    session_title: String,
    /// Unix seconds — when the meeting started, not when the line was said.
    started_at: i64,
    speaker: String,
    /// Milliseconds into that meeting.
    t_start_ms: i64,
    text: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SearchResults {
    hits: Vec<SearchHit>,
}

// ========================================================================
// Tools
// ========================================================================

#[tool_router]
impl AuricleMcp {
    pub fn new(state: AppState) -> AuricleMcp {
        AuricleMcp { state }
    }

    #[tool(
        name = "auricle.live_transcript",
        description = "What is being said right now. Returns the last N minutes of \
                       speech, oldest line first, labelled by speaker: 'You' is the \
                       user's microphone, 'Them' is everyone else (system audio). \
                       The window is a span of time, not a slice of one meeting — \
                       just after a recording stops it still holds that meeting's \
                       words, so do not assume every line belongs to the session \
                       named by session_id.",
        annotations(title = "Read the live transcript", read_only_hint = true)
    )]
    fn live_transcript(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<Json<LiveTranscript>, McpError> {
        let lines: Vec<Line> = self
            .state
            .ring
            .snapshot()
            .into_iter()
            .map(|s| Line {
                speaker: s.speaker,
                t_start_ms: s.t_start_ms,
                text: s.text,
            })
            .collect();
        let active = self.state.engine.active_session();
        self.note_read(
            &ctx,
            active.as_deref(),
            "live_transcript",
            chars_of(lines.iter().map(|l| &l.text)),
        )?;
        Ok(Json(LiveTranscript {
            session_id: active,
            window_minutes: self.state.engine.config().copilot.transcript_window_min,
            lines,
        }))
    }

    #[tool(
        name = "auricle.list_sessions",
        description = "List recorded meetings, newest first. With `query`, lists \
                       only meetings whose title or transcript contains that text \
                       — use it to narrow down before calling auricle.get_session. \
                       To find the specific lines that matched, use auricle.search \
                       instead.",
        annotations(title = "List meetings", read_only_hint = true)
    )]
    fn list_sessions(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ListSessionsParams>,
    ) -> Result<Json<SessionList>, McpError> {
        let rows = self
            .state
            .engine
            .store()
            .list_sessions(p.query.as_deref())
            .map_err(db_error)?;
        // Titles are LLM-written from the transcript, so they are content too:
        // measure them the way the ledger measures every other text payload.
        // Spans every meeting, so no single session owns this read.
        self.note_read(
            &ctx,
            None,
            "session_list",
            chars_of(rows.iter().map(|r| &r.title)),
        )?;
        Ok(Json(SessionList {
            sessions: rows
                .into_iter()
                .map(|r| SessionSummary {
                    id: r.id,
                    title: r.title,
                    started_at: r.started_at,
                    ended_at: r.ended_at,
                    stt_provider: r.stt_provider,
                })
                .collect(),
        }))
    }

    #[tool(
        name = "auricle.get_session",
        description = "Read one recorded meeting: its full transcript plus any \
                       summaries already generated for it. Long meetings are \
                       truncated — check the `truncated` flag.",
        annotations(title = "Read a meeting", read_only_hint = true)
    )]
    fn get_session(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<GetSessionParams>,
    ) -> Result<Json<Session>, McpError> {
        let store = self.state.engine.store();
        let row = store
            .get_session(&p.id)
            .map_err(db_error)?
            .ok_or_else(|| McpError::invalid_params(format!("no session {}", p.id), None))?;
        let segments = store.get_segments(&p.id).map_err(db_error)?;
        let summaries = store.get_summaries(&p.id).map_err(db_error)?;

        // Cut on a segment boundary so a line is never half-returned.
        let mut chars = 0usize;
        let mut lines = Vec::new();
        let mut truncated = false;
        for s in segments {
            if chars + s.text.chars().count() > MAX_TRANSCRIPT_CHARS {
                truncated = true;
                break;
            }
            chars += s.text.chars().count();
            lines.push(Line {
                speaker: s.speaker,
                t_start_ms: s.t_start_ms.max(0) as u64,
                text: s.text,
            });
        }

        // Everything this response discloses, not just the transcript: the
        // title and the summaries are user content too, and a short meeting
        // with a long summary would otherwise log as almost nothing.
        let disclosed = chars
            + row.title.chars().count()
            + summaries
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>();
        self.note_read(&ctx, Some(&p.id), "session_read", Some(disclosed as i64))?;
        Ok(Json(Session {
            id: row.id,
            title: row.title,
            started_at: row.started_at,
            ended_at: row.ended_at,
            stt_provider: row.stt_provider,
            lines,
            truncated,
            summaries: summaries
                .into_iter()
                .map(|s| Summary {
                    template: s.template,
                    model: s.model,
                    content: s.content,
                    created_at: s.created_at,
                })
                .collect(),
        }))
    }

    #[tool(
        name = "auricle.search",
        description = "Find transcript lines across every recorded meeting. Returns \
                       the matching lines themselves, each tagged with the meeting it \
                       came from and how far into it the line was spoken — so you can \
                       answer a question without pulling whole transcripts.",
        annotations(title = "Search transcripts", read_only_hint = true)
    )]
    fn search(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SearchParams>,
    ) -> Result<Json<SearchResults>, McpError> {
        let limit = p
            .limit
            .unwrap_or(SEARCH_LIMIT_DEFAULT)
            .clamp(1, SEARCH_LIMIT_MAX);
        let hits = self
            .state
            .engine
            .store()
            .search_segments(&p.query, limit)
            .map_err(db_error)?;
        // Hits can span several meetings, so no single session owns this read.
        self.note_read(
            &ctx,
            None,
            "transcript_search",
            chars_of(
                hits.iter()
                    .map(|h| &h.text)
                    .chain(hits.iter().map(|h| &h.session_title)),
            ),
        )?;
        Ok(Json(SearchResults {
            hits: hits
                .into_iter()
                .map(|h| SearchHit {
                    session_id: h.session_id,
                    session_title: h.session_title,
                    started_at: h.started_at,
                    speaker: h.speaker,
                    t_start_ms: h.t_start_ms,
                    text: h.text,
                })
                .collect(),
        }))
    }
}

#[tool_handler]
impl ServerHandler for AuricleMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("auricle", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Auricle records the user's meetings locally — system audio and \
                 microphone, transcribed on this machine. These tools read that \
                 transcript and nothing else: they cannot start or stop a \
                 recording, see the screen, or run a model.\n\n\
                 'You' is the user speaking into their microphone. 'Them' is \
                 everything their computer played — the other people in the call.\n\n\
                 For what is happening right now, call auricle.live_transcript. For \
                 something said earlier, call auricle.search before pulling a whole \
                 meeting with auricle.get_session.\n\n\
                 This is the user's private conversation, including anything said \
                 around the meeting. Read what the task needs and leave the rest.",
            )
    }
}

impl AuricleMcp {
    /// Record one agent read in the egress ledger.
    ///
    /// `items` is the rough size in characters, the unit the rest of the
    /// ledger uses for text. The content itself is never recorded, here or
    /// anywhere else in the ledger.
    fn note_read(
        &self,
        ctx: &RequestContext<RoleServer>,
        session_id: Option<&str>,
        kind: &str,
        items: Option<i64>,
    ) -> Result<(), McpError> {
        // `client_info()` reads the calling client's identity from this
        // request's metadata, falling back to the handshake only for legacy
        // sessions. Reading `peer.peer_info()` directly would be wrong: for a
        // stateless call rmcp synthesizes peer info from its *own* build
        // identity, so every such row would be filed against "rmcp" — a
        // plausible-looking name that is not the client's.
        //
        // Still self-reported either way: it names a well-behaved client, it
        // does not authenticate one.
        let client = ctx
            .client_info()
            .map(|i| i.name)
            .unwrap_or_else(|| "unknown".to_string());
        crate::egress::record_checked(
            &self.state.engine.store(),
            session_id,
            kind,
            ("agent", None),
            &client,
            items,
            None,
        )
        .map_err(|e| {
            // Fail closed. The ledger is the whole basis on which this server
            // is allowed to read private meetings; disclosing content we
            // could not record would make it quietly incomplete, which is
            // worse than refusing the call.
            McpError::internal_error(
                format!("refusing to read: the egress ledger could not be written ({e})"),
                None,
            )
        })
    }
}

/// Characters of user content in a response — the ledger's size unit, so a
/// row reads "this agent received N characters of your transcript".
fn chars_of<'a>(texts: impl Iterator<Item = &'a String>) -> Option<i64> {
    Some(texts.map(|t| t.chars().count()).sum::<usize>() as i64)
}

fn db_error(e: auricle_core::Error) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

/// Build the tower service to nest at `/mcp`.
///
/// `LocalSessionManager` keeps sessions in memory: they live and die with the
/// daemon, like the transcript ring.
///
/// rmcp's default `Host` allowlist is disabled deliberately. It permits only
/// loopback authorities, which silently 403s every request to a daemon bound
/// to a LAN address — a configuration Auricle supports, and gates behind a
/// bearer token (see `serve`). `auth_middleware` already covers what that
/// allowlist is for and covers it better: it rejects non-loopback `Host`
/// values on tokenless binds (the DNS-rebinding case) and demands the token
/// everywhere else. Two host checks where one is token-aware and the other is
/// not just means the blind one decides.
pub fn service(state: AppState) -> StreamableHttpService<AuricleMcp, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(AuricleMcp::new(state.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().disable_allowed_hosts(),
    )
}
