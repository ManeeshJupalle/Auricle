//! Dictation: the hotkey's state machine and the engine calls behind it.
//!
//! Every hotkey event and engine answer is an [`Input`] to [`Dictation::on`],
//! a pure reducer that answers with the [`Effect`]s to carry out, so the
//! whole lifecycle is testable without Tauri, an engine, or a desktop.
//! [`run`] is the thin loop that carries them out.
//!
//! Stop and cancel carry the dictation's id once the start has answered
//! with it, and the engine answers 404 if that dictation is gone. A stop
//! sent while the start is still out has no id yet and can overtake the
//! start and find nothing, so a 404 then means "not yet", and the stop goes
//! out again, with the id, once the start has answered. A 404 for our own id
//! means the dictation is over: nothing to paste, nothing left to cancel.
//!
//! Every phase has a deadline, so a hung engine can't keep the machine, or
//! Esc, held: Esc is taken only while it can still cancel, and given back as
//! soon as it can't.
//!
//! Dictated text never reaches a log: nothing here prints an effect.

use std::time::Duration;

use futures_util::future::AbortHandle;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::config::DictationMode;
use crate::dictate::{self, Pasted, Target};

/// Stop and cancel wait for the engine, which gives up on its own after 150 s.
const ENGINE_WAIT: Duration = Duration::from_secs(170);
/// A start the engine says nothing about for this long is given up. A model
/// download reports progress, which counts as something.
const START_SILENCE: Duration = Duration::from_secs(30);
/// The engine stops a dictation at 5 minutes; past this, the overlay stops it.
const LISTEN_MAX: Duration = Duration::from_secs(5 * 60 + 30);
/// How long Esc stays taken while the text is on its way.
const ESC_WHILE_TRANSCRIBING: Duration = Duration::from_secs(30);
const CANCEL_WAIT: Duration = Duration::from_secs(15);
/// Longer than the paste's own bounds put together.
const PASTE_WAIT: Duration = Duration::from_secs(30);

/// The engine's dictation provider when `dictation_provider` is unset.
const DEFAULT_PROVIDER: &str = "whisper-local";
const UNREACHABLE: &str = "The Auricle engine isn't answering.";
pub const STARTING: &str = "Starting…";
pub const TRANSCRIBING: &str = "Transcribing…";
pub const PASTING: &str = "Pasting…";
const NOTHING_HEARD: &str = "Nothing was heard.";
const ALREADY_ENDED: &str =
    "The engine had already ended this dictation, so there was nothing to paste.";
const START_TIMED_OUT: &str =
    "The engine didn't start the dictation within 30 seconds, so Auricle cancelled it.";
const CANCEL_UNCONFIRMED: &str =
    "The engine didn't confirm the cancel, so the mic may still be on.";
const PASTE_STUCK: &str = "The paste didn't finish: another program may be holding the clipboard.";

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// The hotkey went down, with the window that had focus at that moment.
    Press(Option<Target>),
    Release,
    Escape,
    /// The start's answer: the dictation's id, or why not.
    Started {
        run: u64,
        result: Result<String, String>,
    },
    /// A stop's text, or a cancel's empty one.
    Ended {
        run: u64,
        result: Result<Heard, EndError>,
    },
    /// The engine ended a dictation without being asked: its 5-minute cap.
    EngineStopped {
        run: u64,
        id: String,
    },
    /// The engine is busy on the start: a model download reporting progress.
    Alive {
        run: u64,
    },
    Pasted {
        run: u64,
        outcome: Pasted,
    },
    /// A deadline set by [`Effect::Deadline`] came due.
    Due {
        run: u64,
        step: u64,
    },
}

impl Input {
    fn run(&self) -> Option<u64> {
        match self {
            Input::Started { run, .. }
            | Input::Ended { run, .. }
            | Input::EngineStopped { run, .. }
            | Input::Alive { run }
            | Input::Pasted { run, .. }
            | Input::Due { run, .. } => Some(*run),
            _ => None,
        }
    }
}

/// What a stop brought back: the text, and anything the user should know
/// about it, as plain sentences.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Heard {
    pub text: String,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EndError {
    /// 404: no dictation was active, or not the one named.
    NotActive,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// A dictation is beginning: take Esc, and watch the engine's events.
    Begin,
    Start,
    /// Stop or cancel, naming the dictation once its id is known.
    Stop(Option<String>),
    Cancel(Option<String>),
    Paste(String, Option<Target>),
    Pill(Pill),
    /// Esc has no more use: give it back and stop watching. Safe to repeat.
    End,
    /// Send [`Input::Due`] with this step after this long.
    Deadline {
        step: u64,
        after: Duration,
    },
}

