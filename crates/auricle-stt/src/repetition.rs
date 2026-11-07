//! Collapses the loops Whisper falls into, where one word, a phrase, or a run
//! of unspaced CJK text repeats until the decoder runs out of tokens.
//!
//! Ported from voicebox's `collapse_repetitive_artifacts`
//! (backend/services/refinement.py, MIT: see THIRD_PARTY_NOTICES.md), with
//! changes that all lean toward keeping real speech: one copy of a loop stays
//! instead of none, one word needs a longer run than a phrase does, and a run
//! inside a word of a spaced script never counts, so "hahahaha" and
//! "1000000000000" are left alone.

/// Copies of one word in a row before they count as a loop. People really do
/// say "no no no no no" and laugh "ha ha ha ha ha ha ha ha", and Whisper writes
/// that down; its own loops run to dozens of copies.
const WORD_LOOP_COPIES: usize = 10;

/// The same for a phrase. Six times "thank you for watching" is longer than a
/// 5 s chunk can hold at any speaking rate, while people repeat a phrase two
/// or three times at most.
const PHRASE_LOOP_COPIES: usize = 6;

/// Longest repeating unit looked for. Voicebox's 60 characters cover every
/// stock loop it has seen ("Please like and subscribe to my channel." is 41).
const MAX_UNIT_WORDS: usize = 15;
const MAX_UNIT_CHARS: usize = 60;

/// `text` with each machine loop cut to its first copy. Text without one comes
/// back as it was.
pub(crate) fn collapse_repetition(text: &str) -> String {
    collapse_unspaced(&collapse_words(text))
}

fn collapse_words(text: &str) -> String {
    // Each piece keeps its trailing whitespace, so the kept pieces join back
    // into the original text.
    let pieces: Vec<&str> = text.split_inclusive(char::is_whitespace).collect();
    let keys: Vec<String> = pieces.iter().map(|p| word_key(p)).collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < pieces.len() {
        let found = loop_at(&keys[i..], MAX_UNIT_WORDS, |unit, copies| {
            let needed = if unit.len() == 1 {
                WORD_LOOP_COPIES
            } else {
                PHRASE_LOOP_COPIES
            };
            copies >= needed && !unit[0].is_empty()
        });
        let (keep, skip) = found.unwrap_or((1, 1));
        out.extend(pieces[i..i + keep].iter().copied());
        i += skip;
    }
    out.trim_end().to_string()
}

/// CJK has no spaces to split on, so its loops are found character by
/// character. Elsewhere that would find runs inside words.
fn collapse_unspaced(text: &str) -> String {
    if !text.chars().any(is_cjk) {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let found = loop_at(&chars[i..], MAX_UNIT_CHARS, |unit, copies| {
            // Two characters are about one word.
            let needed = if unit.len() <= 2 {
                WORD_LOOP_COPIES
            } else {
                PHRASE_LOOP_COPIES
            };
            // One character over and over is emphasis or laughter ("哈哈哈").
            copies >= needed
                && unit.iter().any(|c| is_cjk(*c))
                && unit.iter().any(|c| *c != unit[0])
        });
        let (keep, skip) = found.unwrap_or((1, 1));
        out.extend(&chars[i..i + keep]);
        i += skip;
    }
    out.trim_end().to_string()
}

/// A loop starting at the head of `items`, shortest unit first: the unit's
/// length, and how far its copies reach.
fn loop_at<T: PartialEq>(
    items: &[T],
    max_unit: usize,
    is_loop: impl Fn(&[T], usize) -> bool,
) -> Option<(usize, usize)> {
    for unit in 1..=max_unit.min(items.len() / 2) {
        // Every item matches the one a unit back, so a last copy cut short
        // still counts toward the end.
        let end = unit
            + items[unit..]
                .iter()
                .zip(items)
                .take_while(|(a, b)| a == b)
                .count();
        let copies = end / unit;
        if is_loop(&items[..unit], copies) {
            // Cut short at the very end of the text is the decoder running out
            // of tokens. Followed by more words, it's where real speech starts.
            let end = if end == items.len() {
                end
            } else {
                copies * unit
            };
            return Some((unit, end));
        }
    }
    None
}

