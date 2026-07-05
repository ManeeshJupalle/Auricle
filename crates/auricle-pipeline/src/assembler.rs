use auricle_core::{ChannelId, SttEvent};
use tokio::sync::broadcast;

/// A transcript line pushed to clients.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptEvent {
    pub session_id: String,
    pub channel: ChannelId,
    pub speaker: String,
    pub is_final: bool,
    pub t_start_ms: u64,
    pub t_end_ms: u64,
    pub text: String,
}

/// Everything broadcast by the assembler.
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    Transcript(TranscriptEvent),
    SttError { channel: ChannelId, message: String },
}

/// Merges per-channel STT events into one transcript.
///
/// - Speaker tags: mic -> "You", loopback -> "Them".
/// - Finals are deduped against the previous final on the same channel when
///   their time ranges overlap (the chunker's 0.5 s overlap): the longest
///   word-level suffix of the previous text matching a prefix of the new
///   text is trimmed from the new text.
/// - Partials are broadcast but never stored; stale partials (older than the
///   channel's last final) are suppressed.
/// - The merged transcript is kept ordered by `t_start_ms` across channels
///   (channels start, stop, and gap independently, so events arrive
///   out of order relative to each other).
pub struct Assembler {
    session_id: String,
    tx: broadcast::Sender<PipelineEvent>,
    finals: Vec<TranscriptEvent>,
    last_final_end: [(ChannelId, u64, String); 2],
}

impl Assembler {
    pub fn new(session_id: &str) -> Self {
        let (tx, _) = broadcast::channel(256);
        Assembler {
            session_id: session_id.to_string(),
            tx,
            finals: Vec::new(),
            last_final_end: [
                (ChannelId::Mic, 0, String::new()),
                (ChannelId::Loopback, 0, String::new()),
            ],
        }
    }

    /// Subscribe to live transcript events.
    // PHASE-4: the WebSocket handler subscribes here.
    pub fn subscribe(&self) -> broadcast::Receiver<PipelineEvent> {
        self.tx.subscribe()
    }

    pub fn speaker_for(channel: ChannelId) -> &'static str {
        match channel {
            ChannelId::Mic => "You",
            ChannelId::Loopback => "Them",
        }
    }

    /// Ingest one STT event; returns the event that was broadcast (if any).
    pub fn ingest(&mut self, event: SttEvent) -> Option<PipelineEvent> {
        let out = match event {
            SttEvent::Error(message) => {
                // Surface on whichever channel errored; the SttEvent::Error
                // carries no channel, so tag it unknown-side as mic-agnostic.
                // PHASE-3: cloud providers attach channel context here.
                Some(PipelineEvent::SttError {
                    channel: ChannelId::Mic,
                    message,
                })
            }
            SttEvent::Partial(seg) => {
                let state = self.channel_state(seg.channel);
                if seg.t_end_ms <= state.1 {
                    None // stale: a final already covers this audio
                } else {
                    Some(PipelineEvent::Transcript(TranscriptEvent {
                        session_id: self.session_id.clone(),
                        channel: seg.channel,
                        speaker: Self::speaker_for(seg.channel).to_string(),
                        is_final: false,
                        t_start_ms: seg.t_start_ms,
                        t_end_ms: seg.t_end_ms,
                        text: seg.text,
                    }))
                }
            }
            SttEvent::Final(seg) => {
                let state = self.channel_state(seg.channel);
                let overlaps = seg.t_start_ms < state.1;
                let text = if overlaps {
                    trim_overlap(&state.2, &seg.text)
                } else {
                    seg.text.clone()
                };
                let state = self.channel_state_mut(seg.channel);
                state.1 = seg.t_end_ms.max(state.1);
                state.2 = seg.text;
                if text.is_empty() {
                    None // entirely duplicated by the previous final
                } else {
                    let ev = TranscriptEvent {
                        session_id: self.session_id.clone(),
                        channel: seg.channel,
                        speaker: Self::speaker_for(seg.channel).to_string(),
                        is_final: true,
                        t_start_ms: seg.t_start_ms,
                        t_end_ms: seg.t_end_ms,
                        text,
                    };
                    let at = self
                        .finals
                        .partition_point(|f| f.t_start_ms <= ev.t_start_ms);
                    self.finals.insert(at, ev.clone());
                    Some(PipelineEvent::Transcript(ev))
                }
            }
        };
        if let Some(ev) = &out {
            let _ = self.tx.send(ev.clone()); // no subscribers is fine
        }
        out
    }

    /// The merged transcript so far, ordered by start time across channels.
    pub fn transcript(&self) -> &[TranscriptEvent] {
        &self.finals
    }

    fn channel_state(&self, channel: ChannelId) -> &(ChannelId, u64, String) {
        self.last_final_end
            .iter()
            .find(|s| s.0 == channel)
            .expect("both channels initialized")
    }

    fn channel_state_mut(&mut self, channel: ChannelId) -> &mut (ChannelId, u64, String) {
        self.last_final_end
            .iter_mut()
            .find(|s| s.0 == channel)
            .expect("both channels initialized")
    }
}