/// What the dictation pill shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Pill {
    Hidden,
    Listening,
    Working(&'static str),
    Done(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Idle,
    Starting { target: Option<Target> },
    Listening { id: String, target: Option<Target> },
    Ending(Ending),
    Pasting { notes: Vec<String> },
}

#[derive(Debug, Clone, PartialEq)]
enum Start {
    Pending,
    Listened,
    Refused(String),
}

/// A stop or a cancel is out.
#[derive(Debug, Clone, PartialEq)]
struct Ending {
    cancel: bool,
    target: Option<Target>,
    start: Start,
    id: Option<String>,
    /// Sent before the start answered, so a 404 may only mean it overtook it.
    early: bool,
    /// It came back 404 while the start was still out.
    missed: bool,
    resent: bool,
    /// Esc while transcribing: the text is no longer wanted.
    discard: bool,
    /// Esc was given back while the text was still on its way.
    esc_back: bool,
}

pub struct Dictation {
    mode: DictationMode,
    phase: Phase,
    /// Numbers each dictation, so answers from an older one are told apart.
    run: u64,
    /// Numbers each deadline, so only the latest one counts.
    step: u64,
}

impl Dictation {
    pub fn new(mode: DictationMode) -> Self {
        Dictation {
            mode,
            phase: Phase::Idle,
            run: 0,
            step: 0,
        }
    }

    pub fn run(&self) -> u64 {
        self.run
    }

    pub fn on(&mut self, input: Input) -> Vec<Effect> {
        if input.run().is_some_and(|run| run != self.run) {
            return Vec::new();
        }
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        let (next, effects) = self.step(phase, input);
        self.phase = next;
        effects
    }

    /// A new deadline, which replaces any earlier one.
    fn deadline(&mut self, after: Duration) -> Effect {
        self.step += 1;
        Effect::Deadline {
            step: self.step,
            after,
        }
    }

    fn due(&self, input: &Input) -> bool {
        matches!(input, Input::Due { step, .. } if *step == self.step)
    }

    fn step(&mut self, phase: Phase, input: Input) -> (Phase, Vec<Effect>) {
        let toggle = self.mode == DictationMode::Toggle;
        let due = self.due(&input);
        match (phase, input) {
            (Phase::Idle, Input::Press(target)) => {
                self.run += 1;
                // The start first: every moment before the mic opens loses words.
                let wait = self.deadline(START_SILENCE);
                (
                    Phase::Starting { target },
                    vec![
                        Effect::Start,
                        Effect::Begin,
                        Effect::Pill(Pill::Working(STARTING)),
                        wait,
                    ],
                )
            }
            // A paste that outlasted its deadline still says how it went.
            (Phase::Idle, Input::Pasted { outcome, .. }) => {
                (Phase::Idle, vec![Effect::Pill(pasted_pill(&outcome, &[]))])
            }
            (Phase::Starting { target }, Input::Alive { .. }) => {
                let wait = self.deadline(START_SILENCE);
                (Phase::Starting { target }, vec![wait])
            }
            (Phase::Starting { target }, Input::Release) if !toggle => {
                self.ending(false, target, Start::Pending, None)
            }
            (Phase::Starting { target }, Input::Press(_)) if toggle => {
                self.ending(false, target, Start::Pending, None)
            }
            (Phase::Starting { target }, Input::Escape) => {
                self.ending(true, target, Start::Pending, None)
            }
            (Phase::Starting { target }, Input::Due { .. }) if due => {
                let (phase, mut effects) = self.ending(true, target, Start::Pending, None);
                for effect in &mut effects {
                    if *effect == Effect::Pill(Pill::Hidden) {
                        *effect = Effect::Pill(Pill::Failed(START_TIMED_OUT.to_string()));
                    }
                }
                (phase, effects)
            }
            (Phase::Starting { target }, Input::Started { result, .. }) => match result {
                Ok(id) => {
                    let wait = self.deadline(LISTEN_MAX);
                    (
                        Phase::Listening { id, target },
                        vec![Effect::Pill(Pill::Listening), wait],
                    )
                }
                Err(why) => over_loud(why),
            },
            (Phase::Listening { id, target }, Input::Release) if !toggle => {
                self.ending(false, target, Start::Listened, Some(id))
            }
            (Phase::Listening { id, target }, Input::Press(_)) if toggle => {
                self.ending(false, target, Start::Listened, Some(id))
            }
            // The cap stopped it, or should have by now; its text waits for
            // a stop to collect.
            (Phase::Listening { id, target }, Input::EngineStopped { id: stopped, .. })
                if stopped == id =>
            {
                self.ending(false, target, Start::Listened, Some(id))
            }
            (Phase::Listening { id, target }, Input::Due { .. }) if due => {
                self.ending(false, target, Start::Listened, Some(id))
            }
            (Phase::Listening { id, target }, Input::Escape) => {
                self.ending(true, target, Start::Listened, Some(id))
            }
            (Phase::Ending(e), input) => self.finish(e, input, due),
            (Phase::Pasting { notes }, Input::Pasted { outcome, .. }) => (
                Phase::Idle,
                vec![Effect::Pill(pasted_pill(&outcome, &notes))],
            ),
            (Phase::Pasting { .. }, Input::Due { .. }) if due => over_loud(PASTE_STUCK.to_string()),
            (phase, _) => (phase, Vec::new()),
        }
    }

    fn ending(
        &mut self,
        cancel: bool,
        target: Option<Target>,
        start: Start,
        id: Option<String>,
    ) -> (Phase, Vec<Effect>) {
        let early = start == Start::Pending;
        let effects = if cancel {
            // Esc did its job; it goes back at once.
            vec![
                Effect::Cancel(id.clone()),
                Effect::End,
                Effect::Pill(Pill::Hidden),
                self.deadline(CANCEL_WAIT),
            ]
        } else {
            vec![
                Effect::Stop(id.clone()),
                Effect::Pill(Pill::Working(TRANSCRIBING)),
                self.deadline(ESC_WHILE_TRANSCRIBING),
            ]
        };
        let e = Ending {
            cancel,
            target,
            start,
            id,
            early,
            missed: false,
            resent: false,
            discard: false,
            esc_back: false,
        };
        (Phase::Ending(e), effects)
    }

    fn finish(&mut self, mut e: Ending, input: Input, due: bool) -> (Phase, Vec<Effect>) {
        let again = |e: &Ending| {
            if e.cancel {
                Effect::Cancel(e.id.clone())
            } else {
                Effect::Stop(e.id.clone())
            }
        };
        match input {
            Input::Escape if !e.cancel && !e.discard => {
                e.discard = true;
                (
                    Phase::Ending(e),
                    vec![Effect::End, Effect::Pill(Pill::Hidden)],
                )
            }
            Input::Started { result, .. } if e.start == Start::Pending => match result {
                Ok(id) => {
                    e.id = Some(id);
                    e.start = Start::Listened;
                    if e.missed {
                        e.missed = false;
                        e.resent = true;
                        let effects = vec![again(&e)];
                        (Phase::Ending(e), effects)
                    } else {
                        (Phase::Ending(e), Vec::new())
                    }
                }
                Err(why) if e.missed => over(&e, why),
                Err(why) => {
                    e.start = Start::Refused(why);
                    (Phase::Ending(e), Vec::new())
                }
            },
            Input::Ended { result, .. } => match result {
                Ok(_) if e.cancel || e.discard => (Phase::Idle, vec![Effect::End]),
                Ok(heard) if heard.text.trim().is_empty() => {
                    let why = if e.start == Start::Listened {
                        NOTHING_HEARD
                    } else {
                        self.too_soon()
                    };
                    over_loud(with_notes(why, &heard.notes))
                }
                Ok(heard) => {
                    let wait = self.deadline(PASTE_WAIT);
                    (
                        Phase::Pasting { notes: heard.notes },
                        vec![
                            Effect::End,
                            Effect::Pill(Pill::Working(PASTING)),
                            Effect::Paste(heard.text, e.target),
                            wait,
                        ],
                    )
                }
                Err(EndError::NotActive) => match &e.start {
                    Start::Pending => {
                        e.missed = true;
                        (Phase::Ending(e), Vec::new())
                    }
                    // Sent without an id, before the start answered: it may
                    // have overtaken the start. Once more, with the id.
                    Start::Listened if e.early && !e.resent => {
                        e.resent = true;
                        let effects = vec![again(&e)];
                        (Phase::Ending(e), effects)
                    }
                    Start::Listened => over(&e, ALREADY_ENDED.to_string()),
                    Start::Refused(why) => {
                        let why = why.clone();
                        over(&e, why)
                    }
                },
                Err(EndError::Failed(why)) if e.cancel => {
                    over_loud(format!("Could not cancel. {why}"))
                }
                Err(EndError::Failed(why)) => over(&e, why),
            },
            Input::Due { .. } if due => {
                if e.cancel {
                    return over_loud(CANCEL_UNCONFIRMED.to_string());
                }
                if e.esc_back {
                    return over(&e, UNREACHABLE.to_string());
                }
                // Still waiting for the text, but Esc is given back.
                e.esc_back = true;
                let wait = self.deadline(ENGINE_WAIT);
                let effects = if e.discard {
                    vec![wait]
                } else {
                    vec![Effect::End, Effect::Pill(Pill::Working(TRANSCRIBING)), wait]
                };
                (Phase::Ending(e), effects)
            }
            _ => (Phase::Ending(e), Vec::new()),
        }
    }

    fn too_soon(&self) -> &'static str {
        match self.mode {
            DictationMode::Hold => {
                "Let go before the mic was ready. Hold the keys until the pill says Listening."
            }
            DictationMode::Toggle => {
                "Stopped before the mic was ready. Wait for Listening before you press again."
            }
        }
    }
}

