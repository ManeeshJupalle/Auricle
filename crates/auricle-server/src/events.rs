//! WebSocket event vocabulary (architecture §4.8): partial/final transcript
//! events plus lifecycle and error events, serialized as tagged JSON.

use serde::Serialize;

// PHASE-6: `provider_changed` joins the vocabulary when the API exposes
// runtime provider swap.

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsEvent {
    Partial {
        session: String,
        channel: String,
        speaker: String,
        t_start_ms: u64,
        t_end_ms: u64,
        text: String,
    },
    Final {
        session: String,
        channel: String,
        speaker: String,
        t_start_ms: u64,
        t_end_ms: u64,
        text: String,
        /// Milliseconds between the audio's end and this event's emission
        /// (chunk→final latency as seen by the engine).
        latency_ms: u64,
    },
    /// The partial on `channel` will get no final, because the assembler
    /// dropped it as a duplicate: clients should remove it (important lane).
    PartialCleared {
        session: String,
        channel: String,
    },
    /// Per-channel RMS level, ~10 Hz while capturing (lossy lane).
    Vu {
        session: String,
        channel: String,
        rms: f32,
    },
    /// A session start is blocked on a first-run model download (whisper
    /// models are 148–500 MB; the start request completes when it's done).
    /// Without this the UI's Start button just hangs silently for minutes.
    ModelDownloadStarted {
        session: String,
        model: String,
        size_mb: u64,
    },
    /// How far that download has got, about once a second while it grows, so
    /// someone waiting to record can tell ten seconds from five minutes.
    /// `done_mb` reaching `size_mb` means the file is being verified.
    ModelDownloadProgress {
        session: String,
        model: String,
        done_mb: u64,
        size_mb: u64,
    },
    SessionStarted {
        session: String,
        title: String,
        stt_provider: String,
    },
    SessionStopped {
        session: String,
    },
    /// Session metadata changed after the fact (currently: the auto-title
    /// generated from the transcript once a session stops).
    SessionUpdated {
        session: String,
        title: String,
    },
    /// One streamed fragment of an ask's answer (mirrors the ask's SSE
    /// response so the overlay can ride the socket it already holds).
    AnswerDelta {
        ask_id: String,
        text: String,
    },
    /// An ask's answer completed; `usage` when the provider reported it.
    AnswerDone {
        ask_id: String,
        usage: Option<auricle_llm::TokenUsage>,
    },
    /// An ask failed (upstream LLM error, unusable context, …).
    AskError {
        ask_id: String,
        message: String,
    },
    /// A dictation has the mic.
    DictationStarted {
        id: String,
        provider: String,
    },
    /// A final of the dictation landed (lossy lane). Every `/ws/live`
    /// subscriber gets this, the dashboard included, so it carries no text.
    DictationPartial {
        id: String,
    },
    /// The dictation is over, however it ended. Its text goes only to the
    /// stop's reply, and to the LLM first when polish is on.
    DictationStopped {
        id: String,
    },
    DeviceLost {
        session: String,
        channel: String,
        message: String,
    },
    Error {
        session: String,
        message: String,
    },
    /// Emitted to a slow consumer after partials or answer fragments were
    /// dropped for it. Both counts are running totals for the connection.
    Lag {
        dropped_partials: u64,
        dropped_answer_deltas: u64,
    },
}

impl WsEvent {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("WsEvent serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_and_final_match_the_architecture_shape() {
        let ev = WsEvent::Partial {
            session: "s1".into(),
            channel: "loopback".into(),
            speaker: "Them".into(),
            t_start_ms: 1000,
            t_end_ms: 2000,
            text: "hello wor".into(),
        };
        let v: serde_json::Value = serde_json::from_str(&ev.to_json()).unwrap();
        assert_eq!(v["type"], "partial");
        assert_eq!(v["session"], "s1");
        assert_eq!(v["speaker"], "Them");
        assert_eq!(v["t_start_ms"], 1000);
        assert_eq!(v["text"], "hello wor");

        let ev = WsEvent::Final {
            session: "s1".into(),
            channel: "mic".into(),
            speaker: "You".into(),
            t_start_ms: 0,
            t_end_ms: 1500,
            text: "done".into(),
            latency_ms: 640,
        };
        let v: serde_json::Value = serde_json::from_str(&ev.to_json()).unwrap();
        assert_eq!(v["type"], "final");
        assert_eq!(v["channel"], "mic");
        assert_eq!(v["latency_ms"], 640);
    }

