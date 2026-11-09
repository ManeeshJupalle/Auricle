use auricle_core::{ChannelId, Segment, SttEvent};

/// How long after a loopback line a mic line may start and still be that
/// audio coming back through the air. Never before it: loopback taps the mix
/// ahead of the speakers, so a mic line that leads is the user, whose voice
/// reached loopback through a monitor path. The delay is output buffering and
/// the flight to the mic, tens of ms on wired speakers and up to ~300 ms on
/// Bluetooth, then up to ~100 ms for the VAD to trigger on the quieter copy.
/// Nobody repeats a line this close behind it: they haven't heard it yet.
const BLEED_MAX_LAG_MS: u64 = 500;
/// How far apart the two lines may end, in either direction. Wider than the
/// start because each channel's VAD closes on its own hangover and a partial
/// trails its audio; timing alone never decides this end.
const BLEED_END_SLACK_MS: u64 = 1_500;
/// Shorter matches are too likely to be real: "yes", "sounds good to me".
const BLEED_MIN_WORDS: usize = 5;
/// Percent of words (in order) two lines must share to count as one. Not 100
/// because the mic hears a worse copy and transcribes it differently.
const BLEED_MIN_SIMILARITY_PCT: usize = 80;
/// Longest a mic final may be, as a percentage of a loopback partial's words,
/// and still match it. A partial lags its audio by about one interim, so the
/// mic may have a few words more, but a much longer line is someone talking.
const BLEED_MAX_PARTIAL_OVERRUN_PCT: usize = 150;

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
/// - A mic final that repeats a loopback line is dropped: on laptop speakers
///   the mic hears the far side, and "Them" would reappear as "You". It must
///   start just after the loopback line, end near it, and share nearly all
///   its words, over a minimum word count. The channels finish in either
///   order, so a mic final that gets here first is matched against the
///   latest loopback partial instead. Drops are counted in
///   [`bleed_finals`](Self::bleed_finals), and
///   [`set_drop_mic_bleed`](Self::set_drop_mic_bleed) turns this off.
///
///   Not covered, so these duplicates stay: a mic final that arrives before
///   any aligned loopback text long enough to match (short sentences, shed
///   interims), because it is already out by the time the evidence comes and
///   taking it back would need a retract event through the store and UI; a
///   mic final that spans or splits several loopback finals, because lines
///   are compared one to one; a line after loopback was silent, because that
///   span has no pre-roll, which puts the mic's start ahead of it; and
///   languages written without spaces, which never reach the word minimum.
pub struct Assembler {
    session_id: String,
    finals: Vec<TranscriptEvent>,
    last_final_end: [(ChannelId, u64, String); 2],
    loopback_partial: Option<Segment>,
    drop_mic_bleed: bool,
    bleed_finals: u64,
}

impl Assembler {
    pub fn new(session_id: &str) -> Self {
        Assembler {
            session_id: session_id.to_string(),
            finals: Vec::new(),
            last_final_end: [
                (ChannelId::Mic, 0, String::new()),
                (ChannelId::Loopback, 0, String::new()),
            ],
            loopback_partial: None,
            drop_mic_bleed: true,
            bleed_finals: 0,
        }
    }

    /// Whether mic finals that repeat a loopback line are dropped (default).
    pub fn set_drop_mic_bleed(&mut self, drop: bool) {
        self.drop_mic_bleed = drop;
    }

    /// Mic finals dropped as bleed so far.
    pub fn bleed_finals(&self) -> u64 {
        self.bleed_finals
    }

