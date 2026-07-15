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

/// The `[stt]` section: default provider and per-provider settings.
/// API keys are named by env var only — never stored in config or on disk.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct SttConfig {
    /// Default provider id: "whisper-local", "deepgram", "groq-whisper",
    /// "openai-compat".
    pub provider: String,
    #[serde(rename = "whisper-local")]
    pub whisper_local: WhisperLocalConfig,
    pub deepgram: DeepgramConfig,
    #[serde(rename = "groq-whisper")]
    pub groq_whisper: GroqWhisperConfig,
    #[serde(rename = "openai-compat")]
    pub openai_compat: OpenAiCompatConfig,
}

impl Default for SttConfig {
    fn default() -> Self {
        SttConfig {
            provider: "whisper-local".to_string(),
            whisper_local: WhisperLocalConfig::default(),
            deepgram: DeepgramConfig::default(),
            groq_whisper: GroqWhisperConfig::default(),
            openai_compat: OpenAiCompatConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct WhisperLocalConfig {
    pub model: String,
}

impl Default for WhisperLocalConfig {
    fn default() -> Self {
        WhisperLocalConfig {
            // base.en keeps up with real time on ordinary laptop CPUs;
            // small.en measurably cannot (benches/RESULTS.md, Phase 2
            // report) and live sessions degrade into shedding. Users who
            // want small.en's accuracy for offline-ish use opt in via
            // config or the Settings view.
            model: "base.en".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct DeepgramConfig {
    pub api_key_env: String,
    pub model: String,
}

impl Default for DeepgramConfig {
    fn default() -> Self {
        DeepgramConfig {
            api_key_env: "DEEPGRAM_API_KEY".to_string(),
            model: "nova-3".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct GroqWhisperConfig {
    pub api_key_env: String,
    pub model: String,
    pub base_url: String,
}

impl Default for GroqWhisperConfig {
    fn default() -> Self {
        GroqWhisperConfig {
            api_key_env: "GROQ_API_KEY".to_string(),
            model: "whisper-large-v3-turbo".to_string(),
            base_url: "https://api.groq.com/openai/v1".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct OpenAiCompatConfig {
    pub api_key_env: String,
    pub model: String,
    pub base_url: String,
}

impl Default for OpenAiCompatConfig {
    fn default() -> Self {
        OpenAiCompatConfig {
            api_key_env: "OPENAI_API_KEY".to_string(),
            model: "whisper-1".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
        }
    }
}

/// The `[llm]` section: default LLM provider for summaries and
/// per-provider settings. One OpenAI-compatible client covers all of them
/// (configurable base_url); keys via env only.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    /// Default provider id: "ollama", "groq", "openai-compat".
    pub provider: String,
    pub ollama: OllamaConfig,
    pub groq: GroqLlmConfig,
    #[serde(rename = "openai-compat")]
    pub openai_compat: OpenAiLlmConfig,
}

impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig {
            provider: "ollama".to_string(),
            ollama: OllamaConfig::default(),
            groq: GroqLlmConfig::default(),
            openai_compat: OpenAiLlmConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct OllamaConfig {
    pub base_url: String,
    pub model: String,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        OllamaConfig {
            base_url: "http://localhost:11434/v1".to_string(),
            model: "llama3.1".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct GroqLlmConfig {
    pub api_key_env: String,
    pub base_url: String,
    pub model: String,
}

impl Default for GroqLlmConfig {
    fn default() -> Self {
        GroqLlmConfig {
            api_key_env: "GROQ_API_KEY".to_string(),
            base_url: "https://api.groq.com/openai/v1".to_string(),
            model: "llama-3.3-70b-versatile".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct OpenAiLlmConfig {
    pub api_key_env: String,
    pub base_url: String,
    pub model: String,
}

impl Default for OpenAiLlmConfig {
    fn default() -> Self {
        OpenAiLlmConfig {
            api_key_env: "OPENAI_API_KEY".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o-mini".to_string(),
        }
    }
}

/// The `[copilot]` section: the screen-aware assistant layer
/// (COPILOT_ARCHITECTURE §3). Hotkeys are consumed by the Phase 9 overlay;
/// they live here so one config section covers the whole copilot.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct CopilotConfig {
    /// Rolling in-memory transcript window fed to asks, in minutes.
    pub transcript_window_min: u64,
    /// Screen OCR text is truncated to this many characters before it
    /// enters the prompt.
    pub max_screen_chars: usize,
    /// Persist asks (question + screen context + answer) to the database.
    /// Default OFF: nothing screen- or question-derived is stored.
    pub retain_context: bool,
    pub hotkey_summon: String,
    pub hotkey_quick: String,
    /// LLM provider for asks; None falls back to `[llm].provider`.
    pub provider: Option<String>,
}

impl Default for CopilotConfig {
    fn default() -> Self {
        CopilotConfig {
            transcript_window_min: 10,
            max_screen_chars: 6_000,
            retain_context: false,
            hotkey_summon: "Ctrl+Shift+Space".to_string(),
            hotkey_quick: "Ctrl+Shift+A".to_string(),
            provider: None,
        }
    }
}

/// The `[server]` section.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Listen address. Non-localhost binds require the bearer token env var.
    pub bind: String,
    /// Env var holding the bearer token (read only for non-localhost binds;
    /// the token itself is never stored in config).
    pub token_env: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind: "127.0.0.1:4820".to_string(),
            token_env: "AURICLE_TOKEN".to_string(),
        }
    }
}

/// Root configuration. Unknown sections are ignored so future additions
/// don't break older builds.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
    pub audio: AudioConfig,
    pub stt: SttConfig,
    pub llm: LlmConfig,
    pub copilot: CopilotConfig,
    pub server: ServerConfig,
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
        // Sections from future versions must not break parsing.
        let text = r#"
[audio]
mic_device = "Mic A"

[integrations]
obsidian = true
"#;
        let cfg = Config::parse(text).unwrap();
        assert_eq!(cfg.audio.mic_device, "Mic A");
    }

    #[test]
    fn llm_defaults_and_overrides() {
        let cfg = Config::parse("").unwrap();
        assert_eq!(cfg.llm.provider, "ollama");
        assert_eq!(cfg.llm.ollama.base_url, "http://localhost:11434/v1");
        assert_eq!(cfg.llm.groq.api_key_env, "GROQ_API_KEY");

        let cfg = Config::parse("[llm]\nprovider = \"groq\"\n[llm.ollama]\nmodel = \"qwen3:8b\"\n")
            .unwrap();
        assert_eq!(cfg.llm.provider, "groq");
        assert_eq!(cfg.llm.ollama.model, "qwen3:8b");
        assert_eq!(cfg.llm.openai_compat.model, "gpt-4o-mini");
    }

    #[test]
    fn stt_defaults() {
        let cfg = Config::parse("").unwrap();
        assert_eq!(cfg.stt.provider, "whisper-local");
        assert_eq!(cfg.stt.whisper_local.model, "base.en");
        assert_eq!(cfg.stt.deepgram.api_key_env, "DEEPGRAM_API_KEY");
        assert_eq!(cfg.stt.deepgram.model, "nova-3");
        assert_eq!(cfg.stt.groq_whisper.api_key_env, "GROQ_API_KEY");
        assert_eq!(
            cfg.stt.groq_whisper.base_url,
            "https://api.groq.com/openai/v1"
        );
        assert_eq!(cfg.stt.openai_compat.api_key_env, "OPENAI_API_KEY");
    }

    #[test]
    fn stt_section_parses_dashed_provider_tables() {
        let text = r#"
[stt]
provider = "deepgram"

[stt.deepgram]
api_key_env = "MY_DG_KEY"

[stt.groq-whisper]
model = "whisper-large-v3"

[stt.openai-compat]
base_url = "http://localhost:8080/v1"
"#;
        let cfg = Config::parse(text).unwrap();
        assert_eq!(cfg.stt.provider, "deepgram");
        assert_eq!(cfg.stt.deepgram.api_key_env, "MY_DG_KEY");
        assert_eq!(cfg.stt.groq_whisper.model, "whisper-large-v3");
        assert_eq!(cfg.stt.openai_compat.base_url, "http://localhost:8080/v1");
    }

    #[test]
    fn copilot_defaults_and_overrides() {
        let cfg = Config::parse("").unwrap();
        assert_eq!(cfg.copilot.transcript_window_min, 10);
        assert_eq!(cfg.copilot.max_screen_chars, 6_000);
        assert!(!cfg.copilot.retain_context, "privacy default: off");
        assert_eq!(cfg.copilot.hotkey_summon, "Ctrl+Shift+Space");
        assert_eq!(cfg.copilot.hotkey_quick, "Ctrl+Shift+A");
        assert_eq!(cfg.copilot.provider, None, "falls back to [llm].provider");

        let cfg = Config::parse(
            "[copilot]\ntranscript_window_min = 5\nretain_context = true\nprovider = \"groq\"\n",
        )
        .unwrap();
        assert_eq!(cfg.copilot.transcript_window_min, 5);
        assert!(cfg.copilot.retain_context);
        assert_eq!(cfg.copilot.provider.as_deref(), Some("groq"));
        assert_eq!(cfg.copilot.max_screen_chars, 6_000);
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
