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

use std::sync::Arc;

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ErrorData as McpError, Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{tool, tool_handler, tool_router, Peer, RoleServer, ServerHandler};
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
    /// The session currently recording, or null when nothing is.
    session_id: Option<String>,
    /// Width of the rolling window, from `[copilot] transcript_window_min`.
    window_minutes: u64,
    /// Oldest first. Empty when nothing has been said in the window — which
    /// is not the same as nothing recording; check `session_id`.
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
        description = "What is being said right now. Returns the rolling window of \
                       the meeting in progress, oldest line first, labelled by \
                       speaker: 'You' is the user's microphone, 'Them' is everyone \
                       else (system audio). Returns an empty line list when nothing \
                       is being recorded.",
        annotations(title = "Read the live transcript", read_only_hint = true)
    )]
    fn live_transcript(&self, peer: Peer<RoleServer>) -> Json<LiveTranscript> {
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
        self.note_read(
            &peer,
            "live_transcript",
            chars_of(lines.iter().map(|l| &l.text)),
        );
        Json(LiveTranscript {
            session_id: self.state.engine.active_session(),
            window_minutes: self.state.engine.config().copilot.transcript_window_min,
            lines,
        })
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
        peer: Peer<RoleServer>,
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
        self.note_read(
            &peer,
            "session_list",
            chars_of(rows.iter().map(|r| &r.title)),
        );
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
        peer: Peer<RoleServer>,
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

        self.note_read(&peer, "session_read", Some(chars as i64));
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
        peer: Peer<RoleServer>,
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
        self.note_read(
            &peer,
            "transcript_search",
            chars_of(hits.iter().map(|h| &h.text)),
        );
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
    fn note_read(&self, peer: &Peer<RoleServer>, kind: &str, items: Option<i64>) {
        // The client's self-reported name from the MCP handshake ("claude-code",
        // "cursor-vscode"). Self-reported, so it identifies a well-behaved client
        // rather than authenticating anything.
        let client = peer
            .peer_info()
            .map(|i| i.client_info.name.clone())
            .unwrap_or_else(|| "unknown".to_string());
        crate::egress::record(
            &self.state.engine.store(),
            self.state.engine.active_session().as_deref(),
            kind,
            ("agent", None),
            &client,
            items,
            None,
        );
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
/// daemon, like the transcript ring. rmcp's own defaults already restrict the
/// `Host` header to loopback, which doubles up with `auth_middleware`'s
/// DNS-rebinding check rather than replacing it.
pub fn service(state: AppState) -> StreamableHttpService<AuricleMcp, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(AuricleMcp::new(state.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}
