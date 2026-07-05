//! Long-transcript handling: transcripts under the token threshold get one
//! completion; above it, line-boundary chunks are summarized independently
//! (map) and the partial summaries merged under the template (reduce).

use auricle_core::Result;

use crate::LlmProvider;

/// Above this estimated token count the map-reduce path kicks in. Chosen
/// well under common 8k context windows, leaving room for the template and
/// the completion.
pub const MAP_REDUCE_TOKEN_THRESHOLD: usize = 6_000;

/// Map-phase chunk target, in estimated tokens.
const CHUNK_TOKENS: usize = 4_000;

/// Rough token estimate: ~4 chars per token for English text. Used only to
/// pick the code path, so being approximate is fine.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// Split `text` into chunks of at most `max_chars`, only ever cutting at
/// line boundaries (transcript lines are the atomic unit). A single line
/// longer than `max_chars` becomes its own oversized chunk rather than
/// being split mid-line.
pub fn chunk_by_lines(text: &str, max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        // +1 for the newline that joins lines within a chunk.
        let added = line.chars().count() + if current.is_empty() { 0 } else { 1 };
        if !current.is_empty() && current.chars().count() + added > max_chars {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Produce a summary of `transcript` under `template`, map-reducing when
/// the transcript exceeds the token threshold.
pub async fn summarize(
    provider: &dyn LlmProvider,
    template: &str,
    transcript: &str,
) -> Result<String> {
    if estimate_tokens(transcript) <= MAP_REDUCE_TOKEN_THRESHOLD {
        return provider.complete(template, transcript).await;
    }

    let chunks = chunk_by_lines(transcript, CHUNK_TOKENS * 4);
    let total = chunks.len();
    let map_system = "You summarize one portion of a longer meeting transcript. \
         Produce a faithful, detailed summary of THIS PORTION ONLY, keeping \
         speaker attributions (You/Them), numbers, names, decisions, and \
         action items. Do not speculate about the rest of the meeting.";
    let mut partials = Vec::with_capacity(total);
    for (i, chunk) in chunks.iter().enumerate() {
        let user = format!("Portion {} of {total}:\n\n{chunk}", i + 1);
        partials.push(provider.complete(map_system, &user).await?);
    }

    let reduce_input = partials
        .iter()
        .enumerate()
        .map(|(i, p)| format!("--- portion {} of {total} ---\n{p}", i + 1))
        .collect::<Vec<_>>()
        .join("\n\n");
    let reduce_system = format!(
        "The following are faithful summaries of consecutive portions of one \
         meeting transcript. Merge them and apply this instruction to the \
         whole meeting:\n\n{template}"
    );
    provider.complete(&reduce_system, &reduce_input).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_is_chars_over_four() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens(&"x".repeat(24_000)), 6_000);
    }

    #[test]
    fn chunks_respect_line_boundaries_and_cover_everything() {
        let lines: Vec<String> = (0..100)
            .map(|i| format!("[{i:02}] line number {i}"))
            .collect();
        let text = lines.join("\n");
        let chunks = chunk_by_lines(&text, 200);

        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.chars().count() <= 200, "chunk too big: {}", c.len());
            // No line was split: every chunk line is one of the originals.
            for l in c.lines() {
                assert!(lines.iter().any(|orig| orig == l), "split line: {l}");
            }
        }
        // Rejoining the chunks reproduces the input exactly.
        assert_eq!(chunks.join("\n"), text);
    }

    #[test]
    fn single_short_input_is_one_chunk() {
        let chunks = chunk_by_lines("a\nb\nc", 1000);
        assert_eq!(chunks, vec!["a\nb\nc".to_string()]);
    }

    #[test]
    fn oversized_single_line_becomes_its_own_chunk() {
        let long = "y".repeat(500);
        let text = format!("short one\n{long}\nshort two");
        let chunks = chunk_by_lines(&text, 100);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[1], long);
        assert_eq!(chunks.join("\n"), text);
    }

    #[test]
    fn empty_input_gives_no_chunks() {
        assert!(chunk_by_lines("", 100).is_empty());
    }
}
