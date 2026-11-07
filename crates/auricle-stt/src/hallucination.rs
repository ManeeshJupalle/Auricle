//! Whisper invents stock phrases on near-silence ("Thank you." on a breath).
//! Those phrases are also things people really say, so text alone never
//! drops anything: a phrase only counts as invented when the model's own
//! no-speech estimate backs that up, or the audio does for a provider that
//! gives no estimate.

use crate::repetition::collapse_repetition;

/// Loudest 100 ms window (RMS) below this is a quiet chunk. Speech in
/// `fixtures/tts_loopback_16k.wav` peaks at 0.08-0.21 per window. Taking the
/// loudest window rather than the chunk average keeps one short quiet "thank
/// you" in a mostly silent 5 s chunk from reading as no speech at all.
const LOW_ENERGY_RMS: f32 = 0.02;

/// Matches whisper.cpp's default `no_speech_thold`.
const NO_SPEECH_PROB: f32 = 0.6;

/// Fewer words than this can't show a loop: "no no no" is just speech.
const REPETITION_MIN_WORDS: usize = 6;

/// Normalized by [`normalize`]: lowercase, alphanumerics and spaces only.
const KNOWN_PHRASES: &[&str] = &[
    "bye",
    "please subscribe",
    "subtitles by the amaraorg community",
    "thank you",
    "thank you for listening",
    "thank you for watching",
    "thank you very much",
    "thanks for listening",
    "thanks for watching",
    "you",
];

/// Whisper emits bracketed pseudo-events for non-speech audio, e.g.
/// "[BLANK_AUDIO]", "(soft music)", "♪". Filter pieces that contain no
/// alphanumeric content outside such annotations.
pub(crate) fn is_noise_annotation(s: &str) -> bool {
    let wrapped = (s.starts_with('[') && s.ends_with(']'))
        || (s.starts_with('(') && s.ends_with(')'))
        || (s.starts_with('*') && s.ends_with('*'));
    wrapped || !s.chars().any(|c| c.is_alphanumeric())
}

pub(crate) fn is_low_energy(samples: &[f32], sample_rate_hz: u32) -> bool {
    let window = (sample_rate_hz as usize / 10).max(1);
    let loudest = samples
        .chunks(window)
        .map(|w| (w.iter().map(|s| s * s).sum::<f32>() / w.len() as f32).sqrt())
        .fold(0.0, f32::max);
    loudest < LOW_ENERGY_RMS
}

/// True only for a known phrase on audio that looks like no speech. The
/// model's estimate outranks energy, since a quiet "thank you" is still speech.
/// A phrase stuck in a loop counts as the phrase.
pub(crate) fn is_hallucination(text: &str, low_energy: bool, no_speech_prob: Option<f32>) -> bool {
    let unlikely_speech = no_speech_prob.map_or(low_energy, |p| p >= NO_SPEECH_PROB);
    unlikely_speech && is_known_phrase(&collapse_repetition(text))
}

fn is_known_phrase(text: &str) -> bool {
    KNOWN_PHRASES.contains(&normalize(text).as_str())
}

/// Whether `text` may prime the next decode. A stock phrase or a repetition
/// loop kept as context invites the model to carry on with it, so unlike
/// [`is_hallucination`] this takes no evidence: losing context costs little.
pub(crate) fn fit_for_prompt(text: &str) -> bool {
    !is_known_phrase(text) && !is_repetitive(text)
}

/// Mostly the same few words over and over. Real sentences reuse little, even
/// with "the" and "and" in them.
fn is_repetitive(text: &str) -> bool {
    let normalized = normalize(text);
    let words: Vec<&str> = normalized.split_whitespace().collect();
    let distinct: std::collections::HashSet<&&str> = words.iter().collect();
    words.len() >= REPETITION_MIN_WORDS && distinct.len() * 2 < words.len()
}

fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_annotations_are_filtered() {
        assert!(is_noise_annotation("[BLANK_AUDIO]"));
        assert!(is_noise_annotation("(soft music)"));
        assert!(is_noise_annotation("♪"));
        assert!(is_noise_annotation("*sigh*"));
        assert!(!is_noise_annotation("hello world"));
        assert!(!is_noise_annotation("it works (mostly) fine"));
    }

    #[test]
    fn known_phrase_needs_audio_evidence_to_be_dropped() {
        assert!(is_hallucination("Thank you.", true, None));
        assert!(is_hallucination(" thank  YOU! ", false, Some(0.9)));
        // A person really said it: normal energy, confident model.
        assert!(!is_hallucination("Thank you.", false, None));
        assert!(!is_hallucination("Thank you.", false, Some(0.1)));
    }

    #[test]
    fn a_stock_phrase_stuck_in_a_loop_is_still_the_stock_phrase() {
        let looped = "Thanks for watching. ".repeat(12);
        assert!(is_hallucination(&looped, true, Some(0.9)));
        // Still needs the evidence: said for real, it collapses but stays.
        assert!(!is_hallucination(&looped, false, Some(0.1)));
    }

    #[test]
    fn quiet_audio_alone_is_not_enough_when_the_model_heard_speech() {
        assert!(!is_hallucination("Thank you.", true, Some(0.1)));
        assert!(is_hallucination("Thank you.", true, Some(0.9)));
    }

    #[test]
    fn real_sentences_survive_even_on_quiet_audio() {
        assert!(!is_hallucination(
            "Thank you for the update.",
            true,
            Some(0.9)
        ));
        assert!(!is_hallucination("ship it on friday", true, None));
    }

    #[test]
    fn stock_phrases_and_loops_never_prime_the_next_decode() {
        assert!(!fit_for_prompt("Thank you."));
        assert!(!fit_for_prompt("thanks for watching"));
        assert!(!fit_for_prompt(
            "I'm sorry. I'm sorry. I'm sorry. I'm sorry."
        ));
        assert!(!fit_for_prompt("the the the the the the the"));
    }

    #[test]
    fn ordinary_speech_primes_the_next_decode() {
        assert!(fit_for_prompt("Thank you for the update."));
        assert!(fit_for_prompt("no no no"));
        assert!(fit_for_prompt(
            "and then the team said that the plan for the launch was the same"
        ));
    }

    #[test]
    fn energy_is_judged_by_the_loudest_window() {
        let rate = 16_000;
        assert!(is_low_energy(&vec![0.0; 16_000], rate));
        assert!(is_low_energy(&vec![0.005; 16_000], rate));
        assert!(!is_low_energy(&vec![0.1; 16_000], rate));

        // 200 ms of speech in 5 s of silence: quiet on average (~0.02), but
        // a person spoke, so the chunk must not read as low energy.
        let mut mostly_silent = vec![0.0; 80_000];
        mostly_silent[40_000..43_200].fill(0.1);
        assert!(!is_low_energy(&mostly_silent, rate));
    }
}
