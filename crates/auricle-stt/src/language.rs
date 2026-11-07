//! The session language: what a tag may look like, and how each provider
//! spells it. They disagree: Deepgram takes a full tag and has no detection
//! on streams, the OpenAI shape wants bare ISO 639-1 and detects when the
//! field is absent, and whisper.cpp wants a code it knows.

use auricle_core::{Error, Result};

/// Detect the language instead of assuming one.
const AUTO: &str = "auto";
/// Deepgram's word for it, which a config written for Deepgram may carry.
const MULTI: &str = "multi";
const MAX_TAG_LEN: usize = 16;

/// Accept "auto" or a short tag like "en" or "pt-BR", and nothing else: the
/// value lands in Deepgram's URL unescaped.
pub fn validate_language(tag: &str) -> Result<()> {
    let mut subtags = tag.split('-');
    let primary = subtags.next().unwrap_or_default();
    let valid = tag.len() <= MAX_TAG_LEN
        && primary.len() >= 2
        && primary.chars().all(|c| c.is_ascii_alphabetic())
        && subtags.all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric()));
    if valid {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "stt language \"{tag}\" is not \"auto\" or a language tag like \"en\" or \"pt-BR\""
        )))
    }
}

/// Check a session's language for `provider_id` before anything is opened.
pub fn check_language(provider_id: &str, language: &str) -> Result<()> {
    validate_language(language)?;
    if provider_id == "whisper-local" {
        whisper_language(language)?;
    }
    Ok(())
}

/// The warning for a language an English-only whisper model cannot honour.
pub fn english_only_warning(provider_id: &str, model: &str, language: &str) -> Option<String> {
    let ignored =
        provider_id == "whisper-local" && model.ends_with(".en") && primary(language) != "en";
    ignored.then(|| {
        format!(
            "whisper model {model} is English-only and ignores language \"{language}\"; \
             use the base or small model for other languages"
        )
    })
}

fn is_auto(tag: &str) -> bool {
    tag.eq_ignore_ascii_case(AUTO) || tag.eq_ignore_ascii_case(MULTI)
}

/// The language without its region: "pt-BR" is "pt".
fn primary(tag: &str) -> String {
    tag.split('-')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// The value for whisper.cpp. It has to be checked here: an unknown language
/// isn't an error there, it becomes a bogus token at the head of every prompt.
pub(crate) fn whisper_language(tag: &str) -> Result<String> {
    if is_auto(tag) {
        return Ok(AUTO.to_string());
    }
    let language = primary(tag);
    match whisper_rs::get_lang_id(&language) {
        Some(_) => Ok(language),
        None => Err(Error::Config(format!(
            "whisper-local does not support language \"{tag}\""
        ))),
    }
}

/// The `language` form field for the OpenAI shape, if any: left out, the
/// provider detects the language itself.
#[cfg(feature = "cloud")]
pub(crate) fn batch_language(tag: &str) -> Option<String> {
    (!is_auto(tag)).then(|| primary(tag))
}

/// The `language` query value for Deepgram. Its streaming API has no language
/// detection; `multi` is the mode that follows whatever is being spoken.
#[cfg(feature = "cloud")]
pub(crate) fn deepgram_language(tag: &str) -> &str {
    if is_auto(tag) {
        MULTI
    } else {
        tag
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_tags_and_auto_are_accepted() {
        for ok in ["en", "de", "pt-BR", "zh-Hant-TW", "yue", "auto", "multi"] {
            validate_language(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
    }

    #[test]
    fn anything_that_could_change_a_url_is_rejected() {
        for bad in [
            "",
            "e",
            "-",
            "en-",
            "-en",
            "en--US",
            "en us",
            "en&model=x",
            "en?a=1",
            "en#frag",
            "en/../x",
            "en%26model",
            "e1",
            "ènglish",
            "a-very-long-language-tag",
        ] {
            let error = validate_language(bad).unwrap_err().to_string();
            assert!(error.contains("not \"auto\""), "{bad}: {error}");
        }
    }

    #[test]
    fn whisper_gets_a_code_it_knows() {
        assert_eq!(whisper_language("de").unwrap(), "de");
        assert_eq!(whisper_language("pt-BR").unwrap(), "pt");
        assert_eq!(whisper_language("EN").unwrap(), "en");
        assert_eq!(whisper_language("auto").unwrap(), "auto");
        assert_eq!(whisper_language("multi").unwrap(), "auto");
    }

    #[test]
    fn a_language_whisper_lacks_is_refused_up_front_for_whisper_only() {
        let error = check_language("whisper-local", "xx").unwrap_err();
        assert!(error.to_string().contains("xx"), "{error}");
        check_language("deepgram", "xx").unwrap();
    }

    #[test]
    fn a_malformed_tag_is_refused_for_every_provider() {
        for provider in ["whisper-local", "deepgram", "groq-whisper"] {
            assert!(
                check_language(provider, "en&model=x").is_err(),
                "{provider}"
            );
        }
    }

    #[cfg(feature = "cloud")]
    #[test]
    fn the_batch_shape_gets_bare_iso_or_nothing() {
        assert_eq!(batch_language("en").as_deref(), Some("en"));
        assert_eq!(batch_language("pt-BR").as_deref(), Some("pt"));
        assert_eq!(batch_language("auto"), None);
        assert_eq!(batch_language("multi"), None);
    }

    #[cfg(feature = "cloud")]
    #[test]
    fn deepgram_keeps_the_full_tag_and_has_no_auto() {
        assert_eq!(deepgram_language("pt-BR"), "pt-BR");
        assert_eq!(deepgram_language("auto"), "multi");
        assert_eq!(deepgram_language("multi"), "multi");
    }

    #[test]
    fn only_a_non_english_language_on_an_english_model_warns() {
        let warning = english_only_warning("whisper-local", "base.en", "de").unwrap();
        assert!(warning.contains("base.en"), "{warning}");
        assert!(warning.contains("\"de\""), "{warning}");

        assert!(english_only_warning("whisper-local", "base.en", "auto").is_some());
        assert_eq!(english_only_warning("whisper-local", "base.en", "en"), None);
        assert_eq!(
            english_only_warning("whisper-local", "base.en", "en-GB"),
            None
        );
        assert_eq!(english_only_warning("whisper-local", "base", "de"), None);
        assert_eq!(english_only_warning("deepgram", "base.en", "de"), None);
    }
}
