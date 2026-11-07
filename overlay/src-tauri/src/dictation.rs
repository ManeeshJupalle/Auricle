//! Dictation: the hotkey's state machine and the engine calls behind it.
//!
//! Every hotkey event and engine answer is an [`Input`] to [`Dictation::on`],
//! a pure reducer that answers with the [`Effect`]s to carry out, so the
//! whole lifecycle is testable without Tauri, an engine, or a desktop.
//! [`run`] is the thin loop that carries them out.
//!
//! The engine's stop and cancel name no dictation: they act on whichever is
//! active. A stop sent while the start is still out can therefore overtake
//! it and find nothing, so a 404 then means "not yet", and the stop goes out
//! again once the start has answered.
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

const UNREACHABLE: &str = "The Auricle engine isn't answering.";
pub const STARTING: &str = "Starting…";
pub const TRANSCRIBING: &str = "Transcribing…";
pub const PASTING: &str = "Pasting…";
const NOTHING_HEARD: &str = "Nothing was heard.";
const ALREADY_ENDED: &str = "The dictation had already ended.";

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
        result: Result<String, EndError>,
    },
    /// The engine ended a dictation without being asked: its 5-minute cap.
    EngineStopped {
        run: u64,
        id: String,
    },
    Pasted {
        run: u64,
        outcome: Pasted,
    },
}

