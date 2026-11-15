//! Long-transcript handling: transcripts under the token threshold get one
//! completion; above it, line-boundary chunks are summarized independently
//! (map) and the partial summaries merged under the template (reduce).

use auricle_core::Result;

use crate::chat::TRUNCATION_NOTICE;
use crate::LlmProvider;

/// Above this estimated token count the map-reduce path kicks in. Chosen
/// well under common 8k context windows, leaving room for the template and
/// the completion.
pub const MAP_REDUCE_TOKEN_THRESHOLD: usize = 6_000;

/// Map-phase chunk target, in estimated tokens.
const CHUNK_TOKENS: usize = 4_000;

/// Ollama runs a model in a 4,096-token context unless its server is set up
/// for more (`OLLAMA_CONTEXT_LENGTH`), and the OpenAI-compatible endpoint
/// used here can't ask for more per request. Past that it drops the oldest
/// tokens, the instructions first. So one call to it carries this much in
/// all, the template included, which leaves the answer about a thousand.
const OLLAMA_CALL_TOKENS: usize = 3_000;
const OLLAMA_CHUNK_TOKENS: usize = 2_000;
/// The least a call's transcript gets, however long the template runs.
const OLLAMA_MIN_TOKENS: usize = 500;
/// What a transcript line's `[MM:SS] Them:` costs beyond the count of its
/// letters: about eight tokens in fourteen characters, and a meeting of short
/// lines is mostly these.
const OLLAMA_LINE_TOKENS: usize = 4;

/// How much one call may carry, in estimated tokens: whole, per map portion.
struct Budget {
    threshold: usize,
    chunk: usize,
    /// Added for each line of what is measured.
    per_line: usize,
}

impl Budget {
    fn measure(&self, text: &str) -> usize {
        estimate_tokens(text) + self.per_line * text.lines().count()
    }
}

fn budget_for(provider: &dyn LlmProvider, template: &str) -> Budget {
    if provider.id() == "ollama" {
        let threshold = OLLAMA_CALL_TOKENS
            .saturating_sub(estimate_tokens(template))
            .max(OLLAMA_MIN_TOKENS);
        Budget {
            threshold,
            chunk: threshold.min(OLLAMA_CHUNK_TOKENS),
            per_line: OLLAMA_LINE_TOKENS,
        }
    } else {
        Budget {
            threshold: MAP_REDUCE_TOKEN_THRESHOLD,
            chunk: CHUNK_TOKENS,
            per_line: 0,
        }
    }
}

/// Rough token estimate: ~4 chars per token for English text, and about one
/// per character for Chinese, Japanese, Korean and Thai, where a character
/// is close to a word. Used only to size calls, so approximate is fine.
pub fn estimate_tokens(text: &str) -> usize {
    let (dense, other) = text.chars().fold((0usize, 0usize), |(dense, other), c| {
        if is_dense(c) {
            (dense + 1, other)
        } else {
            (dense, other + 1)
        }
    });
    dense + other.div_ceil(4)
}

/// Han, kana, Hangul and Thai: scripts with about one token a character.
fn is_dense(c: char) -> bool {
    matches!(c,
        '\u{0E00}'..='\u{0E7F}'
        | '\u{1100}'..='\u{11FF}'
        | '\u{3040}'..='\u{30FF}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{AC00}'..='\u{D7AF}'
        | '\u{F900}'..='\u{FAFF}')
}

/// Split `text` into chunks of at most `max_chars`, only ever cutting at
/// line boundaries (transcript lines are the atomic unit). A single line
/// longer than `max_chars` becomes its own oversized chunk rather than
/// being split mid-line.
pub fn chunk_by_lines(text: &str, max_chars: usize) -> Vec<String> {
    chunk_lines(text, max_chars, |s| s.chars().count())
}