/// Remove from `next` the longest word-level prefix that matches a suffix of
/// `prev` (case- and punctuation-insensitive), returning the trimmed text.
fn trim_overlap(prev: &str, next: &str) -> String {
    let prev_words: Vec<&str> = prev.split_whitespace().collect();
    let next_words: Vec<&str> = next.split_whitespace().collect();
    let max_k = prev_words.len().min(next_words.len());

    for k in (1..=max_k).rev() {
        let matches = prev_words[prev_words.len() - k..]
            .iter()
            .zip(&next_words[..k])
            .all(|(a, b)| normalize(a) == normalize(b));
        if matches {
            return next_words[k..].join(" ");
        }
    }
    next_words.join(" ")
}

fn normalize(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use auricle_core::Segment;

    fn final_seg(channel: ChannelId, t0: u64, t1: u64, text: &str) -> SttEvent {
        SttEvent::Final(Segment {
            channel,
            t_start_ms: t0,
            t_end_ms: t1,
            text: text.to_string(),
        })
    }

    #[test]
    fn overlapping_finals_are_deduped_word_level() {
        let mut a = Assembler::new("s");
        a.ingest(final_seg(
            ChannelId::Mic,
            0,
            5000,
            "the quick brown fox jumps over",
        ));
        // Next chunk overlaps 0.5 s and re-transcribes the overlap words,
        // with punctuation/case differences.
        let ev = a.ingest(final_seg(
            ChannelId::Mic,
            4500,
            9000,
            "Jumps over, the lazy dog.",
        ));
        match ev {
            Some(PipelineEvent::Transcript(t)) => {
                assert_eq!(t.text, "the lazy dog.");
            }
            other => panic!("expected transcript event, got {other:?}"),
        }
    }

    #[test]
    fn fully_duplicated_final_is_suppressed() {
        let mut a = Assembler::new("s");
        a.ingest(final_seg(ChannelId::Mic, 0, 5000, "hello world"));
        let ev = a.ingest(final_seg(ChannelId::Mic, 4500, 4900, "Hello world"));
        assert!(ev.is_none(), "{ev:?}");
        assert_eq!(a.transcript().len(), 1);
    }

    #[test]
    fn non_overlapping_finals_are_untouched() {
        let mut a = Assembler::new("s");
        a.ingest(final_seg(ChannelId::Mic, 0, 2000, "see you tomorrow"));
        let ev = a.ingest(final_seg(ChannelId::Mic, 6000, 8000, "tomorrow works"));
        match ev {
            Some(PipelineEvent::Transcript(t)) => assert_eq!(t.text, "tomorrow works"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn channels_are_speaker_tagged_and_merged_by_timestamp() {
        let mut a = Assembler::new("s");
        // Loopback (Them) event arrives AFTER a later mic (You) event:
        // channels gap independently, arrival order != time order.
        a.ingest(final_seg(ChannelId::Mic, 10_000, 12_000, "my reply"));
        a.ingest(final_seg(
            ChannelId::Loopback,
            2_000,
            6_000,
            "their question",
        ));

        let t = a.transcript();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].speaker, "Them");
        assert_eq!(t[0].text, "their question");
        assert_eq!(t[1].speaker, "You");
        assert_eq!(t[1].text, "my reply");
    }

    #[test]
    fn overlap_dedup_is_per_channel_not_cross_channel() {
        let mut a = Assembler::new("s");
        a.ingest(final_seg(ChannelId::Mic, 0, 5000, "we should ship it"));
        // Loopback overlaps mic in time and repeats words — must NOT dedup.
        let ev = a.ingest(final_seg(ChannelId::Loopback, 4000, 6000, "ship it today"));
        match ev {
            Some(PipelineEvent::Transcript(t)) => assert_eq!(t.text, "ship it today"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stale_partial_after_final_is_suppressed() {
        let mut a = Assembler::new("s");
        a.ingest(final_seg(ChannelId::Mic, 0, 5000, "final text"));
        let ev = a.ingest(SttEvent::Partial(Segment {
            channel: ChannelId::Mic,
            t_start_ms: 0,
            t_end_ms: 4000,
            text: "partial text".to_string(),
        }));
        assert!(ev.is_none());
    }

    #[test]
    fn live_partial_is_broadcast_not_stored() {
        let mut a = Assembler::new("s");
        let ev = a.ingest(SttEvent::Partial(Segment {
            channel: ChannelId::Loopback,
            t_start_ms: 0,
            t_end_ms: 1000,
            text: "so far".to_string(),
        }));
        match ev {
            Some(PipelineEvent::Transcript(t)) => {
                assert!(!t.is_final);
                assert_eq!(t.speaker, "Them");
            }
            other => panic!("{other:?}"),
        }
        assert!(a.transcript().is_empty());
    }

    #[test]
    fn subscribers_receive_broadcast_events() {
        let mut a = Assembler::new("s");
        let mut rx = a.subscribe();
        a.ingest(final_seg(ChannelId::Mic, 0, 1000, "hi"));
        match rx.try_recv() {
            Ok(PipelineEvent::Transcript(t)) => assert_eq!(t.text, "hi"),
            other => panic!("{other:?}"),
        }
    }
}