    pub fn speaker_for(channel: ChannelId) -> &'static str {
        match channel {
            ChannelId::Mic => "You",
            ChannelId::Loopback => "Them",
        }
    }

    /// Ingest one STT event; returns the event to emit (if any).
    pub fn ingest(&mut self, event: SttEvent) -> Option<PipelineEvent> {
        match event {
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
                    if seg.channel == ChannelId::Loopback {
                        self.loopback_partial = Some(seg.clone());
                    }
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
                // The final says everything the partial did and more.
                if seg.channel == ChannelId::Loopback {
                    self.loopback_partial = None;
                }
                // The far side heard through the speakers.
                let bleed = self.drop_mic_bleed
                    && seg.channel == ChannelId::Mic
                    && !text.is_empty()
                    && self.is_bleed(seg.t_start_ms, seg.t_end_ms, &text);
                self.bleed_finals += bleed as u64;
                // Or nothing left after trimming the previous final.
                if bleed || text.is_empty() {
                    None
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
        }
    }

    /// The merged transcript so far, ordered by start time across channels.
    pub fn transcript(&self) -> &[TranscriptEvent] {
        &self.finals
    }

    /// Whether a mic final is loopback speech picked up off the speakers.
    /// Finals must match in full. A partial trails its audio, so only the
    /// words it has so far can count, and the mic final may run on past it
    /// only as far as that lag explains.
    fn is_bleed(&self, t_start_ms: u64, t_end_ms: u64, text: &str) -> bool {
        let mic = words(text);
        if mic.len() < BLEED_MIN_WORDS {
            return false;
        }
        let aligned = |start: u64, end: u64| {
            (start..=start + BLEED_MAX_LAG_MS).contains(&t_start_ms)
                && end.abs_diff(t_end_ms) <= BLEED_END_SLACK_MS
        };

        let first = self
            .finals
            .partition_point(|f| f.t_start_ms + BLEED_MAX_LAG_MS < t_start_ms);
        let matches_final = self.finals[first..]
            .iter()
            .take_while(|f| f.t_start_ms <= t_start_ms)
            .filter(|f| f.channel == ChannelId::Loopback && aligned(f.t_start_ms, f.t_end_ms))
            .any(|f| {
                let theirs = words(&f.text);
                is_similar(common_words(&mic, &theirs), mic.len().max(theirs.len()))
            });

        matches_final
            || self.loopback_partial.as_ref().is_some_and(|p| {
                let theirs = words(&p.text);
                aligned(p.t_start_ms, p.t_end_ms)
                    && theirs.len() >= BLEED_MIN_WORDS
                    && mic.len() * 100 <= theirs.len() * BLEED_MAX_PARTIAL_OVERRUN_PCT
                    && is_similar(common_words(&mic, &theirs), theirs.len())
            })
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

fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(normalize)
        .filter(|w| !w.is_empty())
        .collect()
}

fn is_similar(common: usize, total: usize) -> bool {
    common * 100 >= total * BLEED_MIN_SIMILARITY_PCT
}

/// Length of the longest common subsequence of two word lists: the words
/// they share, in order.
fn common_words(a: &[String], b: &[String]) -> usize {
    let mut row = vec![0; b.len() + 1];
    for x in a {
        let mut diagonal = 0;
        for (j, y) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = if x == y {
                diagonal + 1
            } else {
                above.max(row[j])
            };
            diagonal = above;
        }
    }
    row[b.len()]
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

    const SENTENCE: &str = "we should ship the release on friday morning";

    fn partial_seg(channel: ChannelId, t0: u64, t1: u64, text: &str) -> SttEvent {
        SttEvent::Partial(Segment {
            channel,
            t_start_ms: t0,
            t_end_ms: t1,
            text: text.to_string(),
        })
    }

    fn speakers(a: &Assembler) -> Vec<&str> {
        a.transcript().iter().map(|t| t.speaker.as_str()).collect()
    }

    #[test]
    fn bleed_after_the_loopback_final_is_dropped() {
        let mut a = Assembler::new("s");
        a.ingest(final_seg(ChannelId::Loopback, 10_000, 13_500, SENTENCE));
        let ev = a.ingest(final_seg(
            ChannelId::Mic,
            10_150,
            13_700,
            "We should ship the release on Friday morning.",
        ));
        assert!(ev.is_none(), "{ev:?}");
        assert_eq!(speakers(&a), ["Them"]);
    }

    #[test]
    fn bleed_with_a_transcription_error_is_dropped() {
        let mut a = Assembler::new("s");
        a.ingest(final_seg(ChannelId::Loopback, 10_000, 13_500, SENTENCE));
        // The mic hears a worse copy, so its words won't match exactly.
        let ev = a.ingest(final_seg(
            ChannelId::Mic,
            10_150,
            13_700,
            "we should ship a release on friday morning",
        ));
        assert!(ev.is_none(), "{ev:?}");
    }

    #[test]
    fn bleed_ahead_of_the_loopback_final_is_caught_by_its_partial() {
        let mut a = Assembler::new("s");
        a.ingest(partial_seg(
            ChannelId::Loopback,
            10_000,
            12_500,
            "we should ship the release on",
        ));
        let ev = a.ingest(final_seg(ChannelId::Mic, 10_150, 13_700, SENTENCE));
        assert!(ev.is_none(), "{ev:?}");

        a.ingest(final_seg(ChannelId::Loopback, 10_000, 13_500, SENTENCE));
        assert_eq!(speakers(&a), ["Them"]);
    }

    /// Pins the known gap: with no loopback text yet there is nothing to
    /// match, and by the time there is, the mic line is already out.
    #[test]
    fn bleed_ahead_of_all_loopback_text_stays() {
        let mut a = Assembler::new("s");
        let ev = a.ingest(final_seg(ChannelId::Mic, 10_150, 13_700, SENTENCE));
        assert!(ev.is_some());
        let ev = a.ingest(final_seg(ChannelId::Loopback, 10_000, 13_500, SENTENCE));
        assert!(ev.is_some(), "loopback lines are never dropped");
        assert_eq!(speakers(&a), ["Them", "You"]);
    }

    #[test]
    fn short_replies_survive_even_when_identical() {
        let mut a = Assembler::new("s");
        a.ingest(final_seg(ChannelId::Loopback, 10_000, 10_400, "yes"));
        assert!(a
            .ingest(final_seg(ChannelId::Mic, 10_100, 10_500, "yes"))
            .is_some());

        a.ingest(final_seg(
            ChannelId::Loopback,
            20_000,
            21_000,
            "sounds good to me",
        ));
        assert!(a
            .ingest(final_seg(
                ChannelId::Mic,
                20_100,
                21_100,
                "sounds good to me"
            ))
            .is_some());
    }

    #[test]
    fn repeating_one_of_their_phrases_survives() {
        let mut a = Assembler::new("s");
        a.ingest(final_seg(
            ChannelId::Loopback,
            10_000,
            14_000,
            "i think we should ship the release on friday morning what do you think",
        ));
        let ev = a.ingest(final_seg(ChannelId::Mic, 14_300, 16_000, SENTENCE));
        assert!(ev.is_some(), "a reply after them is not bleed");
    }

    #[test]
    fn repeating_their_whole_sentence_back_survives() {
        let mut a = Assembler::new("s");
        a.ingest(final_seg(ChannelId::Loopback, 10_000, 12_500, SENTENCE));
        let ev = a.ingest(final_seg(ChannelId::Mic, 12_800, 15_300, SENTENCE));
        assert!(ev.is_some(), "same words, but said after them");
    }

    #[test]
    fn talking_over_them_about_something_else_survives() {
        let mut a = Assembler::new("s");
        a.ingest(final_seg(
            ChannelId::Loopback,
            10_000,
            13_000,
            "the quarterly numbers came in above forecast again",
        ));
        let ev = a.ingest(final_seg(
            ChannelId::Mic,
            10_200,
            13_200,
            "can you hear me okay on your side",
        ));
        assert!(ev.is_some());
    }

    #[test]
    fn overlapping_mic_final_with_shared_words_survives() {
        // The mirror of overlap_dedup_is_per_channel_not_cross_channel, with
        // the mic arriving second, where bleed matching applies.
        let mut a = Assembler::new("s");
        a.ingest(final_seg(ChannelId::Loopback, 0, 5000, "we should ship it"));
        let ev = a.ingest(final_seg(ChannelId::Mic, 4000, 6000, "ship it today"));
        assert!(ev.is_some());
    }

    #[test]
    fn a_partial_cannot_drop_a_mic_final_that_runs_on_past_it() {
        let mut a = Assembler::new("s");
        a.ingest(partial_seg(
            ChannelId::Loopback,
            10_000,
            12_000,
            "we should ship the release on",
        ));
        let ev = a.ingest(final_seg(
            ChannelId::Mic,
            10_100,
            16_000,
            "we should ship the release on friday but i would wait another full week",
        ));
        assert!(ev.is_some(), "the extra words are real speech");
    }

    #[test]
    fn old_loopback_text_cannot_drop_a_later_mic_final() {
        let mut a = Assembler::new("s");
        a.ingest(final_seg(ChannelId::Loopback, 10_000, 13_500, SENTENCE));
        a.ingest(partial_seg(ChannelId::Loopback, 14_000, 17_000, SENTENCE));
        let ev = a.ingest(final_seg(ChannelId::Mic, 60_000, 63_500, SENTENCE));
        assert!(ev.is_some());
    }

    #[test]
    fn a_dropped_bleed_final_still_trims_the_next_mic_chunk() {
        let mut a = Assembler::new("s");
        a.ingest(final_seg(ChannelId::Loopback, 0, 5000, SENTENCE));
        assert!(a
            .ingest(final_seg(ChannelId::Mic, 0, 5000, SENTENCE))
            .is_none());
        // The next chunk re-transcribes the 0.5 s overlap, as always.
        let ev = a.ingest(final_seg(
            ChannelId::Mic,
            4500,
            8000,
            "friday morning and then we celebrate",
        ));
        match ev {
            Some(PipelineEvent::Transcript(t)) => assert_eq!(t.text, "and then we celebrate"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_real_reply_that_echoes_them_survives() {
        let mut a = Assembler::new("s");
        a.ingest(partial_seg(
            ChannelId::Loopback,
            10_000,
            11_200,
            "we ship the release on",
        ));
        a.ingest(final_seg(
            ChannelId::Loopback,
            10_000,
            11_500,
            "we ship the release on friday",
        ));
        let ev = a.ingest(final_seg(
            ChannelId::Mic,
            11_300,
            12_600,
            "okay we ship the release on friday then",
        ));
        assert!(ev.is_some());
    }

    #[test]
    fn a_loopback_final_retires_its_partial() {
        let mut a = Assembler::new("s");
        a.ingest(partial_seg(
            ChannelId::Loopback,
            10_000,
            12_000,
            "we should ship the release on",
        ));
        a.ingest(final_seg(
            ChannelId::Loopback,
            10_000,
            14_000,
            "we should ship the release on friday morning and then call it a week",
        ));
        // Only the words they had said by the partial match this line, and
        // the final shows that was never all of it.
        let ev = a.ingest(final_seg(
            ChannelId::Mic,
            10_200,
            13_000,
            "we should ship the release on friday",
        ));
        assert!(ev.is_some());
    }

    #[test]
    fn a_longer_mic_line_containing_their_partial_survives() {
        let mut a = Assembler::new("s");
        a.ingest(partial_seg(
            ChannelId::Loopback,
            10_000,
            12_000,
            "i think we should create",
        ));
        let ev = a.ingest(final_seg(
            ChannelId::Mic,
            10_200,
            13_200,
            "i think we should create a new branch for this",
        ));
        assert!(ev.is_some(), "\"a new branch\" is real speech");
    }

    #[test]
    fn reading_their_line_back_as_they_finish_survives() {
        let mut a = Assembler::new("s");
        let line = "the code is four seven nine";
        a.ingest(final_seg(ChannelId::Loopback, 10_000, 11_800, line));
        let ev = a.ingest(final_seg(ChannelId::Mic, 10_900, 12_600, line));
        assert!(ev.is_some(), "same words, a person's reaction time later");
    }

    /// "Listen to this device", a virtual mixer's monitor bus, or a colleague's
    /// mic in the same room: the user's own voice reaches loopback, after the
    /// mic heard it. That is the reverse of bleed, and the line is theirs.
    #[test]
    fn a_monitor_mix_keeps_the_users_line() {
        let mut a = Assembler::new("s");
        a.ingest(final_seg(ChannelId::Loopback, 10_060, 13_560, SENTENCE));
        let ev = a.ingest(final_seg(ChannelId::Mic, 10_000, 13_500, SENTENCE));
        assert!(ev.is_some());
        assert_eq!(speakers(&a), ["You", "Them"]);
    }

    #[test]
    fn a_monitor_mix_keeps_the_users_line_against_a_partial() {
        let mut a = Assembler::new("s");
        a.ingest(partial_seg(
            ChannelId::Loopback,
            10_060,
            12_500,
            "we should ship the release on",
        ));
        let ev = a.ingest(final_seg(ChannelId::Mic, 10_000, 13_500, SENTENCE));
        assert!(ev.is_some());
    }

    #[test]
    fn bleed_never_leads_loopback_and_trails_it_by_at_most_the_bound() {
        let dropped_at = |mic_start: u64| {
            let mut a = Assembler::new("s");
            a.ingest(final_seg(ChannelId::Loopback, 10_000, 13_500, SENTENCE));
            let ev = a.ingest(final_seg(ChannelId::Mic, mic_start, 13_700, SENTENCE));
            ev.is_none()
        };
        assert!(!dropped_at(9_999), "the mic led");
        assert!(dropped_at(10_000));
        assert!(dropped_at(10_000 + BLEED_MAX_LAG_MS), "bluetooth speakers");
        assert!(!dropped_at(10_001 + BLEED_MAX_LAG_MS));
    }

    #[test]
    fn dropped_lines_are_counted() {
        let mut a = Assembler::new("s");
        a.ingest(final_seg(ChannelId::Loopback, 10_000, 13_500, SENTENCE));
        a.ingest(final_seg(ChannelId::Mic, 10_150, 13_700, SENTENCE));
        // Overlap trimming also yields nothing, and that isn't bleed.
        a.ingest(final_seg(ChannelId::Mic, 13_600, 13_650, "friday morning"));
        assert_eq!(a.bleed_finals(), 1);
    }

    #[test]
    fn with_the_filter_off_the_mic_line_stays() {
        let mut a = Assembler::new("s");
        a.set_drop_mic_bleed(false);
        a.ingest(final_seg(ChannelId::Loopback, 10_000, 13_500, SENTENCE));
        let ev = a.ingest(final_seg(ChannelId::Mic, 10_150, 13_700, SENTENCE));
        assert!(ev.is_some());
        assert_eq!(a.bleed_finals(), 0);
    }

    #[test]
    fn word_order_counts_toward_similarity() {
        let reversed = "morning friday on release the ship should we";
        let mut a = Assembler::new("s");
        a.ingest(final_seg(ChannelId::Loopback, 10_000, 13_500, SENTENCE));
        let ev = a.ingest(final_seg(ChannelId::Mic, 10_150, 13_700, reversed));
        assert!(ev.is_some());
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
}