fn chunk_lines(text: &str, max: usize, measure: impl Fn(&str) -> usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if current.is_empty() {
            current.push_str(line);
            continue;
        }
        // Measured joined, newline and all: an estimate rounds, so the
        // parts' estimates don't add up to the whole's.
        let mut joined = current.clone();
        joined.push('\n');
        joined.push_str(line);
        if measure(&joined) > max {
            chunks.push(std::mem::replace(&mut current, line.to_string()));
        } else {
            current = joined;
        }
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
    let budget = budget_for(provider, template);
    if budget.measure(transcript) <= budget.threshold {
        return provider.complete_patiently(template, transcript).await;
    }

    let chunks = chunk_lines(transcript, budget.chunk, |text| budget.measure(text));
    let total = chunks.len();
    let map_system = "You summarize one portion of a longer meeting transcript. \
         Produce a faithful, detailed summary of THIS PORTION ONLY, keeping \
         speaker attributions (You/Them), numbers, names, decisions, and \
         action items. Do not speculate about the rest of the meeting.";
    let mut partials = Vec::with_capacity(total);
    let mut failure = None;
    for (i, chunk) in chunks.iter().enumerate() {
        let user = format!("Portion {} of {total}:\n\n{chunk}", i + 1);
        match provider.complete_patiently(map_system, &user).await {
            Ok(partial) => partials.push(partial),
            Err(e) if partials.is_empty() => return Err(e),
            // On a slow local model what finished can be hours of work, and
            // whatever stopped this portion would likely stop the next.
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }

    // The reduce call rewrites everything it sees, so a notice on a portion
    // may not survive it. Carry the fact across explicitly.
    let mut truncated = partials.iter().any(|p| is_truncated(p));
    let mut summary = match reduce(
        provider,
        template,
        partials.clone(),
        budget.threshold,
        &mut truncated,
    )
    .await
    {
        Ok(summary) => summary,
        Err(e) => {
            failure.get_or_insert(e);
            partials.join("\n\n")
        }
    };
    if truncated && !is_truncated(&summary) {
        summary = format!("{summary}\n\n{TRUNCATION_NOTICE}");
    }
    if let Some(e) = failure {
        let what = match total - partials.len() {
            0 => "the portions above could not be merged".to_string(),
            missing => format!("{missing} of {total} portions were not summarized"),
        };
        summary = format!("{summary}\n\n[Incomplete: {what}: {e}]");
    }
    Ok(summary)
}

/// One summary of `partials` under `template`.
async fn reduce(
    provider: &dyn LlmProvider,
    template: &str,
    partials: Vec<String>,
    threshold: usize,
    truncated: &mut bool,
) -> Result<String> {
    let partials = collapse_to_budget(provider, partials, threshold, truncated).await?;

    let total = partials.len();
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
    provider
        .complete_patiently(&reduce_system, &reduce_input)
        .await
}

fn is_truncated(text: &str) -> bool {
    text.ends_with(TRUNCATION_NOTICE)
}

/// Merge neighboring partials until they all fit one reduce call, however
/// long the meeting ran. Every pass shrinks the list, so this terminates.
/// Sets `truncated` if a merge is cut off: the next pass would rewrite it,
/// and its notice with it.
async fn collapse_to_budget(
    provider: &dyn LlmProvider,
    mut partials: Vec<String>,
    threshold: usize,
    truncated: &mut bool,
) -> Result<Vec<String>> {
    let merge_system = "The following are faithful summaries of consecutive parts of one \
         meeting. Merge them into one faithful, detailed summary, keeping speaker \
         attributions (You/Them), numbers, names, decisions, and action items, in order.";
    while partials.len() > 1 && estimate_tokens(&partials.join("\n\n")) > threshold {
        let mut merged = Vec::new();
        for group in merge_groups(&partials, threshold) {
            merged.push(match group {
                [only] => only.clone(),
                _ => {
                    let merge = provider
                        .complete_patiently(merge_system, &group.join("\n\n"))
                        .await?;
                    *truncated |= is_truncated(&merge);
                    merge
                }
            });
        }
        partials = merged;
    }
    Ok(partials)
}

/// Consecutive runs of partials that fit the budget together. A run is always
/// at least two wide when it can be, even over budget, or a list of large
/// partials would never shrink.
fn merge_groups(partials: &[String], threshold: usize) -> Vec<&[String]> {
    let mut groups = Vec::new();
    let mut start = 0;
    let mut tokens = 0;
    for (i, partial) in partials.iter().enumerate() {
        let partial_tokens = estimate_tokens(partial);
        if i - start >= 2 && tokens + partial_tokens > threshold {
            groups.push(&partials[start..i]);
            start = i;
            tokens = 0;
        }
        tokens += partial_tokens;
    }
    groups.push(&partials[start..]);
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn token_estimate_is_chars_over_four() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens(&"x".repeat(24_000)), 6_000);
    }

    #[test]
    fn a_character_is_about_a_token_where_words_have_no_spaces() {
        assert_eq!(estimate_tokens("我们明天开会"), 6);
        assert_eq!(estimate_tokens("会議を始めます"), 7);
        assert_eq!(estimate_tokens("회의를 시작합니다"), 9);
        assert_eq!(estimate_tokens("ok 好的"), 3);
    }

    /// [`Fake`], as Ollama.
    struct Ollama<F>(F);

    #[async_trait::async_trait]
    impl<F> LlmProvider for Ollama<F>
    where
        F: Fn(&str, &str) -> Result<String> + Send + Sync,
    {
        fn id(&self) -> &str {
            "ollama"
        }
        fn model(&self) -> &str {
            "m"
        }
        async fn complete(&self, system: &str, user: &str) -> Result<String> {
            (self.0)(system, user)
        }
    }

    #[tokio::test]
    async fn every_call_to_ollama_fits_its_default_context() {
        // About 4,000 tokens: one call to anyone else, past 4k for Ollama
        // once the template and the answer are in.
        let text = vec!["x".repeat(399); 40].join("\n");
        let largest = std::sync::atomic::AtomicUsize::new(0);
        let provider = Ollama(|system: &str, user: &str| {
            let sent = estimate_tokens(system) + estimate_tokens(user);
            largest.fetch_max(sent, Ordering::SeqCst);
            Ok("short summary".to_string())
        });
        summarize(&provider, "make minutes", &text).await.unwrap();
        let largest = largest.load(Ordering::SeqCst);
        assert!(
            largest <= 3_000,
            "a call of {largest} tokens leaves no room"
        );
    }

    /// The largest call `summarize` makes to Ollama, system prompt and all,
    /// with each line's `[MM:SS] Them:` counted at what it costs.
    async fn largest_ollama_call(template: &str, transcript: &str) -> usize {
        let largest = std::sync::atomic::AtomicUsize::new(0);
        let provider = Ollama(|system: &str, user: &str| {
            let sent = estimate_tokens(system) + estimate_tokens(user) + 4 * user.lines().count();
            largest.fetch_max(sent, Ordering::SeqCst);
            Ok("short summary".to_string())
        });
        summarize(&provider, template, transcript).await.unwrap();
        largest.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn a_long_template_comes_out_of_what_ollama_is_sent() {
        // 2,400 tokens of transcript fits one call, but not beside a
        // template of 2,000: a custom one can run that long.
        let transcript = vec!["x".repeat(399); 24].join("\n");
        let template = "t".repeat(8_000);
        let largest = largest_ollama_call(&template, &transcript).await;
        assert!(
            largest <= 3_100,
            "a call of {largest} tokens leaves no room"
        );
    }

    #[tokio::test]
    async fn short_lines_are_counted_with_their_timestamps() {
        // 500 two-word lines: 2,125 tokens by their letters, about twice
        // that once every `[00:01] Them:` is paid for.
        let transcript = vec!["[00:01] Them: ok"; 500].join("\n");
        let largest = largest_ollama_call("make minutes", &transcript).await;
        assert!(
            largest <= 3_100,
            "a call of {largest} tokens leaves no room"
        );
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

    fn partials(tokens: &[usize]) -> Vec<String> {
        tokens.iter().map(|t| "x".repeat(t * 4)).collect()
    }

    #[test]
    fn merge_groups_fill_the_budget_and_keep_order() {
        let list = partials(&[2_000, 2_000, 2_000, 2_000, 2_000]);
        let widths: Vec<_> = merge_groups(&list, MAP_REDUCE_TOKEN_THRESHOLD)
            .iter()
            .map(|g| g.len())
            .collect();
        assert_eq!(widths, vec![3, 2]);
    }

    /// Answers with a closure of (system, user).
    struct Fake<F>(F);

    #[async_trait::async_trait]
    impl<F> LlmProvider for Fake<F>
    where
        F: Fn(&str, &str) -> Result<String> + Send + Sync,
    {
        fn id(&self) -> &str {
            "fake"
        }
        fn model(&self) -> &str {
            "m"
        }
        async fn complete(&self, system: &str, user: &str) -> Result<String> {
            (self.0)(system, user)
        }
    }

    fn is_map(system: &str) -> bool {
        system.contains("THIS PORTION ONLY")
    }

    fn is_merge(system: &str) -> bool {
        system.contains("Merge them into one")
    }

    fn failed() -> Result<String> {
        Err(auricle_core::Error::Config("server went away".to_string()))
    }

    /// A transcript that splits into exactly `portions` map calls.
    fn transcript(portions: usize) -> String {
        vec!["x".repeat(999); 16 * portions].join("\n")
    }

    fn portion_of(user: &str) -> usize {
        let number = user.strip_prefix("Portion ").unwrap();
        number.split(' ').next().unwrap().parse().unwrap()
    }

    #[tokio::test]
    async fn a_failed_portion_keeps_the_ones_that_finished() {
        let calls = AtomicU32::new(0);
        let provider = Fake(|system: &str, user: &str| {
            calls.fetch_add(1, Ordering::SeqCst);
            if !is_map(system) {
                assert!(user.contains("summary 2") && !user.contains("summary 3"));
                return Ok("merged".to_string());
            }
            match portion_of(user) {
                3 => failed(),
                n => Ok(format!("summary {n}")),
            }
        });

        let summary = summarize(&provider, "t", &transcript(4)).await.unwrap();

        assert!(summary.starts_with("merged"), "{summary}");
        assert!(summary.contains("2 of 4 portions"), "{summary}");
        assert!(summary.contains("server went away"), "{summary}");
        assert_eq!(calls.load(Ordering::SeqCst), 4, "3 map + 1 reduce");
    }

    #[tokio::test]
    async fn a_first_portion_that_fails_is_an_error() {
        let provider = Fake(|_: &str, _: &str| failed());
        let err = summarize(&provider, "t", &transcript(2)).await.unwrap_err();
        assert!(err.to_string().contains("server went away"), "{err}");
    }

    #[tokio::test]
    async fn a_failed_merge_keeps_the_finished_portions() {
        let provider = Fake(|system: &str, user: &str| {
            if is_map(system) {
                Ok(format!("summary {}", portion_of(user)))
            } else {
                failed()
            }
        });

        let summary = summarize(&provider, "t", &transcript(2)).await.unwrap();

        assert!(summary.contains("summary 1"), "{summary}");
        assert!(summary.contains("summary 2"), "{summary}");
        assert!(summary.contains("could not be merged"), "{summary}");
    }

    #[tokio::test]
    async fn a_server_gone_for_good_still_leaves_what_finished() {
        let provider = Fake(|system: &str, user: &str| {
            if is_map(system) && portion_of(user) == 1 {
                Ok("summary 1".to_string())
            } else {
                failed()
            }
        });

        let summary = summarize(&provider, "t", &transcript(3)).await.unwrap();

        assert!(summary.starts_with("summary 1"), "{summary}");
        assert!(summary.contains("2 of 3 portions"), "{summary}");
    }

    #[tokio::test]
    async fn a_cut_off_merge_is_remembered_after_a_later_merge_rewrites_it() {
        let merges = AtomicU32::new(0);
        let provider = Fake(|system: &str, _: &str| {
            let long = "x".repeat(MAP_REDUCE_TOKEN_THRESHOLD * 2);
            if is_map(system) {
                Ok(long)
            } else if !is_merge(system) {
                Ok("the summary".to_string())
            } else if merges.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(format!("{long}\n\n{TRUNCATION_NOTICE}"))
            } else {
                Ok(long)
            }
        });

        let summary = summarize(&provider, "t", &transcript(8)).await.unwrap();

        assert!(merges.load(Ordering::SeqCst) > 4, "needs a second pass");
        assert!(summary.ends_with(TRUNCATION_NOTICE), "{summary}");
    }

    #[test]
    fn oversized_partials_still_pair_up_so_every_pass_shrinks() {
        for n in 2..12 {
            let list = partials(&vec![MAP_REDUCE_TOKEN_THRESHOLD * 2; n]);
            assert!(
                merge_groups(&list, MAP_REDUCE_TOKEN_THRESHOLD).len() < n,
                "{n} partials did not shrink"
            );
        }
    }
}
