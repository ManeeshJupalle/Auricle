//! Markdown export shared by `GET /sessions/:id/export?format=md` and
//! `auricle export <id> --md`.

use crate::store::{SegmentRow, SessionRow, SummaryRow};

/// Render a session as clean markdown: title, date, duration, provider,
/// the speaker-labeled transcript with timestamps, then any summaries.
pub fn render_markdown_full(
    session: &SessionRow,
    segments: &[SegmentRow],
    summaries: &[SummaryRow],
) -> String {
    let mut out = render_markdown(session, segments);
    for s in summaries {
        out.push_str(&format!(
            "## Summary — {} ({})\n\n{}\n\n",
            s.template,
            s.model,
            s.content.trim()
        ));
    }
    out
}

/// Transcript-only markdown (title, metadata, speaker-labeled lines).
pub fn render_markdown(session: &SessionRow, segments: &[SegmentRow]) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", session.title));
    out.push_str(&format!("- session: `{}`\n", session.id));
    out.push_str(&format!("- date: {}\n", format_utc(session.started_at)));
    let duration = session
        .ended_at
        .map(|e| format_duration(e.saturating_sub(session.started_at)))
        .unwrap_or_else(|| "in progress".to_string());
    out.push_str(&format!("- duration: {duration}\n"));
    out.push_str(&format!("- stt provider: {}\n", session.stt_provider));
    if session.meta.get("interrupted").is_some() {
        out.push_str("- note: session was interrupted (not stopped cleanly)\n");
    }
    out.push_str("\n## Transcript\n\n");
    if segments.is_empty() {
        out.push_str("_(no speech transcribed)_\n");
    }
    for seg in segments {
        out.push_str(&format!(
            "**[{}] {}:** {}\n\n",
            format_ts_ms(seg.t_start_ms),
            seg.speaker,
            seg.text
        ));
    }
    out
}

/// Plain-text transcript used as LLM input: `[MM:SS] Speaker: text` lines.
pub fn render_transcript_text(segments: &[SegmentRow]) -> String {
    segments
        .iter()
        .map(|seg| {
            format!(
                "[{}] {}: {}",
                format_ts_ms(seg.t_start_ms),
                seg.speaker,
                seg.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_ts_ms(ms: i64) -> String {
    let ms = ms.max(0) as u64;
    format!("{:02}:{:02}", ms / 60_000, (ms / 1000) % 60)
}

fn format_duration(secs: i64) -> String {
    let secs = secs.max(0);
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// Unix seconds -> "YYYY-MM-DD HH:MM UTC" without a date-time dependency
/// (civil-from-days algorithm, Howard Hinnant's classic).
fn format_utc(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let secs_of_day = unix_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_session() -> SessionRow {
        SessionRow {
            id: "rec-1751700000".into(),
            title: "Weekly sync".into(),
            started_at: 1_751_700_000, // 2025-07-05 07:20 UTC
            ended_at: Some(1_751_700_125),
            stt_provider: "deepgram".into(),
            meta: serde_json::json!({}),
        }
    }

    #[test]
    fn golden_markdown_export() {
        let segments = vec![
            SegmentRow {
                channel: 1,
                speaker: "Them".into(),
                t_start_ms: 1_200,
                t_end_ms: 4_000,
                text: "Shall we start with the pipeline?".into(),
                provider: "deepgram".into(),
            },
            SegmentRow {
                channel: 0,
                speaker: "You".into(),
                t_start_ms: 64_500,
                t_end_ms: 66_000,
                text: "Yes, latency numbers first.".into(),
                provider: "deepgram".into(),
            },
        ];
        let md = render_markdown(&sample_session(), &segments);
        let expected = "\
# Weekly sync

- session: `rec-1751700000`
- date: 2025-07-05 07:20 UTC
- duration: 02:05
- stt provider: deepgram

## Transcript

**[00:01] Them:** Shall we start with the pipeline?

**[01:04] You:** Yes, latency numbers first.

";
        assert_eq!(md, expected);
    }

    #[test]
    fn empty_transcript_and_open_session() {
        let mut session = sample_session();
        session.ended_at = None;
        let md = render_markdown(&session, &[]);
        assert!(md.contains("- duration: in progress"));
        assert!(md.contains("_(no speech transcribed)_"));
    }

    #[test]
    fn summaries_are_appended_after_the_transcript() {
        let summaries = vec![SummaryRow {
            id: 1,
            template: "minutes".into(),
            model: "llama-3.3-70b-versatile".into(),
            content: "- decided to ship Friday\n".into(),
            created_at: 0,
        }];
        let md = render_markdown_full(&sample_session(), &[], &summaries);
        let idx_t = md.find("## Transcript").unwrap();
        let idx_s = md
            .find("## Summary — minutes (llama-3.3-70b-versatile)")
            .unwrap();
        assert!(idx_s > idx_t);
        assert!(md.contains("ship Friday"));
    }

    #[test]
    fn transcript_text_lines_for_llm_input() {
        let segments = vec![SegmentRow {
            channel: 1,
            speaker: "Them".into(),
            t_start_ms: 61_000,
            t_end_ms: 62_000,
            text: "ship it".into(),
            provider: "x".into(),
        }];
        assert_eq!(render_transcript_text(&segments), "[01:01] Them: ship it");
    }

    #[test]
    fn interrupted_flag_is_noted() {
        let mut session = sample_session();
        session.meta = serde_json::json!({"interrupted": true});
        let md = render_markdown(&session, &[]);
        assert!(md.contains("interrupted"));
    }

    #[test]
    fn civil_date_conversion() {
        assert_eq!(format_utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_utc(86_399), "1970-01-01 23:59 UTC");
        assert_eq!(format_utc(951_827_696), "2000-02-29 12:34 UTC");
    }
}
