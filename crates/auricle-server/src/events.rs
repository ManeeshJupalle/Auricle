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
    /// Per-channel RMS level, ~10 Hz while capturing (lossy lane).
    Vu {
        session: String,
        channel: String,
        rms: f32,
    },
    SessionStarted {
        session: String,
        title: String,
        stt_provider: String,
    },
    SessionStopped {
        session: String,
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
    /// Emitted to a slow consumer after partials were dropped for it.
    Lag {
        dropped_partials: u64,
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
            }
            .to_json(),
        )
        .unwrap();
        assert_eq!(v["type"], "lag");
        assert_eq!(v["dropped_partials"], 7);
    }
}
