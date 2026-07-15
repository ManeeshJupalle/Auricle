//! Ask-prompt assembly (COPILOT_ARCHITECTURE §2.2).
//!
//! Budget is allocated in strict priority order: (1) the user's question,
//! (1b) earlier Q/A when the ask is a follow-up (a follow-up is
//! meaningless without the answer it follows), (2) screen text capped at
//! `copilot.max_screen_chars`, (3) the transcript window keeping the most
//! recent segments, (4) session metadata. Rendering order is the reverse
//! narrative order — context sections first, question last — but what
//! gets truncated when space runs out follows the priority list.

use crate::ring::RingSegment;
use auricle_vision::ScreenContext;

/// Whole-prompt character budget: ~6k estimated tokens at the ~4 chars/
/// token heuristic used across auricle-llm — safely inside common 8k
/// local-model context windows with room for the system template and the
/// completion.
const PROMPT_CHAR_BUDGET: usize = 24_000;

/// Follow-up history: newest pairs first, at most this many.
const HISTORY_MAX_PAIRS: usize = 3;
/// A single prior answer is context, not payload: cap each one.
const HISTORY_ANSWER_CAP: usize = 1_500;

/// One remembered ask + answer (in-memory only; persisted elsewhere and
/// only when `copilot.retain_context = true`).
#[derive(Debug, Clone)]
pub struct QaPair {
    pub question: String,
    pub answer: String,
}

pub struct TranscriptWindow<'a> {
    pub segments: &'a [RingSegment],
    pub window_min: u64,
}

pub struct AskPromptParts<'a> {
    pub question: &'a str,
    /// Prior Q/A pairs, oldest first. Empty unless `follow_up: true`.
    pub history: &'a [QaPair],
    pub screen: Option<&'a ScreenContext>,
    /// `None` when the ask did not request transcript context.
    pub transcript: Option<TranscriptWindow<'a>>,
    /// Session title, when a session is attached to the ask.
    pub session_title: Option<&'a str>,
}

pub fn assemble_user_prompt(parts: &AskPromptParts<'_>, max_screen_chars: usize) -> String {
    assemble_with_budget(parts, max_screen_chars, PROMPT_CHAR_BUDGET)
}