/// The dictation is over without text. Only someone still waiting on the
/// text hears why: not after a cancel, nor after Esc while transcribing.
fn over(e: &Ending, why: String) -> (Phase, Vec<Effect>) {
    if e.cancel || e.discard {
        (Phase::Idle, vec![Effect::End])
    } else {
        over_loud(why)
    }
}

fn over_loud(why: String) -> (Phase, Vec<Effect>) {
    (
        Phase::Idle,
        vec![Effect::End, Effect::Pill(Pill::Failed(why))],
    )
}

fn with_notes(message: &str, notes: &[String]) -> String {
    std::iter::once(message)
        .chain(notes.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

fn pasted_pill(outcome: &Pasted, notes: &[String]) -> Pill {
    match outcome {
        Pasted::Landed { restored: true } => Pill::Done(with_notes("Pasted.", notes)),
        Pasted::Landed { restored: false } => Pill::Done(with_notes(
            "Pasted. Your earlier clipboard could not be fully put back.",
            notes,
        )),
        Pasted::Unconfirmed => Pill::Done(with_notes(
            "Pasted, as far as Auricle can tell. The text also stays on the clipboard, and \
             your earlier clipboard wasn't put back.",
            notes,
        )),
        Pasted::OnClipboard(dictate::Left::NotRead) => Pill::Failed(with_notes(
            &format!(
                "{} The text is on the clipboard: press Ctrl+V if it isn't there.",
                dictate::Left::NotRead.reason()
            ),
            notes,
        )),
        Pasted::OnClipboard(left) => Pill::Failed(with_notes(
            &format!(
                "{} The text is on the clipboard: press Ctrl+V.",
                left.reason()
            ),
            notes,
        )),
        Pasted::Lost(e) => Pill::Failed(with_notes(
            &format!("The text could not be pasted or put on the clipboard. {e}"),
            notes,
        )),
    }
}

/// `message` as a sentence: a capital first letter and a full stop.
fn sentence(message: &str) -> String {
    let message = message.trim();
    let mut chars = message.chars();
    let mut out: String = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => return String::new(),
    };
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

/// A stop's answer as text and plain notes: words the engine says may be
/// missing (`incomplete`, `note`), and polish that didn't happen
/// (`polish_note`).
fn heard(body: &serde_json::Value) -> Heard {
    let text = body["text"].as_str().unwrap_or_default().to_string();
    let mut notes = Vec::new();
    if body["incomplete"] == true {
        notes.push(match body["note"].as_str().map(sentence) {
            Some(why) if !why.is_empty() => format!("Some words may be missing. {why}"),
            _ => "Some words may be missing.".to_string(),
        });
    }
    if let Some(why) = body["polish_note"].as_str().map(str::trim) {
        if !why.is_empty() {
            let mut note = format!("Not polished: {why}");
            if !note.ends_with(['.', '!', '?']) {
                note.push('.');
            }
            notes.push(note);
        }
    }
    Heard { text, notes }
}

/// The engine's `error`, or the status when it gave none.
async fn reason(resp: reqwest::Response) -> String {
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    match body["error"].as_str() {
        Some(error) => sentence(error),
        None => format!("The engine answered {status}."),
    }
}

/// POST /dictation/start: the new dictation's id, or why there is none.
/// No timeout: a first-run model download can take minutes. The reducer's
/// deadline, kept alive by the download's progress, ends a hung wait.
pub async fn start(http: &reqwest::Client, base: &str) -> Result<String, String> {
    let resp = http
        .post(format!("{base}/api/v1/dictation/start"))
        .send()
        .await
        .map_err(|_| UNREACHABLE.to_string())?;
    match resp.status().as_u16() {
        201 => {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            body["id"]
                .as_str()
                .map(String::from)
                .ok_or_else(|| "The engine started a dictation without an id.".to_string())
        }
        404 | 405 => Err("This engine can't dictate. Update Auricle.".to_string()),
        _ => Err(reason(resp).await),
    }
}

/// POST /dictation/stop: the text heard, once the last chunk is transcribed.
pub async fn stop(http: &reqwest::Client, base: &str, id: Option<&str>) -> Result<Heard, EndError> {
    let resp = end(http, base, "stop", id).await?;
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    Ok(heard(&body))
}

/// POST /dictation/cancel: everything heard is thrown away.
pub async fn cancel(
    http: &reqwest::Client,
    base: &str,
    id: Option<&str>,
) -> Result<Heard, EndError> {
    end(http, base, "cancel", id)
        .await
        .map(|_| Heard::default())
}

async fn end(
    http: &reqwest::Client,
    base: &str,
    action: &str,
    id: Option<&str>,
) -> Result<reqwest::Response, EndError> {
    let mut req = http
        .post(format!("{base}/api/v1/dictation/{action}"))
        .timeout(ENGINE_WAIT);
    if let Some(id) = id {
        req = req.json(&serde_json::json!({ "id": id }));
    }
    let resp = req
        .send()
        .await
        .map_err(|_| EndError::Failed(UNREACHABLE.to_string()))?;
    match resp.status().as_u16() {
        200 => Ok(resp),
        404 => Err(EndError::NotActive),
        _ => Err(EndError::Failed(reason(resp).await)),
    }
}

/// What the effects loop works with.
pub struct Driver {
    pub app: tauri::AppHandle,
    pub mode: DictationMode,
    /// The dictate hotkey as configured, for the pill's hints.
    pub hotkey: String,
    pub http: reqwest::Client,
    pub base: String,
    pub esc: Shortcut,
}

/// Carry out the machine's effects for the life of the overlay. `feed` is
/// how the engine's answers come back in as inputs.
pub async fn run(
    driver: Driver,
    mut inputs: UnboundedReceiver<Input>,
    feed: UnboundedSender<Input>,
) {
    let Driver {
        app,
        mode,
        hotkey,
        http,
        base,
        esc,
    } = driver;
    let mut machine = Dictation::new(mode);
    let mut watch: Option<AbortHandle> = None;
    let mut esc_on = false;
    while let Some(input) = inputs.recv().await {
        for effect in machine.on(input) {
            let run = machine.run();
            match effect {
                Effect::Begin => {
                    // Esc is only taken while a dictation needs it: held
                    // globally, it would be swallowed from every app.
                    esc_on = set_esc(&app, esc, true).await;
                    let (handle, registration) = AbortHandle::new_pair();
                    watch = Some(handle);
                    let (http, base, feed) = (http.clone(), base.clone(), feed.clone());
                    tauri::async_runtime::spawn(async move {
                        let _ = crate::ws::listen(&http, &base, registration, |event| {
                            let input = match event["type"].as_str() {
                                Some("dictation_stopped") => match event["id"].as_str() {
                                    Some(id) => Input::EngineStopped {
                                        run,
                                        id: id.to_string(),
                                    },
                                    None => return,
                                },
                                Some("model_download_started" | "model_download_progress") => {
                                    Input::Alive { run }
                                }
                                _ => return,
                            };
                            let _ = feed.send(input);
                        })
                        .await;
                    });
                }
                Effect::Start => {
                    let (http, base, feed) = (http.clone(), base.clone(), feed.clone());
                    tauri::async_runtime::spawn(async move {
                        let result = start(&http, &base).await;
                        let _ = feed.send(Input::Started { run, result });
                    });
                }
                Effect::Stop(id) => send_end(&http, &base, &feed, run, false, id),
                Effect::Cancel(id) => send_end(&http, &base, &feed, run, true, id),
                Effect::Paste(text, target) => {
                    let feed = feed.clone();
                    tauri::async_runtime::spawn_blocking(move || {
                        let outcome = dictate::paste_text(&text, target);
                        let _ = feed.send(Input::Pasted { run, outcome });
                    });
                }
                Effect::Pill(pill) => crate::pill::show(&app, &pill, mode, &hotkey, esc_on),
                Effect::End => {
                    if esc_on {
                        set_esc(&app, esc, false).await;
                        esc_on = false;
                    }
                    if let Some(handle) = watch.take() {
                        handle.abort();
                    }
                }
                Effect::Deadline { step, after } => {
                    let feed = feed.clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(after).await;
                        let _ = feed.send(Input::Due { run, step });
                    });
                }
            }
        }
    }
}

