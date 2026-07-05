use std::path::Path;

use serde::Deserialize;

use crate::error::{Error, Result};

/// The `[audio]` section of `auricle.toml`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Mic device: "default" or an exact device name from `auricle devices`.
    pub mic_device: String,
    /// Loopback device: "default" or an exact output device name.
    pub loopback_device: String,
    /// Keep per-channel raw WAVs alongside session data (privacy default: off).
    pub retain_raw_audio: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        AudioConfig {
            mic_device: "default".to_string(),
            loopback_device: "default".to_string(),
            retain_raw_audio: false,
        }
    }
}

/// Root configuration. Phase 1 only understands `[audio]`; unknown sections
/// (e.g. `[stt]`, `[llm]`, `[server]`) are ignored so a full `auricle.toml`
/// still parses.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
    pub audio: AudioConfig,
}

impl Config {
    /// Load configuration from a TOML file.
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        Config::parse(&text)
    }

    /// Parse configuration from a TOML string.
    pub fn parse(text: &str) -> Result<Config> {
        toml::from_str(text).map_err(|e| Error::Config(format!("invalid TOML: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_empty() {
        let cfg = Config::parse("").unwrap();
        assert_eq!(cfg.audio.mic_device, "default");
        assert_eq!(cfg.audio.loopback_device, "default");
        assert!(!cfg.audio.retain_raw_audio);
    }

    #[test]
    fn partial_audio_section_fills_defaults() {
        let cfg = Config::parse("[audio]\nmic_device = \"Microphone (USB)\"\n").unwrap();
        assert_eq!(cfg.audio.mic_device, "Microphone (USB)");
        assert_eq!(cfg.audio.loopback_device, "default");
        assert!(!cfg.audio.retain_raw_audio);
    }

    #[test]
    fn full_audio_section() {
        let text = r#"
[audio]
mic_device = "Mic A"
loopback_device = "Speakers B"
retain_raw_audio = true
"#;
        let cfg = Config::parse(text).unwrap();
        assert_eq!(cfg.audio.mic_device, "Mic A");
        assert_eq!(cfg.audio.loopback_device, "Speakers B");
        assert!(cfg.audio.retain_raw_audio);
    }

    #[test]
    fn unknown_sections_are_ignored() {
        // A future full auricle.toml must not break Phase 1 parsing.
        let text = r#"
[audio]
mic_device = "Mic A"

[stt]
provider = "whisper-local"

[server]
bind = "127.0.0.1:4820"
"#;
        let cfg = Config::parse(text).unwrap();
        assert_eq!(cfg.audio.mic_device, "Mic A");
    }

    #[test]
    fn invalid_toml_is_a_clean_error() {
        let err = Config::parse("[audio\nmic_device=").unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn wrong_type_is_a_clean_error() {
        let err = Config::parse("[audio]\nretain_raw_audio = \"yes\"").unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }
}
