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
        // AWS access key id, `sk-` keys (including the segmented
        // `sk-proj-...` shape), and Groq keys.
        p("[secret]", r"\bAKIA[0-9A-Z]{16}\b", None),
        p("[secret]", r"\bsk-[A-Za-z0-9_-]{20,}\b", None),
        p("[secret]", r"\bgsk_[A-Za-z0-9]{20,}\b", None),
        p(
            "[ssn]",
            r"\b(?:\d{3}-\d{2}-\d{4}|\d{3} \d{2} \d{4})\b",
            None,
        ),
        // 13 digits or more, ending on a digit (so a trailing separator isn't
        // swallowed), Luhn-gated to reject ordinary long numbers. No upper
        // bound: digits said before a card would otherwise fill the match,
        // which then fails the check, and the card after them goes free.
        p(
            "[card]",
            r"\b\d(?:[ -]?\d){12,}\b",
            Some(card_ok as fn(&str) -> bool),
        ),
        // Speech to text also writes a card read out in groups with commas
        // or dots, which lists of numbers use too, so only card groupings:
        // four fours, and the 4-6-5 Amex uses.
        p(
            "[card]",
            r"\b(?:\d{4}(?:, ?|\.)\d{4}(?:, ?|\.)\d{4}(?:, ?|\.)\d{4}|\d{4}(?:, ?|\.)\d{6}(?:, ?|\.)\d{5})\b",
            Some(listed_card_ok as fn(&str) -> bool),
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
        // Any other country's grouping, when said with its `+` country code:
        // 8 to 15 digits in all, the international length.
        p(
            "[phone]",
            r"\+\d{1,3}(?:[ .-]\(?\d{1,5}\)?){1,5}\b",
            Some(phone_ok as fn(&str) -> bool),
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

/// True when a card-shaped span holds a card. The match is greedy, so digits
/// said right next to a card (its CVV, a room number) widen it, and a Luhn
/// check of the whole span would wave the card through. Test every run of
/// whole digit groups instead; a span with no separators is one group, so an
/// ordinary long number still gets exactly one check.
fn card_ok(s: &str) -> bool {
    let groups: Vec<&str> = s.split([' ', '-']).collect();
    (0..groups.len())
        .any(|start| (start + 1..=groups.len()).any(|end| luhn_ok(&groups[start..end].concat())))
}

/// True when a number written as a list, with commas or dots, is a card. One
/// list of four numbers in ten passes the checksum by chance, and lists are
/// everywhere in speech, so this also wants the digits a card network starts
/// with: 3 to 6, or Mastercard's 2221 to 2720. Years and most counts start
/// otherwise.
fn listed_card_ok(s: &str) -> bool {
    let digits: String = s.chars().filter(char::is_ascii_digit).collect();
    let issued = matches!(digits.as_bytes().first(), Some(b'3'..=b'6'))
        || digits
            .get(..4)
            .and_then(|prefix| prefix.parse::<u16>().ok())
            .is_some_and(|prefix| (2221..=2720).contains(&prefix));
    issued && luhn_ok(&digits)
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

/// An international number has 8 to 15 digits, country code included.
fn phone_ok(s: &str) -> bool {
    (8..=15).contains(&s.chars().filter(char::is_ascii_digit).count())
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
        // Fake keys are split in two so secret scanners don't flag the source;
        // the redactor still sees the joined string.
        assert_eq!(
            redact(concat!("key AKIA", "IOSFODNN7EXAMPLE here")),
            "key [secret] here"
        );
        assert!(redact("token sk-abcdefghij0123456789XYZ").contains("[secret]"));
    }

    #[test]
    fn redacts_current_llm_key_shapes() {
        assert_eq!(
            redact("key sk-proj-Ab3_dE-fGh1jKlMnOpQrStUvWxYz0123 here"),
            "key [secret] here"
        );
        assert_eq!(
            redact(concat!(
                "key sk-",
                "ant-api03-Ab3dEfGh1jKlMnOp_QrStUvWx-yz here"
            )),
            "key [secret] here"
        );
        assert_eq!(
            redact("key gsk_Ab3dEfGh1jKlMnOpQrStUvWxYz0123456789abcd here"),
            "key [secret] here"
        );
        // A hyphenated word that merely contains the prefix is not a key.
        assert_eq!(
            redact("the task-force-for-quarterly-planning-review met"),
            "the task-force-for-quarterly-planning-review met"
        );
    }

    #[test]
    fn digits_next_to_a_card_do_not_hide_it() {
        // Greedy matching takes all 19 digits: its CVV, read straight after.
        assert_eq!(redact("card 4111 1111 1111 1111 123"), "card [card]");
        assert_eq!(redact("card 4111-1111-1111-1111-123 ok"), "card [card] ok");
        // Leading digits, and the 4-6-5 grouping Amex uses.
        assert_eq!(redact("room 12 4111 1111 1111 1111"), "room [card]");
        assert_eq!(redact("amex 3782 822463 10005 123"), "amex [card]");
        // Still Luhn-gated: same shape, no valid card inside it.
        assert_eq!(
            redact("ref 1234 5678 9012 3457 123"),
            "ref 1234 5678 9012 3457 123"
        );
    }

    #[test]
    fn redacts_pii_as_speech_to_text_writes_it() {
        assert_eq!(redact("card 4111, 1111, 1111, 1111 ok"), "card [card] ok");
        assert_eq!(redact("card 4111.1111.1111.1111"), "card [card]");
        assert_eq!(redact("amex 3782, 822463, 10005"), "amex [card]");
        // Four digits said first used to fill the match and hide the card.
        assert_eq!(redact("pin 1234 4111 1111 1111 1111"), "pin [card]");
        assert_eq!(redact("ssn 123 45 6789 ok"), "ssn [ssn] ok");
        assert_eq!(redact("call +44 20 7946 0958 now"), "call [phone] now");
        assert_eq!(redact("call +91 98765 43210"), "call [phone]");
    }

    #[test]
    fn number_lists_and_scores_are_not_pii() {
        assert_eq!(
            redact("we sold 1,200, 3,400 and 5,600 units"),
            "we sold 1,200, 3,400 and 5,600 units"
        );
        assert_eq!(
            redact("rows 1234, 5678, 9012, 3456"),
            "rows 1234, 5678, 9012, 3456"
        );
        assert_eq!(redact("version 1.2.3 shipped"), "version 1.2.3 shipped");
        assert_eq!(
            redact("up +3 points, +12 percent"),
            "up +3 points, +12 percent"
        );
    }

    /// One list of four numbers in ten passes the card checksum by chance,
    /// and these years do. No card number starts the way a year does.
    #[test]
    fn a_list_of_years_is_not_a_card() {
        assert!(
            luhn_ok("2023202420252026"),
            "precondition: the checksum passes"
        );
        assert_eq!(
            redact("revenue in 2023, 2024, 2025, 2026 grew"),
            "revenue in 2023, 2024, 2025, 2026 grew"
        );
        assert!(
            luhn_ok("1989199019911992"),
            "precondition: the checksum passes"
        );
        assert_eq!(
            redact("born 1989. 1990. 1991. 1992"),
            "born 1989. 1990. 1991. 1992"
        );
        // A card said the same way still goes: Visa, Mastercard, Amex.
        assert_eq!(redact("card 4111, 1111, 1111, 1111"), "card [card]");
        assert_eq!(redact("card 5555, 5555, 5555, 4444"), "card [card]");
        assert_eq!(redact("card 2223, 0031, 2200, 3222"), "card [card]");
        assert_eq!(redact("amex 3782, 822463, 10005"), "amex [card]");
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