/// Send a stop or a cancel, and feed its answer back in.
fn send_end(
    http: &reqwest::Client,
    base: &str,
    feed: &UnboundedSender<Input>,
    run: u64,
    cancelling: bool,
    id: Option<String>,
) {
    let (http, base, feed) = (http.clone(), base.to_string(), feed.clone());
    tauri::async_runtime::spawn(async move {
        let id = id.as_deref();
        let result = if cancelling {
            cancel(&http, &base, id).await
        } else {
            stop(&http, &base, id).await
        };
        let _ = feed.send(Input::Ended { run, result });
    });
}

/// Register or release Esc. Off the runtime: the plugin waits on the main
/// thread for it.
async fn set_esc(app: &tauri::AppHandle, esc: Shortcut, on: bool) -> bool {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let shortcuts = app.global_shortcut();
        if on {
            shortcuts.register(esc).is_ok()
        } else {
            shortcuts.unregister(esc).is_ok()
        }
    })
    .await
    .unwrap_or(false)
}

/// What the setup check reports about dictation's provider, from the
/// engine's settings and provider list.
pub fn provider_info(
    settings: &serde_json::Value,
    providers: &serde_json::Value,
) -> serde_json::Value {
    let id = settings["dictation_provider"]
        .as_str()
        .unwrap_or(DEFAULT_PROVIDER);
    let entry = providers["providers"]
        .as_array()
        .and_then(|list| list.iter().find(|p| p["id"] == id));
    serde_json::json!({
        "id": id,
        "local": entry.is_some_and(|p| p["kind"] == "local"),
        "ready": entry.is_some_and(|p| p["ready"] == true),
        "detail": entry.and_then(|p| p["detail"].as_str()),
        "is_default": id == DEFAULT_PROVIDER,
    })
}