/// Compared without case or punctuation, so "URL," and "url." are one word.
fn word_key(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Kana, CJK ideographs and Hangul syllables.
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30FF}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{AC00}'..='\u{D7AF}'
        | '\u{F900}'..='\u{FAFF}')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn times(unit: &str, n: usize) -> String {
        unit.repeat(n)
    }

    // ---------- real speech stays ----------

    #[test]
    fn people_repeating_themselves_are_left_alone() {
        for said in [
            "no no no",
            "ha ha ha",
            "very very good",
            "no no no no no",
            "I said no, no, no, no, no and she left",
            "ha ha ha ha ha ha ha ha",
            "yeah yeah yeah yeah yeah yeah yeah yeah yeah",
            "bye bye bye bye bye bye",
            "thank you thank you thank you",
            "thanks for watching thanks for watching thanks for watching thanks for watching thanks for watching",
            "again and again and again and again",
        ] {
            assert_eq!(collapse_repetition(said), said);
        }
    }

    #[test]
    fn runs_inside_a_word_are_not_loops() {
        let long_wow = format!("w{}w", "o".repeat(40));
        for said in [
            "that's wooooooow amazing",
            long_wow.as_str(),
            "hahahahahahahahahahahahaha",
            "hmmmmmmmmmmmmmmmm",
            "we raised 1000000000000 dollars",
            "it was 0.3333333333333333",
        ] {
            assert_eq!(collapse_repetition(said), said);
        }
    }

    #[test]
    fn text_without_a_loop_is_unchanged() {
        assert_eq!(collapse_repetition(""), "");
        assert_eq!(collapse_repetition("just three words"), "just three words");
        let sentence = "and then the team said that the plan for the launch was the same";
        assert_eq!(collapse_repetition(sentence), sentence);
    }

    #[test]
    fn short_cjk_repeats_are_speech() {
        for said in [
            "好好好好好",
            "哈哈哈哈哈哈哈哈哈哈哈哈",
            "はいはいはいはいはいはい",
            "对对对",
        ] {
            assert_eq!(collapse_repetition(said), said);
        }
    }

    // ---------- machine loops go, down to one copy ----------

    #[test]
    fn a_long_single_word_run_keeps_one_copy() {
        let raw = format!("Hello {} goodbye", times("URL ", 10).trim());
        assert_eq!(collapse_repetition(&raw), "Hello URL goodbye");
        let raw = format!("Hello {}", times("URL ", 40));
        assert_eq!(collapse_repetition(&raw), "Hello URL");
    }

    #[test]
    fn a_word_loop_matches_across_case_and_punctuation() {
        let raw = format!("Hello {}URL. goodbye", times("URL, ", 9));
        assert_eq!(collapse_repetition(&raw), "Hello URL, goodbye");
        let mixed = ["Url", "URL", "url"].repeat(4).join(" ");
        assert_eq!(
            collapse_repetition(&format!("hi {mixed} bye")),
            "hi Url bye"
        );
    }

    #[test]
    fn a_phrase_loop_keeps_one_copy() {
        let raw = format!(
            "Okay so the meeting is at three. {}",
            times("thanks for watching ", 6)
        );
        assert_eq!(
            collapse_repetition(&raw),
            "Okay so the meeting is at three. thanks for watching"
        );

        let raw = format!("The point is clear. {}right", times("please like and ", 7));
        assert_eq!(
            collapse_repetition(&raw),
            "The point is clear. please like and right"
        );

        let unit = "Please like and subscribe to my channel. ";
        let raw = format!("End of video. {}", times(unit, 6));
        assert_eq!(
            collapse_repetition(&raw),
            format!("End of video. {}", unit.trim())
        );
    }

    #[test]
    fn a_sentence_looped_with_its_punctuation_keeps_one_copy() {
        let raw = times("I'm sorry. ", 8);
        assert_eq!(collapse_repetition(&raw), "I'm sorry.");
    }

    #[test]
    fn a_copy_cut_off_by_the_token_limit_goes_with_the_loop() {
        let raw = format!(
            "we ship friday {}thanks for",
            times("thanks for watching ", 8)
        );
        assert_eq!(
            collapse_repetition(&raw),
            "we ship friday thanks for watching"
        );
    }

    #[test]
    fn real_words_after_a_loop_are_kept_even_when_they_start_like_it() {
        let raw = format!("{}thanks for coming", times("thanks for watching ", 6));
        assert_eq!(
            collapse_repetition(&raw),
            "thanks for watching thanks for coming"
        );
    }

    #[test]
    fn a_loop_mid_text_keeps_what_surrounds_it() {
        let raw = format!("we agreed {}to ship it", times("the plan is ", 12));
        assert_eq!(
            collapse_repetition(&raw),
            "we agreed the plan is to ship it"
        );
    }

    #[test]
    fn cjk_loops_without_spaces_keep_one_copy() {
        let raw = format!("會議在三點開始{}", times("謝謝觀看", 7));
        assert_eq!(collapse_repetition(&raw), "會議在三點開始謝謝觀看");

        let unit = "ご視聴ありがとうございました";
        let raw = format!("明日の会議は午後三時です。{}", times(unit, 6));
        assert_eq!(
            collapse_repetition(&raw),
            format!("明日の会議は午後三時です。{unit}")
        );

        let raw = times("감사합니다", 8);
        assert_eq!(collapse_repetition(&raw), "감사합니다");
    }

    #[test]
    fn a_two_character_cjk_word_needs_the_longer_run() {
        assert_eq!(collapse_repetition(&times("好的", 12)), "好的");
        assert_eq!(collapse_repetition(&times("好的", 9)), times("好的", 9));
    }

    #[test]
    fn the_thresholds_sit_where_the_tests_above_say() {
        let word = |n| times("no ", n).trim().to_string();
        assert_eq!(collapse_repetition(&word(WORD_LOOP_COPIES - 1)), word(9));
        assert_eq!(collapse_repetition(&word(WORD_LOOP_COPIES)), "no");
        let phrase = |n| times("thank you ", n).trim().to_string();
        assert_eq!(
            collapse_repetition(&phrase(PHRASE_LOOP_COPIES - 1)),
            phrase(5)
        );
        assert_eq!(
            collapse_repetition(&phrase(PHRASE_LOOP_COPIES)),
            "thank you"
        );
    }
}