impl Input {
    fn run(&self) -> Option<u64> {
        match self {
            Input::Started { run, .. }
            | Input::Ended { run, .. }
            | Input::EngineStopped { run, .. }
            | Input::Pasted { run, .. } => Some(*run),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EndError {
    /// 404: no dictation was active.
    NotActive,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// A dictation is beginning: take Esc, and watch the engine's events.
    Begin,
    Start,
    Stop,
    Cancel,
    Paste(String, Option<Target>),
    Pill(Pill),
    /// It is over: give Esc back and stop watching.
    End,
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
    Pasting,
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
    /// Sent before the start answered, so a 404 may only mean it overtook it.
    early: bool,
    /// It came back 404 while the start was still out.
    missed: bool,
    resent: bool,
    /// Esc while transcribing: the text is no longer wanted.
    discard: bool,
}

pub struct Dictation {
    mode: DictationMode,
    phase: Phase,
    /// Numbers each dictation, so answers from an older one are told apart.
    run: u64,
}

impl Dictation {
    pub fn new(mode: DictationMode) -> Self {
        Dictation {
            mode,
            phase: Phase::Idle,
            run: 0,
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

    fn step(&mut self, phase: Phase, input: Input) -> (Phase, Vec<Effect>) {
        let toggle = self.mode == DictationMode::Toggle;
        match (phase, input) {
            (Phase::Idle, Input::Press(target)) => {
                self.run += 1;
                // The start first: every moment before the mic opens loses words.
                (
                    Phase::Starting { target },
                    vec![
                        Effect::Start,
                        Effect::Begin,
                        Effect::Pill(Pill::Working(STARTING)),
                    ],
                )
            }
            (Phase::Starting { target }, Input::Release) if !toggle => {
                ending(false, target, Start::Pending)
            }
            (Phase::Starting { target }, Input::Press(_)) if toggle => {
                ending(false, target, Start::Pending)
            }
            (Phase::Starting { target }, Input::Escape) => ending(true, target, Start::Pending),
            (Phase::Starting { target }, Input::Started { result, .. }) => match result {
                Ok(id) => (
                    Phase::Listening { id, target },
                    vec![Effect::Pill(Pill::Listening)],
                ),
                Err(why) => (
                    Phase::Idle,
                    vec![Effect::End, Effect::Pill(Pill::Failed(why))],
                ),
            },
            (Phase::Listening { target, .. }, Input::Release) if !toggle => {
                ending(false, target, Start::Listened)
            }
            (Phase::Listening { target, .. }, Input::Press(_)) if toggle => {
                ending(false, target, Start::Listened)
            }
            // The cap stopped it; its text waits for a stop to collect.
            (Phase::Listening { id, target }, Input::EngineStopped { id: stopped, .. })
                if stopped == id =>
            {
                ending(false, target, Start::Listened)
            }
            (Phase::Listening { target, .. }, Input::Escape) => {
                ending(true, target, Start::Listened)
            }
            (Phase::Ending(e), input) => self.ending(e, input),
            (Phase::Pasting, Input::Pasted { outcome, .. }) => {
                (Phase::Idle, vec![Effect::Pill(pasted_pill(&outcome))])
            }
            (phase, _) => (phase, Vec::new()),
        }
    }

    fn ending(&self, mut e: Ending, input: Input) -> (Phase, Vec<Effect>) {
        let again = if e.cancel {
            Effect::Cancel
        } else {
            Effect::Stop
        };
        match input {
            Input::Escape if !e.cancel && !e.discard => {
                e.discard = true;
                (Phase::Ending(e), vec![Effect::Pill(Pill::Hidden)])
            }
            Input::Started { result, .. } if e.start == Start::Pending => match result {
                Ok(_) if e.missed => {
                    e.start = Start::Listened;
                    e.missed = false;
                    e.resent = true;
                    (Phase::Ending(e), vec![again])
                }
                Ok(_) => {
                    e.start = Start::Listened;
                    (Phase::Ending(e), Vec::new())
                }
                Err(why) if e.missed => over(&e, why),
                Err(why) => {
                    e.start = Start::Refused(why);
                    (Phase::Ending(e), Vec::new())
                }
            },
            Input::Ended { result, .. } => match result {
                Ok(_) if e.cancel || e.discard => (Phase::Idle, vec![Effect::End]),
                Ok(text) if text.trim().is_empty() => {
                    let why = if e.start == Start::Listened {
                        NOTHING_HEARD
                    } else {
                        self.too_soon()
                    };
                    (
                        Phase::Idle,
                        vec![Effect::End, Effect::Pill(Pill::Failed(why.to_string()))],
                    )
                }
                Ok(text) => (
                    Phase::Pasting,
                    vec![
                        Effect::End,
                        Effect::Pill(Pill::Working(PASTING)),
                        Effect::Paste(text, e.target),
                    ],
                ),
                Err(EndError::NotActive) => match &e.start {
                    Start::Pending => {
                        e.missed = true;
                        (Phase::Ending(e), Vec::new())
                    }
                    Start::Listened if e.early && !e.resent => {
                        e.resent = true;
                        (Phase::Ending(e), vec![again])
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

fn ending(cancel: bool, target: Option<Target>, start: Start) -> (Phase, Vec<Effect>) {
    let early = start == Start::Pending;
    let e = Ending {
        cancel,
        target,
        start,
        early,
        missed: false,
        resent: false,
        discard: false,
    };
    let effects = if cancel {
        vec![Effect::Cancel, Effect::Pill(Pill::Hidden)]
    } else {
        vec![Effect::Stop, Effect::Pill(Pill::Working(TRANSCRIBING))]
    };
    (Phase::Ending(e), effects)
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

fn pasted_pill(outcome: &Pasted) -> Pill {
    match outcome {
        Pasted::Landed { restored: true } => Pill::Done("Pasted.".to_string()),
        Pasted::Landed { restored: false } => {
            Pill::Done("Pasted. Your earlier clipboard could not be put back.".to_string())
        }
        Pasted::OnClipboard(left) => Pill::Failed(format!(
            "{} The text is on the clipboard: press Ctrl+V.",
            left.reason()
        )),
        Pasted::Lost(e) => Pill::Failed(format!(
            "The text could not be pasted or put on the clipboard. {e}"
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
/// No timeout: a first-run model download can take minutes, and a release or
/// Esc ends the wait through the engine instead.
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
pub async fn stop(http: &reqwest::Client, base: &str) -> Result<String, EndError> {
    let resp = end(http, base, "stop").await?;
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    Ok(body["text"].as_str().unwrap_or_default().to_string())
}

/// POST /dictation/cancel: everything heard is thrown away.
pub async fn cancel(http: &reqwest::Client, base: &str) -> Result<String, EndError> {
    end(http, base, "cancel").await.map(|_| String::new())
}

async fn end(
    http: &reqwest::Client,
    base: &str,
    action: &str,
) -> Result<reqwest::Response, EndError> {
    let resp = http
        .post(format!("{base}/api/v1/dictation/{action}"))
        .timeout(ENGINE_WAIT)
        .send()
        .await
        .map_err(|_| EndError::Failed(UNREACHABLE.to_string()))?;
    match resp.status().as_u16() {
        200 => Ok(resp),
        404 => Err(EndError::NotActive),
        _ => Err(EndError::Failed(reason(resp).await)),
    }
}

/// Carry out the machine's effects for the life of the overlay. `feed` is
/// how the engine's answers come back in as inputs.
pub async fn run(
    app: tauri::AppHandle,
    mode: DictationMode,
    http: reqwest::Client,
    base: String,
    esc: Shortcut,
    mut inputs: UnboundedReceiver<Input>,
    feed: UnboundedSender<Input>,
) {
    let mut machine = Dictation::new(mode);
    let mut watch: Option<AbortHandle> = None;
    while let Some(input) = inputs.recv().await {
        for effect in machine.on(input) {
            let run = machine.run();
            match effect {
                Effect::Begin => {
                    // Esc is only taken while a dictation needs it: held
                    // globally, it would be swallowed from every app.
                    set_esc(&app, esc, true).await;
                    let (handle, registration) = AbortHandle::new_pair();
                    watch = Some(handle);
                    let (http, base, feed) = (http.clone(), base.clone(), feed.clone());
                    tauri::async_runtime::spawn(async move {
                        let _ = crate::ws::listen(&http, &base, registration, |event| {
                            if event["type"] == "dictation_stopped" {
                                if let Some(id) = event["id"].as_str() {
                                    let id = id.to_string();
                                    let _ = feed.send(Input::EngineStopped { run, id });
                                }
                            }
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
                Effect::Stop | Effect::Cancel => {
                    let cancelling = effect == Effect::Cancel;
                    let (http, base, feed) = (http.clone(), base.clone(), feed.clone());
                    tauri::async_runtime::spawn(async move {
                        let result = if cancelling {
                            cancel(&http, &base).await
                        } else {
                            stop(&http, &base).await
                        };
                        let _ = feed.send(Input::Ended { run, result });
                    });
                }
                Effect::Paste(text, target) => {
                    let feed = feed.clone();
                    tauri::async_runtime::spawn_blocking(move || {
                        let outcome = dictate::paste_text(&text, target);
                        let _ = feed.send(Input::Pasted { run, outcome });
                    });
                }
                Effect::Pill(pill) => show_pill(&app, pill),
                Effect::End => {
                    set_esc(&app, esc, false).await;
                    if let Some(handle) = watch.take() {
                        handle.abort();
                    }
                }
            }
        }
    }
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

fn show_pill(app: &tauri::AppHandle, pill: Pill) {
    if let Pill::Failed(why) = pill {
        crate::notify(app, why);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictate::Left;

    const WIN: Target = Target { hwnd: 42, pid: 7 };

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
            result: Ok(text.to_string()),
        }
    }

    fn not_active(m: &Dictation) -> Input {
        Input::Ended {
            run: m.run(),
            result: Err(EndError::NotActive),
        }
    }

    fn failed(text: &str) -> Effect {
        Effect::Pill(Pill::Failed(text.to_string()))
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
        assert_eq!(
            m.on(Input::Press(Some(WIN))),
            vec![
                Effect::Start,
                Effect::Begin,
                Effect::Pill(Pill::Working(STARTING))
            ]
        );
        assert_eq!(m.on(started(&m, "d1")), vec![Effect::Pill(Pill::Listening)]);
        assert_eq!(
            m.on(Input::Release),
            vec![Effect::Stop, Effect::Pill(Pill::Working(TRANSCRIBING))]
        );
        assert_eq!(m.on(heard(&m, "hello there")), paste("hello there"));
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
            m.on(Input::Press(None)),
            vec![Effect::Stop, Effect::Pill(Pill::Working(TRANSCRIBING))]
        );
        assert_eq!(m.on(heard(&m, "ok")), paste("ok"));
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
            m.on(Input::Release),
            vec![Effect::Stop, Effect::Pill(Pill::Working(TRANSCRIBING))]
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
    fn a_stop_that_overtook_the_start_goes_again_once_the_start_answers() {
        let mut m = Dictation::new(DictationMode::Hold);
        m.on(Input::Press(Some(WIN)));
        m.on(Input::Release);
        assert!(m.on(not_active(&m)).is_empty(), "wait for the start");
        assert_eq!(m.on(started(&m, "d1")), vec![Effect::Stop]);
        assert_eq!(m.on(heard(&m, "late words")), paste("late words"));
    }

    #[test]
    fn the_overtaken_stop_can_also_answer_after_the_start() {
        let mut m = Dictation::new(DictationMode::Hold);
        m.on(Input::Press(Some(WIN)));
        m.on(Input::Release);
        assert!(m.on(started(&m, "d1")).is_empty());
        assert_eq!(m.on(not_active(&m)), vec![Effect::Stop]);
        // Only once: a second 404 is real.
        assert_eq!(
            m.on(not_active(&m)),
            vec![Effect::End, failed(ALREADY_ENDED)]
        );
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
            m.on(ours),
            vec![Effect::Stop, Effect::Pill(Pill::Working(TRANSCRIBING))]
        );
        assert_eq!(
            m.on(heard(&m, "five minutes of it")),
            paste("five minutes of it")
        );
        // The press that would have stopped it is now just ignored.
        assert!(m.on(Input::Press(None)).is_empty());
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
    fn esc_while_listening_cancels_quietly() {
        let mut m = listening(DictationMode::Toggle);
        assert_eq!(
            m.on(Input::Escape),
            vec![Effect::Cancel, Effect::Pill(Pill::Hidden)]
        );
        assert_eq!(m.on(heard(&m, "")), vec![Effect::End]);
        assert_eq!(m.phase, Phase::Idle);
    }

    #[test]
    fn esc_while_transcribing_drops_the_text() {
        let mut m = listening(DictationMode::Hold);
        m.on(Input::Release);
        assert_eq!(m.on(Input::Escape), vec![Effect::Pill(Pill::Hidden)]);
        assert_eq!(m.on(heard(&m, "never mind")), vec![Effect::End]);
    }

    #[test]
    fn esc_before_the_mic_is_ready_cancels_even_when_it_overtakes_the_start() {
        let mut m = Dictation::new(DictationMode::Hold);
        m.on(Input::Press(Some(WIN)));
        assert_eq!(
            m.on(Input::Escape),
            vec![Effect::Cancel, Effect::Pill(Pill::Hidden)]
        );
        assert!(m.on(not_active(&m)).is_empty());
        assert_eq!(m.on(started(&m, "d1")), vec![Effect::Cancel]);
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
        let pill = pasted_pill(&Pasted::OnClipboard(Left::TargetGone));
        let Pill::Failed(text) = pill else {
            panic!("{pill:?}")
        };
        assert!(text.contains("press Ctrl+V"), "{text}");
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
    /// says which request it was.
    fn answer_once(status: u16, body: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            let _ = tx.send(head.lines().next().unwrap_or_default().to_string());
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
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
            stop(&http, &base).await,
            Err(EndError::Failed(UNREACHABLE.to_string()))
        );
    }

    #[tokio::test]
    async fn stop_returns_the_text_even_when_it_is_empty() {
        let (base, seen) = answer_once(200, r#"{"id":"d1","text":"","duration_ms":40}"#);
        let http = reqwest::Client::new();
        assert_eq!(stop(&http, &base).await, Ok(String::new()));
        assert!(seen
            .recv()
            .unwrap()
            .starts_with("POST /api/v1/dictation/stop"));
    }

    #[tokio::test]
    async fn stop_tells_nothing_active_from_a_failure() {
        let http = reqwest::Client::new();
        let (base, _) = answer_once(404, r#"{"error":"no dictation is active"}"#);
        assert_eq!(stop(&http, &base).await, Err(EndError::NotActive));
        let (base, _) = answer_once(502, r#"{"error":"whisper crashed"}"#);
        assert_eq!(
            stop(&http, &base).await,
            Err(EndError::Failed("Whisper crashed.".to_string()))
        );
        let (base, _) = answer_once(504, r#"{"error":"the dictation did not finish in time"}"#);
        assert_eq!(
            stop(&http, &base).await,
            Err(EndError::Failed(
                "The dictation did not finish in time.".to_string()
            ))
        );
    }

    #[tokio::test]
    async fn cancel_hits_the_cancel_route() {
        let (base, seen) = answer_once(200, r#"{"id":"d1"}"#);
        let http = reqwest::Client::new();
        assert_eq!(cancel(&http, &base).await, Ok(String::new()));
        assert!(seen
            .recv()
            .unwrap()
            .starts_with("POST /api/v1/dictation/cancel"));
    }
}