/// Read the provider dictation will use; None if the engine won't say.
pub async fn provider(http: &reqwest::Client, base: &str) -> Option<serde_json::Value> {
    let get = |path: &'static str| async move {
        http.get(format!("{base}/api/v1/{path}"))
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .ok()?
            .json::<serde_json::Value>()
            .await
            .ok()
    };
    let settings = get("settings").await?;
    let providers = get("providers").await?;
    Some(provider_info(&settings, &providers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictate::Left;

    const WIN: Target = Target {
        hwnd: 42,
        pid: 7,
        focus: 43,
        focus_pid: 7,
    };

    fn started(m: &Dictation, id: &str) -> Input {
        Input::Started {
            run: m.run(),
            result: Ok(id.to_string()),
        }
    }

    fn refused(m: &Dictation, why: &str) -> Input {
        Input::Started {
            run: m.run(),
            result: Err(why.to_string()),
        }
    }

    fn heard(m: &Dictation, text: &str) -> Input {
        Input::Ended {
            run: m.run(),
            result: Ok(Heard {
                text: text.to_string(),
                notes: Vec::new(),
            }),
        }
    }

    fn not_active(m: &Dictation) -> Input {
        Input::Ended {
            run: m.run(),
            result: Err(EndError::NotActive),
        }
    }

    /// The deadline the machine set last, come due.
    fn due(m: &Dictation) -> Input {
        Input::Due {
            run: m.run(),
            step: m.step,
        }
    }

    fn failed(text: &str) -> Effect {
        Effect::Pill(Pill::Failed(text.to_string()))
    }

    fn id(id: &str) -> Option<String> {
        Some(id.to_string())
    }

    /// The effects without the deadlines, for tests about something else.
    fn acts(effects: Vec<Effect>) -> Vec<Effect> {
        effects
            .into_iter()
            .filter(|e| !matches!(e, Effect::Deadline { .. }))
            .collect()
    }

    /// How long the last deadline among `effects` waits.
    fn waits(effects: &[Effect]) -> Option<Duration> {
        effects.iter().rev().find_map(|e| match e {
            Effect::Deadline { after, .. } => Some(*after),
            _ => None,
        })
    }

    fn listening(mode: DictationMode) -> Dictation {
        let mut m = Dictation::new(mode);
        m.on(Input::Press(Some(WIN)));
        m.on(started(&m, "d1"));
        m
    }

    fn paste(text: &str) -> Vec<Effect> {
        vec![
            Effect::End,
            Effect::Pill(Pill::Working(PASTING)),
            Effect::Paste(text.to_string(), Some(WIN)),
        ]
    }

    #[test]
    fn hold_starts_on_press_and_pastes_on_release() {
        let mut m = Dictation::new(DictationMode::Hold);
        let effects = m.on(Input::Press(Some(WIN)));
        assert_eq!(waits(&effects), Some(START_SILENCE));
        assert_eq!(
            acts(effects),
            vec![
                Effect::Start,
                Effect::Begin,
                Effect::Pill(Pill::Working(STARTING))
            ]
        );
        assert_eq!(
            acts(m.on(started(&m, "d1"))),
            vec![Effect::Pill(Pill::Listening)]
        );
        assert_eq!(
            acts(m.on(Input::Release)),
            vec![
                Effect::Stop(id("d1")),
                Effect::Pill(Pill::Working(TRANSCRIBING))
            ],
            "the stop names the dictation"
        );
        let effects = m.on(heard(&m, "hello there"));
        assert_eq!(waits(&effects), Some(PASTE_WAIT));
        assert_eq!(acts(effects), paste("hello there"));
        let outcome = Pasted::Landed { restored: true };
        assert_eq!(
            m.on(Input::Pasted {
                run: m.run(),
                outcome
            }),
            vec![Effect::Pill(Pill::Done("Pasted.".to_string()))]
        );
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn toggle_ignores_release_and_stops_on_the_second_press() {
        let mut m = listening(DictationMode::Toggle);
        assert!(m.on(Input::Release).is_empty());
        assert_eq!(
            acts(m.on(Input::Press(None))),
            vec![
                Effect::Stop(id("d1")),
                Effect::Pill(Pill::Working(TRANSCRIBING))
            ]
        );
        assert_eq!(acts(m.on(heard(&m, "ok"))), paste("ok"));
    }

    #[test]
    fn a_second_press_in_hold_mode_changes_nothing() {
        let mut m = listening(DictationMode::Hold);
        assert!(m.on(Input::Press(None)).is_empty());
        assert!(matches!(m.phase, Phase::Listening { .. }));
    }

    #[test]
    fn letting_go_while_the_model_loads_stops_at_once_and_says_why_nothing_came() {
        let mut m = Dictation::new(DictationMode::Hold);
        m.on(Input::Press(Some(WIN)));
        assert_eq!(
            acts(m.on(Input::Release)),
            vec![
                Effect::Stop(None),
                Effect::Pill(Pill::Working(TRANSCRIBING))
            ],
            "no id yet"
        );
        assert!(m
            .on(refused(&m, "The dictation was stopped before it began."))
            .is_empty());
        let effects = m.on(heard(&m, ""));
        assert_eq!(effects, vec![Effect::End, failed(m.too_soon())]);
        assert!(m.too_soon().contains("Hold the keys"));
    }

    #[test]
    fn the_same_holds_when_the_empty_text_beats_the_start_answer() {
        let mut m = Dictation::new(DictationMode::Toggle);
        m.on(Input::Press(Some(WIN)));
        m.on(Input::Press(None));
        let effects = m.on(heard(&m, ""));
        assert_eq!(effects, vec![Effect::End, failed(m.too_soon())]);
        // The start's own answer comes after it's over and changes nothing.
        assert!(m.on(refused(&m, "stopped")).is_empty());
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn a_stop_that_overtook_the_start_goes_again_with_the_id_once_the_start_answers() {
        let mut m = Dictation::new(DictationMode::Hold);
        m.on(Input::Press(Some(WIN)));
        m.on(Input::Release);
        assert!(m.on(not_active(&m)).is_empty(), "wait for the start");
        assert_eq!(m.on(started(&m, "d1")), vec![Effect::Stop(id("d1"))]);
        assert_eq!(acts(m.on(heard(&m, "late words"))), paste("late words"));
    }

    #[test]
    fn the_overtaken_stop_can_also_answer_after_the_start() {
        let mut m = Dictation::new(DictationMode::Hold);
        m.on(Input::Press(Some(WIN)));
        m.on(Input::Release);
        assert!(m.on(started(&m, "d1")).is_empty());
        assert_eq!(m.on(not_active(&m)), vec![Effect::Stop(id("d1"))]);
        // A 404 for our own id is real: it's over.
        assert_eq!(
            m.on(not_active(&m)),
            vec![Effect::End, failed(ALREADY_ENDED)]
        );
    }

    #[test]
    fn a_404_for_our_own_id_is_not_retried() {
        let mut m = listening(DictationMode::Hold);
        m.on(Input::Release);
        assert_eq!(
            m.on(not_active(&m)),
            vec![Effect::End, failed(ALREADY_ENDED)]
        );
        // And for a cancel it means there is nothing left to cancel.
        let mut m = listening(DictationMode::Toggle);
        m.on(Input::Escape);
        assert_eq!(m.on(not_active(&m)), vec![Effect::End]);
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn a_meeting_in_progress_refuses_the_start_with_the_engines_reason() {
        let mut m = Dictation::new(DictationMode::Hold);
        m.on(Input::Press(Some(WIN)));
        let why = "A meeting is recording; dictation can start once it stops.";
        assert_eq!(m.on(refused(&m, why)), vec![Effect::End, failed(why)]);
        assert!(m.on(Input::Release).is_empty());
        // The next press is a fresh dictation.
        assert_eq!(m.on(Input::Press(Some(WIN)))[0], Effect::Start);
        assert_eq!(m.run(), 2);
    }

    #[test]
    fn a_refused_start_reaches_the_user_whichever_answer_comes_first() {
        let why = "A meeting is recording; dictation can start once it stops.";
        // Stop's 404 first.
        let mut m = Dictation::new(DictationMode::Hold);
        m.on(Input::Press(Some(WIN)));
        m.on(Input::Release);
        assert!(m.on(not_active(&m)).is_empty());
        assert_eq!(m.on(refused(&m, why)), vec![Effect::End, failed(why)]);
        // The start's refusal first.
        let mut m = Dictation::new(DictationMode::Hold);
        m.on(Input::Press(Some(WIN)));
        m.on(Input::Release);
        assert!(m.on(refused(&m, why)).is_empty());
        assert_eq!(m.on(not_active(&m)), vec![Effect::End, failed(why)]);
    }

    #[test]
    fn silence_after_listening_says_nothing_was_heard() {
        let mut m = listening(DictationMode::Hold);
        m.on(Input::Release);
        assert_eq!(
            m.on(heard(&m, "  ")),
            vec![Effect::End, failed(NOTHING_HEARD)]
        );
    }

    #[test]
    fn a_failed_transcription_or_a_timeout_reaches_the_user() {
        for why in [
            "Transcription failed.",
            "The dictation did not finish in time.",
        ] {
            let mut m = listening(DictationMode::Hold);
            m.on(Input::Release);
            let effects = m.on(Input::Ended {
                run: m.run(),
                result: Err(EndError::Failed(why.to_string())),
            });
            assert_eq!(effects, vec![Effect::End, failed(why)]);
        }
    }

    #[test]
    fn the_engines_own_stop_collects_the_text() {
        let mut m = listening(DictationMode::Toggle);
        let other = Input::EngineStopped {
            run: m.run(),
            id: "d-other".to_string(),
        };
        assert!(m.on(other).is_empty(), "another dictation's stop");
        let ours = Input::EngineStopped {
            run: m.run(),
            id: "d1".to_string(),
        };
        assert_eq!(
            acts(m.on(ours)),
            vec![
                Effect::Stop(id("d1")),
                Effect::Pill(Pill::Working(TRANSCRIBING))
            ]
        );
        assert_eq!(
            acts(m.on(heard(&m, "five minutes of it"))),
            paste("five minutes of it")
        );
        // The press that would have stopped it is now just ignored.
        assert!(m.on(Input::Press(None)).is_empty());
    }

    #[test]
    fn past_the_cap_with_no_word_from_the_engine_the_overlay_stops_it() {
        let mut m = Dictation::new(DictationMode::Toggle);
        m.on(Input::Press(Some(WIN)));
        let effects = m.on(started(&m, "d1"));
        assert_eq!(waits(&effects), Some(LISTEN_MAX));
        assert_eq!(
            acts(m.on(due(&m))),
            vec![
                Effect::Stop(id("d1")),
                Effect::Pill(Pill::Working(TRANSCRIBING))
            ]
        );
    }

    #[test]
    fn our_own_stop_echoed_back_is_not_a_second_stop() {
        let mut m = listening(DictationMode::Hold);
        m.on(Input::Release);
        let echo = Input::EngineStopped {
            run: m.run(),
            id: "d1".to_string(),
        };
        assert!(m.on(echo).is_empty());
    }

    #[test]
    fn esc_while_listening_cancels_quietly_and_gives_esc_back_at_once() {
        let mut m = listening(DictationMode::Toggle);
        let effects = m.on(Input::Escape);
        assert_eq!(waits(&effects), Some(CANCEL_WAIT));
        assert_eq!(
            acts(effects),
            vec![
                Effect::Cancel(id("d1")),
                Effect::End,
                Effect::Pill(Pill::Hidden)
            ]
        );
        assert_eq!(m.on(heard(&m, "")), vec![Effect::End]);
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn esc_while_transcribing_drops_the_text() {
        let mut m = listening(DictationMode::Hold);
        m.on(Input::Release);
        assert_eq!(
            m.on(Input::Escape),
            vec![Effect::End, Effect::Pill(Pill::Hidden)]
        );
        assert_eq!(m.on(heard(&m, "never mind")), vec![Effect::End]);
    }

    #[test]
    fn esc_before_the_mic_is_ready_cancels_even_when_it_overtakes_the_start() {
        let mut m = Dictation::new(DictationMode::Hold);
        m.on(Input::Press(Some(WIN)));
        assert_eq!(
            acts(m.on(Input::Escape)),
            vec![
                Effect::Cancel(None),
                Effect::End,
                Effect::Pill(Pill::Hidden)
            ]
        );
        assert!(m.on(not_active(&m)).is_empty());
        assert_eq!(m.on(started(&m, "d1")), vec![Effect::Cancel(id("d1"))]);
        assert_eq!(m.on(heard(&m, "")), vec![Effect::End]);
    }

    #[test]
    fn a_cancel_that_fails_says_so() {
        let mut m = listening(DictationMode::Toggle);
        m.on(Input::Escape);
        let effects = m.on(Input::Ended {
            run: m.run(),
            result: Err(EndError::Failed(UNREACHABLE.to_string())),
        });
        assert_eq!(
            effects,
            vec![
                Effect::End,
                failed(&format!("Could not cancel. {UNREACHABLE}"))
            ]
        );
    }

    #[test]
    fn a_hung_start_gives_esc_back_cancels_and_says_so() {
        let mut m = Dictation::new(DictationMode::Toggle);
        m.on(Input::Press(Some(WIN)));
        let effects = m.on(due(&m));
        assert_eq!(
            acts(effects.clone()),
            vec![Effect::Cancel(None), Effect::End, failed(START_TIMED_OUT)]
        );
        assert_eq!(waits(&effects), Some(CANCEL_WAIT));
        // The cancel never answers either: the machine is free again anyway.
        assert_eq!(m.on(due(&m)), vec![Effect::End, failed(CANCEL_UNCONFIRMED)]);
        assert_eq!(m.phase, Phase::Idle);
        assert_eq!(m.on(Input::Press(Some(WIN)))[0], Effect::Start);
    }

    #[test]
    fn a_model_download_keeps_the_start_alive() {
        let mut m = Dictation::new(DictationMode::Hold);
        m.on(Input::Press(Some(WIN)));
        let first = due(&m);
        assert_eq!(
            m.on(Input::Alive { run: m.run() }).len(),
            1,
            "a fresh deadline"
        );
        assert!(
            m.on(first).is_empty(),
            "the earlier deadline no longer counts"
        );
        assert!(matches!(m.phase, Phase::Starting { .. }));
        assert_eq!(
            acts(m.on(started(&m, "d1"))),
            vec![Effect::Pill(Pill::Listening)]
        );
    }

    #[test]
    fn esc_goes_back_while_a_slow_transcription_is_still_awaited() {
        let mut m = listening(DictationMode::Hold);
        let effects = m.on(Input::Release);
        assert_eq!(waits(&effects), Some(ESC_WHILE_TRANSCRIBING));
        let effects = m.on(due(&m));
        assert_eq!(
            acts(effects.clone()),
            vec![Effect::End, Effect::Pill(Pill::Working(TRANSCRIBING))],
            "Esc back, the pill redrawn without its hint"
        );
        assert_eq!(waits(&effects), Some(ENGINE_WAIT));
        // The text can still come, and is pasted.
        assert_eq!(acts(m.on(heard(&m, "slow"))), paste("slow"));
    }

    #[test]
    fn a_stop_that_never_comes_back_ends_the_dictation() {
        let mut m = Dictation::new(DictationMode::Hold);
        m.on(Input::Press(Some(WIN)));
        m.on(Input::Release);
        // Overtook the start, and the start never answers.
        m.on(not_active(&m));
        m.on(due(&m));
        assert_eq!(m.on(due(&m)), vec![Effect::End, failed(UNREACHABLE)]);
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn a_paste_stuck_on_the_clipboard_frees_the_machine_and_reports_late() {
        let mut m = listening(DictationMode::Hold);
        m.on(Input::Release);
        m.on(heard(&m, "words"));
        assert_eq!(m.on(due(&m)), vec![Effect::End, failed(PASTE_STUCK)]);
        assert_eq!(m.phase, Phase::Idle);
        let late = Input::Pasted {
            run: m.run(),
            outcome: Pasted::Landed { restored: true },
        };
        assert_eq!(
            m.on(late),
            vec![Effect::Pill(Pill::Done("Pasted.".to_string()))]
        );
    }

    #[test]
    fn stale_deadlines_change_nothing() {
        let mut m = listening(DictationMode::Hold);
        let old = due(&m);
        m.on(Input::Release);
        assert!(m.on(old).is_empty());
        assert!(matches!(m.phase, Phase::Ending(_)));
    }

    #[test]
    fn answers_from_an_earlier_dictation_are_ignored() {
        let mut m = Dictation::new(DictationMode::Hold);
        m.on(Input::Press(Some(WIN)));
        m.on(refused(&m, "no"));
        m.on(Input::Press(Some(WIN)));
        let stale = Input::Started {
            run: 1,
            result: Ok("d-old".to_string()),
        };
        assert!(m.on(stale).is_empty());
        assert!(matches!(m.phase, Phase::Starting { .. }));
    }

    #[test]
    fn presses_while_a_dictation_finishes_start_nothing() {
        let mut m = listening(DictationMode::Hold);
        m.on(Input::Release);
        assert!(m.on(Input::Press(Some(WIN))).is_empty());
        m.on(heard(&m, "x"));
        assert!(m.on(Input::Press(Some(WIN))).is_empty(), "still pasting");
    }

    #[test]
    fn a_paste_that_could_not_land_says_where_the_text_is() {
        let pill = pasted_pill(&Pasted::OnClipboard(Left::TargetGone), &[]);
        let Pill::Failed(text) = pill else {
            panic!("{pill:?}")
        };
        assert!(text.contains("press Ctrl+V"), "{text}");
        let Pill::Failed(text) = pasted_pill(&Pasted::OnClipboard(Left::NotRead), &[]) else {
            panic!()
        };
        assert!(text.contains("if it isn't there"), "{text}");
    }

    #[test]
    fn an_unconfirmed_paste_says_the_old_clipboard_was_not_put_back() {
        let Pill::Done(text) = pasted_pill(&Pasted::Unconfirmed, &[]) else {
            panic!()
        };
        assert!(text.contains("wasn't put back"), "{text}");
    }

    #[test]
    fn notes_about_the_text_reach_the_pill_after_the_paste() {
        let mut m = listening(DictationMode::Hold);
        m.on(Input::Release);
        let note = "Some words may be missing.".to_string();
        m.on(Input::Ended {
            run: m.run(),
            result: Ok(Heard {
                text: "words".to_string(),
                notes: vec![note],
            }),
        });
        let effects = m.on(Input::Pasted {
            run: m.run(),
            outcome: Pasted::Landed { restored: true },
        });
        assert_eq!(
            effects,
            vec![Effect::Pill(Pill::Done(
                "Pasted. Some words may be missing.".to_string()
            ))]
        );
    }

    #[test]
    fn the_stop_reply_notes_missing_words_and_skipped_polish_plainly() {
        let reply = serde_json::json!({
            "id": "d1", "text": "hello", "duration_ms": 900, "polished": false,
            "incomplete": true, "note": "one chunk failed to transcribe",
            "polish_note": "the LLM took longer than 8s",
        });
        assert_eq!(
            super::heard(&reply),
            Heard {
                text: "hello".to_string(),
                notes: vec![
                    "Some words may be missing. One chunk failed to transcribe.".to_string(),
                    "Not polished: the LLM took longer than 8s.".to_string(),
                ],
            }
        );
        let bare = serde_json::json!({"text": "hi", "incomplete": true});
        assert_eq!(
            super::heard(&bare).notes,
            vec!["Some words may be missing.".to_string()]
        );
        let clean = serde_json::json!({"text": "hi", "incomplete": false, "polished": true});
        assert!(super::heard(&clean).notes.is_empty());
    }

    #[test]
    fn an_unset_provider_is_the_local_default() {
        let providers = serde_json::json!({"providers": [
            {"id": "whisper-local", "kind": "local", "ready": true, "detail": "model small.en present"},
            {"id": "deepgram", "kind": "cloud-streaming", "ready": true, "default": true},
        ]});
        let info = provider_info(&serde_json::json!({}), &providers);
        assert_eq!(info["id"], "whisper-local");
        assert_eq!(info["local"], true);
        assert_eq!(info["is_default"], true);
        assert_eq!(info["ready"], true);
    }

    #[test]
    fn a_cloud_dictation_provider_is_reported_as_not_local() {
        let providers = serde_json::json!({"providers": [
            {"id": "groq-whisper", "kind": "cloud-batch", "ready": false, "detail": "env var GROQ_API_KEY not set"},
        ]});
        let settings = serde_json::json!({"dictation_provider": "groq-whisper"});
        let info = provider_info(&settings, &providers);
        assert_eq!(info["local"], false);
        assert_eq!(info["is_default"], false);
        assert_eq!(info["ready"], false);
        assert_eq!(info["detail"], "env var GROQ_API_KEY not set");
        let unknown = provider_info(
            &serde_json::json!({"dictation_provider": "nope"}),
            &providers,
        );
        assert_eq!(unknown["local"], false);
        assert_eq!(unknown["ready"], false);
    }

    #[test]
    fn engine_messages_read_as_sentences() {
        assert_eq!(
            sentence("a dictation is already active"),
            "A dictation is already active."
        );
        assert_eq!(sentence("Done!"), "Done!");
        assert_eq!(sentence(""), "");
    }

    /// An engine that answers one request with `status` and `body`, and
    /// hands back the request line and body it got.
    fn answer_once(
        status: u16,
        body: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<(String, String)>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let mut length = 0;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                    break;
                }
                if let Some(n) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = n.trim().parse().unwrap_or(0);
                }
            }
            let mut sent = vec![0u8; length];
            let _ = reader.read_exact(&mut sent);
            let _ = tx.send((
                line.trim_end().to_string(),
                String::from_utf8_lossy(&sent).into_owned(),
            ));
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            let mut s = s;
            let _ = s.write_all(response.as_bytes());
        });
        (base, rx)
    }

    #[tokio::test]
    async fn start_returns_the_new_dictations_id() {
        let (base, seen) = answer_once(
            201,
            r#"{"id":"d19","provider":"whisper-local","local":true}"#,
        );
        let http = reqwest::Client::new();
        assert_eq!(start(&http, &base).await, Ok("d19".to_string()));
        assert!(seen
            .recv()
            .unwrap()
            .0
            .starts_with("POST /api/v1/dictation/start"));
    }

    #[tokio::test]
    async fn a_refused_start_carries_the_engines_message() {
        let (base, _) = answer_once(
            409,
            r#"{"error":"a meeting is recording; dictation can start once it stops"}"#,
        );
        let http = reqwest::Client::new();
        assert_eq!(
            start(&http, &base).await,
            Err("A meeting is recording; dictation can start once it stops.".to_string())
        );
    }

    #[tokio::test]
    async fn an_engine_without_dictation_says_to_update() {
        let (base, _) = answer_once(404, "");
        let http = reqwest::Client::new();
        let err = start(&http, &base).await.unwrap_err();
        assert!(err.contains("Update"), "{err}");
    }

    #[tokio::test]
    async fn no_engine_at_all_is_said_plainly() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let http = reqwest::Client::new();
        assert_eq!(start(&http, &base).await, Err(UNREACHABLE.to_string()));
        assert_eq!(
            stop(&http, &base, None).await,
            Err(EndError::Failed(UNREACHABLE.to_string()))
        );
    }

    #[tokio::test]
    async fn stop_returns_the_text_even_when_it_is_empty() {
        let (base, seen) = answer_once(200, r#"{"id":"d1","text":"","duration_ms":40}"#);
        let http = reqwest::Client::new();
        assert_eq!(stop(&http, &base, None).await, Ok(Heard::default()));
        let (line, body) = seen.recv().unwrap();
        assert!(line.starts_with("POST /api/v1/dictation/stop"));
        assert_eq!(body, "", "no id known, no body");
    }

    #[tokio::test]
    async fn stop_and_cancel_name_the_dictation_when_they_know_it() {
        let http = reqwest::Client::new();
        let (base, seen) = answer_once(200, r#"{"id":"d7","text":"hi","duration_ms":40}"#);
        stop(&http, &base, Some("d7")).await.unwrap();
        let body: serde_json::Value = serde_json::from_str(&seen.recv().unwrap().1).unwrap();
        assert_eq!(body, serde_json::json!({"id": "d7"}));
        let (base, seen) = answer_once(200, r#"{"id":"d7"}"#);
        cancel(&http, &base, Some("d7")).await.unwrap();
        let (line, body) = seen.recv().unwrap();
        assert!(line.starts_with("POST /api/v1/dictation/cancel"));
        assert_eq!(body, r#"{"id":"d7"}"#);
    }

    #[tokio::test]
    async fn stop_tells_nothing_active_from_a_failure() {
        let http = reqwest::Client::new();
        let (base, _) = answer_once(404, r#"{"error":"no dictation is active"}"#);
        assert_eq!(stop(&http, &base, None).await, Err(EndError::NotActive));
        let (base, _) = answer_once(502, r#"{"error":"whisper crashed"}"#);
        assert_eq!(
            stop(&http, &base, None).await,
            Err(EndError::Failed("Whisper crashed.".to_string()))
        );
        let (base, _) = answer_once(504, r#"{"error":"the dictation did not finish in time"}"#);
        assert_eq!(
            stop(&http, &base, None).await,
            Err(EndError::Failed(
                "The dictation did not finish in time.".to_string()
            ))
        );
    }

    #[tokio::test]
    async fn cancel_hits_the_cancel_route() {
        let (base, seen) = answer_once(200, r#"{"id":"d1"}"#);
        let http = reqwest::Client::new();
        assert_eq!(cancel(&http, &base, None).await, Ok(Heard::default()));
        assert!(seen
            .recv()
            .unwrap()
            .0
            .starts_with("POST /api/v1/dictation/cancel"));
    }
}