fn assemble_with_budget(
    parts: &AskPromptParts<'_>,
    max_screen_chars: usize,
    budget_chars: usize,
) -> String {
    let question_section = format!("## Question\n{}", parts.question.trim());
    let mut remaining = budget_chars.saturating_sub(chars(&question_section));

    // (1b) Follow-up history: keep the newest pairs that fit.
    let history_section = if parts.history.is_empty() {
        None
    } else {
        let mut kept: Vec<String> = Vec::new();
        for pair in parts.history.iter().rev().take(HISTORY_MAX_PAIRS) {
            let answer = truncate_chars(&pair.answer, HISTORY_ANSWER_CAP);
            let entry = format!("Q: {}\nA: {}", pair.question.trim(), answer);
            if chars(&entry) + 1 > remaining {
                break;
            }
            remaining -= chars(&entry) + 1;
            kept.push(entry);
        }
        if kept.is_empty() {
            None
        } else {
            kept.reverse(); // chronological again
            Some(format!(
                "## Earlier in this conversation\n{}",
                kept.join("\n")
            ))
        }
    };

    // (2) Screen text: config cap first, then whatever budget remains.
    let screen_section = parts.screen.map(|ctx| {
        let cap = max_screen_chars.min(remaining.saturating_sub(80)); // header allowance
        let text = truncate_chars(ctx.text.trim(), cap);
        let section = format!(
            "## Screen ({} — \"{}\")\n{}",
            ctx.app_name, ctx.window_title, text
        );
        remaining = remaining.saturating_sub(chars(&section));
        section
    });

    // (3) Transcript: keep the most recent segments that fit, render
    // oldest-to-newest.
    let transcript_section = parts.transcript.as_ref().map(|w| {
        let header = format!("## Recent transcript (last {} min)", w.window_min);
        let mut kept: Vec<String> = Vec::new();
        let mut used = chars(&header);
        for seg in w.segments.iter().rev() {
            let secs = seg.t_start_ms / 1000;
            let line = format!(
                "[{:02}:{:02}] {}: {}",
                secs / 60,
                secs % 60,
                seg.speaker,
                seg.text
            );
            if used + chars(&line) + 1 > remaining {
                break;
            }
            used += chars(&line) + 1;
            kept.push(line);
        }
        kept.reverse();
        remaining = remaining.saturating_sub(used);
        if kept.is_empty() {
            format!("{header}\n(nothing was captured in this window)")
        } else {
            format!("{header}\n{}", kept.join("\n"))
        }
    });

    // (4) Session metadata: one line, only if it still fits.
    let session_section = parts.session_title.and_then(|title| {
        let section =
            format!("## Session\nTitle: {title}. Speakers: You (microphone), Them (system audio).");
        (chars(&section) <= remaining).then_some(section)
    });

    // Narrative order: context first, question last.
    [
        session_section,
        transcript_section,
        screen_section,
        history_section,
        Some(question_section),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n\n")
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

/// Char-boundary-safe truncation with an ellipsis marker when cut.
fn truncate_chars(s: &str, max: usize) -> String {
    if chars(s) <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn seg(t_start_ms: u64, speaker: &str, text: &str) -> RingSegment {
        RingSegment::for_tests(t_start_ms, speaker, text, Instant::now())
    }

    fn screen(text: &str) -> ScreenContext {
        ScreenContext {
            window_title: "Sprint Board - Jira".into(),
            app_name: "chrome".into(),
            text: text.into(),
            captured_at: 0,
            ocr_ms: 100,
        }
    }

    #[test]
    fn all_sections_render_in_narrative_order_with_question_last() {
        let segs = vec![
            seg(12_000, "Them", "let's review latency"),
            seg(15_000, "You", "sure"),
        ];
        let sc = screen("Ticket AUR-42: fix chunker overlap");
        let history = vec![QaPair {
            question: "what is AUR-42?".into(),
            answer: "A chunker bug.".into(),
        }];
        let prompt = assemble_user_prompt(
            &AskPromptParts {
                question: "what did they just say?",
                history: &history,
                screen: Some(&sc),
                transcript: Some(TranscriptWindow {
                    segments: &segs,
                    window_min: 10,
                }),
                session_title: Some("Weekly sync"),
            },
            6_000,
        );

        let order = [
            "## Session",
            "## Recent transcript (last 10 min)",
            "[00:12] Them: let's review latency",
            "[00:15] You: sure",
            "## Screen (chrome — \"Sprint Board - Jira\")",
            "## Earlier in this conversation",
            "Q: what is AUR-42?",
            "## Question",
            "what did they just say?",
        ];
        let mut pos = 0;
        for needle in order {
            let at = prompt[pos..]
                .find(needle)
                .unwrap_or_else(|| panic!("{needle:?} missing or out of order in:\n{prompt}"));
            pos += at + needle.len();
        }
    }

    #[test]
    fn screen_text_truncates_to_the_config_cap() {
        let long = "x".repeat(10_000);
        let sc = screen(&long);
        let prompt = assemble_user_prompt(
            &AskPromptParts {
                question: "q",
                history: &[],
                screen: Some(&sc),
                transcript: None,
                session_title: None,
            },
            6_000,
        );
        let body = prompt
            .split("## Screen")
            .nth(1)
            .unwrap()
            .lines()
            .nth(1)
            .unwrap();
        assert_eq!(body.chars().count(), 6_000);
        assert!(body.ends_with('…'), "cut is marked");
    }

    #[test]
    fn transcript_keeps_the_newest_segments_when_budget_is_tight() {
        // ~30 chars per line, budget leaves room for only a few.
        let segs: Vec<RingSegment> = (0..200)
            .map(|i| seg(i * 1000, "Them", &format!("segment number {i:03} spoken")))
            .collect();
        let prompt = assemble_with_budget(
            &AskPromptParts {
                question: "q",
                history: &[],
                screen: None,
                transcript: Some(TranscriptWindow {
                    segments: &segs,
                    window_min: 10,
                }),
                session_title: None,
            },
            6_000,
            400,
        );
        assert!(
            prompt.contains("segment number 199"),
            "newest kept:\n{prompt}"
        );
        assert!(!prompt.contains("segment number 000"), "oldest dropped");
        // Chronological render: the kept-oldest appears before the newest.
        let first_kept = prompt.find("segment number 19").unwrap();
        let newest = prompt.find("segment number 199").unwrap();
        assert!(first_kept <= newest);
    }

    #[test]
    fn priority_question_survives_when_everything_else_is_squeezed_out() {
        let segs = vec![seg(0, "Them", &"t".repeat(500))];
        let sc = screen(&"s".repeat(500));
        let question = "the only thing that must survive";
        let prompt = assemble_with_budget(
            &AskPromptParts {
                question,
                history: &[],
                screen: Some(&sc),
                transcript: Some(TranscriptWindow {
                    segments: &segs,
                    window_min: 10,
                }),
                session_title: Some("title"),
            },
            6_000,
            chars(question) + 15, // just the question section
        );
        assert!(prompt.contains(question));
        assert!(!prompt.contains("ttttt"), "no transcript slipped in");
        assert!(!prompt.contains("sssss"), "no screen text slipped in");
        assert!(!prompt.contains("## Session"), "meta dropped last-priority");
    }

    #[test]
    fn history_keeps_newest_pairs_and_caps_answers() {
        let history: Vec<QaPair> = (0..5)
            .map(|i| QaPair {
                question: format!("question {i}"),
                answer: format!("answer {i} {}", "a".repeat(2_000)),
            })
            .collect();
        let prompt = assemble_user_prompt(
            &AskPromptParts {
                question: "follow-up",
                history: &history,
                screen: None,
                transcript: None,
                session_title: None,
            },
            6_000,
        );
        // Only the newest three pairs, chronological order.
        assert!(!prompt.contains("question 0"));
        assert!(!prompt.contains("question 1"));
        let p2 = prompt.find("question 2").unwrap();
        let p4 = prompt.find("question 4").unwrap();
        assert!(p2 < p4);
        // Every included answer is capped.
        for chunk in prompt.split("A: ").skip(1) {
            let answer_line = chunk.lines().next().unwrap();
            assert!(answer_line.chars().count() <= HISTORY_ANSWER_CAP);
        }
    }

    #[test]
    fn empty_transcript_window_is_stated_not_omitted() {
        let prompt = assemble_user_prompt(
            &AskPromptParts {
                question: "q",
                history: &[],
                screen: None,
                transcript: Some(TranscriptWindow {
                    segments: &[],
                    window_min: 10,
                }),
                session_title: None,
            },
            6_000,
        );
        assert!(prompt.contains("(nothing was captured in this window)"));
    }
}