    #[test]
    fn vu_event_serializes() {
        let v: serde_json::Value = serde_json::from_str(
            &WsEvent::Vu {
                session: "s1".into(),
                channel: "loopback".into(),
                rms: 0.125,
            }
            .to_json(),
        )
        .unwrap();
        assert_eq!(v["type"], "vu");
        assert_eq!(v["channel"], "loopback");
        assert!((v["rms"].as_f64().unwrap() - 0.125).abs() < 1e-6);
    }

    #[test]
    fn model_download_event_serializes() {
        let v: serde_json::Value = serde_json::from_str(
            &WsEvent::ModelDownloadStarted {
                session: "s1".into(),
                model: "base.en".into(),
                size_mb: 141,
            }
            .to_json(),
        )
        .unwrap();
        assert_eq!(v["type"], "model_download_started");
        assert_eq!(v["model"], "base.en");
        assert_eq!(v["size_mb"], 141);
    }

    #[test]
    fn ask_events_serialize() {
        let v: serde_json::Value = serde_json::from_str(
            &WsEvent::AnswerDelta {
                ask_id: "a1".into(),
                text: "The chunker".into(),
            }
            .to_json(),
        )
        .unwrap();
        assert_eq!(v["type"], "answer_delta");
        assert_eq!(v["ask_id"], "a1");
        assert_eq!(v["text"], "The chunker");

        let v: serde_json::Value = serde_json::from_str(
            &WsEvent::AnswerDone {
                ask_id: "a1".into(),
                usage: Some(auricle_llm::TokenUsage {
                    prompt_tokens: 54,
                    completion_tokens: 21,
                    total_tokens: 75,
                }),
            }
            .to_json(),
        )
        .unwrap();
        assert_eq!(v["type"], "answer_done");
        assert_eq!(v["usage"]["total_tokens"], 75);

        let v: serde_json::Value = serde_json::from_str(
            &WsEvent::AskError {
                ask_id: "a1".into(),
                message: "llm ollama returned HTTP 404".into(),
            }
            .to_json(),
        )
        .unwrap();
        assert_eq!(v["type"], "ask_error");
        assert!(v["message"].as_str().unwrap().contains("404"));
    }

    #[test]
    fn dictation_events_serialize() {
        let json = |ev: WsEvent| serde_json::from_str::<serde_json::Value>(&ev.to_json()).unwrap();
        assert_eq!(
            json(WsEvent::DictationStarted {
                id: "d1".into(),
                provider: "whisper-local".into(),
            }),
            serde_json::json!({"type": "dictation_started", "id": "d1", "provider": "whisper-local"})
        );
        assert_eq!(
            json(WsEvent::DictationPartial { id: "d1".into() }),
            serde_json::json!({"type": "dictation_partial", "id": "d1"})
        );
        assert_eq!(
            json(WsEvent::DictationStopped { id: "d1".into() }),
            serde_json::json!({"type": "dictation_stopped", "id": "d1"})
        );
    }

    #[test]
    fn lifecycle_and_error_events_serialize() {
        let v: serde_json::Value = serde_json::from_str(
            &WsEvent::SessionStarted {
                session: "s1".into(),
                title: "standup".into(),
                stt_provider: "deepgram".into(),
            }
            .to_json(),
        )
        .unwrap();
        assert_eq!(v["type"], "session_started");
        assert_eq!(v["stt_provider"], "deepgram");

        let v: serde_json::Value = serde_json::from_str(
            &WsEvent::SessionStopped {
                session: "s1".into(),
            }
            .to_json(),
        )
        .unwrap();
        assert_eq!(v["type"], "session_stopped");

        let v: serde_json::Value = serde_json::from_str(
            &WsEvent::DeviceLost {
                session: "s1".into(),
                channel: "mic".into(),
                message: "device invalidated".into(),
            }
            .to_json(),
        )
        .unwrap();
        assert_eq!(v["type"], "device_lost");

        let v: serde_json::Value = serde_json::from_str(
            &WsEvent::Lag {
                dropped_partials: 7,
                dropped_answer_deltas: 3,
            }
            .to_json(),
        )
        .unwrap();
        assert_eq!(v["type"], "lag");
        assert_eq!(v["dropped_partials"], 7);
        assert_eq!(v["dropped_answer_deltas"], 3);
    }
}
