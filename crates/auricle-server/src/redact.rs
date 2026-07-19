//! PII / secret redaction for transcript text.
//!
//! When the `redact_pii` setting is on, final and partial transcript text is
//! scrubbed at the single fan-out point in the engine — before it is
//! persisted, shown, or sent to any cloud LLM (summaries and asks read the
//! same segments). Matches become a visible label (e.g. `[email]`) so the
//! redaction is obvious, never silent.
//!
//! Scope is transcript text. Screen-OCR and typed ask questions are not
//! redacted here (a possible follow-up); they are on-demand, user-initiated
//! inputs, not the always-on capture stream.

use std::sync::LazyLock;

use regex::Regex;

struct Pattern {
    label: &'static str,
    re: Regex,
    /// Optional validator to cut false positives (e.g. Luhn for cards).
    valid: Option<fn(&str) -> bool>,
}

// Applied in order; once a span is replaced with its label, later patterns
// see the label text and won't re-match. More specific / higher-confidence
// patterns run before broad ones (phone is the most false-positive-prone, so
// it runs last).
static PATTERNS: LazyLock<Vec<Pattern>> = LazyLock::new(|| {
    let p = |label, re: &str, valid| Pattern {
        label,
        re: Regex::new(re).expect("valid redaction pattern"),
        valid,
    };
    vec![
        p(
            "[email]",
            r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b",
            None,
        ),
        // AWS access key id and OpenAI-style secret keys.
        p("[secret]", r"\bAKIA[0-9A-Z]{16}\b", None),
        p("[secret]", r"\bsk-[A-Za-z0-9]{20,}\b", None),
        p("[ssn]", r"\b\d{3}-\d{2}-\d{4}\b", None),
        // 13–19 digits ending on a digit (so a trailing separator isn't
        // swallowed), Luhn-gated to reject ordinary long numbers.
        p(
            "[card]",
            r"\b\d(?:[ -]?\d){12,18}\b",
            Some(luhn_ok as fn(&str) -> bool),
        ),
        p(
            "[ip]",
            r"\b(?:\d{1,3}\.){3}\d{1,3}\b",
            Some(ipv4_ok as fn(&str) -> bool),
        ),
        // Separators are mandatory so a bare digit run isn't mistaken for a
        // phone number (the most false-positive-prone pattern).
        p(
            "[phone]",
            r"\b(?:\+?\d{1,3}[ .-])?\(?\d{3}\)?[ .-]\d{3}[ .-]\d{4}\b",
            None,
        ),
    ]
});

/// Replace emails, card/SSN numbers, IPs, phone numbers, and API keys in
/// `text` with visible `[label]` placeholders.
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for p in PATTERNS.iter() {
        out =
            p.re.replace_all(&out, |caps: &regex::Captures| {
                let m = &caps[0];
                match p.valid {
                    Some(v) if !v(m) => m.to_string(), // failed validation: leave as-is
                    _ => p.label.to_string(),
                }
            })
            .into_owned();
    }
    out
}

/// Luhn checksum over the digits of `s` (13–19 of them), the standard
/// credit-card validity test — keeps plain long numbers from matching.
fn luhn_ok(s: &str) -> bool {
    let digits: Vec<u32> = s.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let mut sum = 0u32;
    for (i, &d) in digits.iter().rev().enumerate() {
        let mut d = d;
        if i % 2 == 1 {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
    }
    sum.is_multiple_of(10)
}

/// Every dotted octet is 0–255 — rejects version strings like `1.2.3.4444`.
fn ipv4_ok(s: &str) -> bool {
    s.split('.')
        .all(|o| o.parse::<u16>().map(|n| n <= 255).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_common_pii() {
        assert_eq!(redact("mail me at a.b+x@mail.co"), "mail me at [email]");
        assert_eq!(redact("ssn 123-45-6789 ok"), "ssn [ssn] ok");
        assert_eq!(redact("call 415-555-0132 now"), "call [phone] now");
        assert_eq!(redact("host 192.168.0.1 up"), "host [ip] up");
        // A Luhn-valid Visa test number.
        assert_eq!(redact("card 4111 1111 1111 1111 end"), "card [card] end");
        assert_eq!(redact("key AKIAIOSFODNN7EXAMPLE here"), "key [secret] here");
        assert!(redact("token sk-abcdefghij0123456789XYZ").contains("[secret]"));
    }

    #[test]
    fn leaves_ordinary_text_and_invalid_numbers_alone() {
        assert_eq!(
            redact("the meeting is at 3pm sharp"),
            "the meeting is at 3pm sharp"
        );
        // Not Luhn-valid → not a card.
        assert_eq!(
            redact("order 1234567890123 shipped"),
            "order 1234567890123 shipped"
        );
        // Octet out of range → not an IP.
        assert_eq!(redact("build 1.2.3.999 tag"), "build 1.2.3.999 tag");
    }

    #[test]
    fn empty_is_empty() {
        assert_eq!(redact(""), "");
    }
}
