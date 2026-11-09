use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The config file's name under the data root, where it is found without
/// `--config`.
pub const CONFIG_FILE_NAME: &str = "auricle.toml";

/// Values `[stt].provider` accepts.
pub const STT_PROVIDER_IDS: &[&str] =
    &["whisper-local", "deepgram", "groq-whisper", "openai-compat"];

/// Values `[llm].provider` and `[copilot].provider` accept.
pub const LLM_PROVIDER_IDS: &[&str] = &["ollama", "groq", "openai-compat"];

/// The `[audio]` section of `auricle.toml`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct SttConfig {
    /// Default provider id: "whisper-local", "deepgram", "groq-whisper",
    /// "openai-compat".
    pub provider: String,
    /// Spoken language hint passed to the provider, e.g. "en".
    pub language: String,
    /// Drop mic lines that repeat what loopback just played: the far side
    /// heard through the speakers. A wrong match deletes the user's own
    /// words, so it can be turned off. Each drop is counted in the session's
    /// `meta.dropped.bleed_finals`.
    pub drop_mic_bleed: bool,
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
            language: "en".to_string(),
            drop_mic_bleed: true,
            whisper_local: WhisperLocalConfig::default(),
            deepgram: DeepgramConfig::default(),
            groq_whisper: GroqWhisperConfig::default(),
            openai_compat: OpenAiCompatConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct WhisperLocalConfig {
    pub model: String,
    /// Run inference on the GPU. Needs a build with the `vulkan` feature;
    /// any other build warns and stays on the CPU.
    pub use_gpu: bool,
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
            use_gpu: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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
/// (COPILOT_ARCHITECTURE §3). The hotkey keys are accepted but unused: the
/// overlay reads its hotkeys from its own `overlay.toml`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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

/// What [`Config::discover`] came back with.
#[derive(Debug)]
pub struct LoadedConfig {
    pub config: Config,
    /// The file it was read from, or None for the built-in defaults.
    pub path: Option<PathBuf>,
    /// Anything wrong that didn't stop the load, for the caller to show.
    pub warnings: Vec<String>,
}

/// Root configuration. Unknown keys never fail a load, so future additions
/// don't break older builds; [`Config::discover`] reports them as warnings.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
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

    /// Find and check the config: `explicit` if given (it must exist), else
    /// `auricle.toml` under `data_root` if there is one, else the built-in
    /// defaults. `required` lists the keys the command can't run without, as
    /// in [`validate`](Self::validate): a bad value there is an error, and
    /// anything else wrong is a warning, so a typo in `[llm]` can't stop a
    /// command that never reaches it.
    ///
    /// A file found on its own never stops a command. Nobody asked for it, and
    /// builds that predate this lookup never read it, so whatever is in it must
    /// not stop what used to start: it is set aside with a warning, leaving the
    /// defaults, which keep everything local.
    pub fn discover(
        explicit: Option<&Path>,
        data_root: Option<&Path>,
        required: &[&str],
    ) -> Result<LoadedConfig> {
        let default_file = data_root.map(|root| root.join(CONFIG_FILE_NAME));
        let path = match (explicit, &default_file) {
            (Some(path), _) => path,
            (None, Some(path)) if path.is_file() => path.as_path(),
            _ => {
                return Ok(LoadedConfig {
                    config: Config::default(),
                    path: None,
                    warnings: Vec::new(),
                })
            }
        };

        match Config::read_checked(path, required) {
            Ok((config, warnings)) => Ok(LoadedConfig {
                config,
                path: Some(path.to_path_buf()),
                warnings,
            }),
            Err(e) if explicit.is_some() => Err(e),
            Err(e) => Ok(LoadedConfig {
                config: Config::default(),
                path: None,
                warnings: vec![format!("{e}; ignoring this file and using the defaults")],
            }),
        }
    }

    fn read_checked(path: &Path, required: &[&str]) -> Result<(Config, Vec<String>)> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        let cfg: Config = toml::from_str(&text)
            .map_err(|e| Error::Config(format!("{}: invalid TOML: {e}", path.display())))?;
        let (fatal, others): (Vec<_>, Vec<_>) = cfg
            .problems()
            .into_iter()
            .partition(|problem| is_required(problem, required));
        if !fatal.is_empty() {
            let fatal = fatal.join("; ");
            return Err(Error::Config(format!("{}: {fatal}", path.display())));
        }
        let unknown = unknown_keys(&text)
            .into_iter()
            .map(|key| format!("unknown key `{key}` is ignored, check its spelling"));
        let warnings = others
            .into_iter()
            .chain(unknown)
            .map(|warning| format!("{}: {warning}", path.display()))
            .collect();
        Ok((cfg, warnings))
    }

    /// Check the values a typo would otherwise turn into a confusing failure
    /// much later. Anything needing a provider or device to answer (model
    /// names, whether a key is set) is left to those code paths. `required`
    /// holds dotted keys or their prefixes, like `server.bind` or `audio.`;
    /// values outside it aren't checked.
    pub fn validate(&self, required: &[&str]) -> Result<()> {
        let problems: Vec<_> = self
            .problems()
            .into_iter()
            .filter(|problem| is_required(problem, required))
            .collect();
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::Config(problems.join("; ")))
        }
    }

    /// Each one starts with its dotted key, which [`is_required`] relies on.
    fn problems(&self) -> Vec<String> {
        let (stt, llm) = (&self.stt, &self.llm);
        [
            not_empty("audio.mic_device", &self.audio.mic_device),
            not_empty("audio.loopback_device", &self.audio.loopback_device),
            one_of("stt.provider", &stt.provider, STT_PROVIDER_IDS),
            language_tag("stt.language", &stt.language),
            not_empty("stt.whisper-local.model", &stt.whisper_local.model),
            env_name("stt.deepgram.api_key_env", &stt.deepgram.api_key_env),
            not_empty("stt.deepgram.model", &stt.deepgram.model),
            env_name(
                "stt.groq-whisper.api_key_env",
                &stt.groq_whisper.api_key_env,
            ),
            not_empty("stt.groq-whisper.model", &stt.groq_whisper.model),
            http_url("stt.groq-whisper.base_url", &stt.groq_whisper.base_url),
            env_name(
                "stt.openai-compat.api_key_env",
                &stt.openai_compat.api_key_env,
            ),
            not_empty("stt.openai-compat.model", &stt.openai_compat.model),
            http_url("stt.openai-compat.base_url", &stt.openai_compat.base_url),
            one_of("llm.provider", &llm.provider, LLM_PROVIDER_IDS),
            http_url("llm.ollama.base_url", &llm.ollama.base_url),
            not_empty("llm.ollama.model", &llm.ollama.model),
            env_name("llm.groq.api_key_env", &llm.groq.api_key_env),
            http_url("llm.groq.base_url", &llm.groq.base_url),
            not_empty("llm.groq.model", &llm.groq.model),
            env_name(
                "llm.openai-compat.api_key_env",
                &llm.openai_compat.api_key_env,
            ),
            http_url("llm.openai-compat.base_url", &llm.openai_compat.base_url),
            not_empty("llm.openai-compat.model", &llm.openai_compat.model),
            (self.copilot.transcript_window_min == 0)
                .then(|| "copilot.transcript_window_min = 0 gives asks no transcript".to_string()),
            self.copilot
                .provider
                .as_deref()
                .and_then(|p| one_of("copilot.provider", p, LLM_PROVIDER_IDS)),
            socket_addr("server.bind", &self.server.bind),
            env_name("server.token_env", &self.server.token_env),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

fn is_required(problem: &str, required: &[&str]) -> bool {
    required.iter().any(|key| problem.starts_with(key))
}

/// Whether a base URL points at this machine.
pub fn is_loopback_url(url: &str) -> bool {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?']).next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn not_empty(key: &str, value: &str) -> Option<String> {
    value
        .trim()
        .is_empty()
        .then(|| format!("{key} must not be empty"))
}

fn one_of(key: &str, value: &str, allowed: &[&str]) -> Option<String> {
    (!allowed.contains(&value))
        .then(|| format!("{key} = \"{value}\" is not one of: {}", allowed.join(", ")))
}

/// Strict because the value lands in a provider's URL unescaped.
fn language_tag(key: &str, value: &str) -> Option<String> {
    let valid = !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    (!valid).then(|| format!("{key} = \"{value}\" is not a language tag like \"en\" or \"pt-BR\""))
}

/// The message never echoes the value: the likely mistake here is pasting
/// the API key itself, and this error ends up in terminals and logs.
fn env_name(key: &str, value: &str) -> Option<String> {
    let mut chars = value.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    (!valid)
        .then(|| format!("{key} must be the name of an environment variable, not the key itself"))
}

fn http_url(key: &str, value: &str) -> Option<String> {
    let rest = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"));
    let valid = rest.is_some_and(|r| !r.is_empty() && !r.contains(char::is_whitespace));
    (!valid).then(|| format!("{key} = \"{value}\" is not an http:// or https:// URL"))
}

/// Matches how `serve` parses it: a literal address, no host names.
fn socket_addr(key: &str, value: &str) -> Option<String> {
    value
        .parse::<std::net::SocketAddr>()
        .is_err()
        .then(|| format!("{key} = \"{value}\" is not an ip:port address like \"127.0.0.1:4820\""))
}

/// Dotted paths of the keys in `text` that no config field reads. An unknown
/// table is reported once, not key by key.
fn unknown_keys(text: &str) -> Vec<String> {
    let Ok(user) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    // The serialized defaults are the schema, so a new field can't be left
    // out of it. A None serializes to nothing, so optional fields are filled
    // in here.
    let mut schema = Config::default();
    schema.copilot.provider = Some(String::new());
    let Ok(known) = toml::Table::try_from(schema) else {
        return Vec::new();
    };
    let mut unknown = Vec::new();
    collect_unknown(&user, &known, "", &mut unknown);
    unknown
}

fn collect_unknown(user: &toml::Table, known: &toml::Table, prefix: &str, out: &mut Vec<String>) {
    for (key, value) in user {
        let path = format!("{prefix}{key}");
        match (known.get(key), value) {
            (None, _) => out.push(path),
            (Some(toml::Value::Table(known)), toml::Value::Table(user)) => {
                collect_unknown(user, known, &format!("{path}."), out);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A fresh temp directory acting as the data root, removed on drop.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("auricle-config-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempRoot(dir)
        }

        fn write_config(&self, text: &str) -> PathBuf {
            let path = self.0.join(CONFIG_FILE_NAME);
            std::fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Matches every key.
    const ALL: &[&str] = &[""];

    fn problem(text: &str) -> String {
        Config::parse(text)
            .unwrap()
            .validate(ALL)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn the_gpu_is_opt_in() {
        assert!(!Config::parse("").unwrap().stt.whisper_local.use_gpu);
        let cfg = Config::parse("[stt.whisper-local]\nuse_gpu = true\n").unwrap();
        assert!(cfg.stt.whisper_local.use_gpu);
    }

    #[test]
    fn dropping_mic_bleed_is_on_unless_turned_off() {
        assert!(Config::parse("").unwrap().stt.drop_mic_bleed);
        let cfg = Config::parse("[stt]\ndrop_mic_bleed = false\n").unwrap();
        assert!(!cfg.stt.drop_mic_bleed);
    }

    #[test]
    fn stt_language_defaults_to_english() {
        assert_eq!(Config::parse("").unwrap().stt.language, "en");
        let cfg = Config::parse("[stt]\nlanguage = \"de\"\n").unwrap();
        assert_eq!(cfg.stt.language, "de");
    }

    #[test]
    fn defaults_are_valid() {
        Config::default().validate(ALL).unwrap();
    }

    #[test]
    fn invalid_values_name_the_key_and_the_fix() {
        let msg = problem("[stt]\nprovider = \"deepgrm\"\n");
        assert!(msg.contains("stt.provider = \"deepgrm\""), "{msg}");
        assert!(msg.contains("deepgram"), "lists the valid ids: {msg}");

        let msg = problem("[llm]\nprovider = \"gpt\"\n");
        assert!(msg.contains("llm.provider"), "{msg}");

        let msg = problem("[copilot]\nprovider = \"gpt\"\n");
        assert!(msg.contains("copilot.provider"), "{msg}");

        let msg = problem("[copilot]\ntranscript_window_min = 0\n");
        assert!(msg.contains("copilot.transcript_window_min"), "{msg}");

        let msg = problem("[server]\nbind = \"localhost\"\n");
        assert!(msg.contains("server.bind"), "{msg}");

        let msg = problem("[audio]\nmic_device = \" \"\n");
        assert!(msg.contains("audio.mic_device"), "{msg}");

        let msg = problem("[llm.ollama]\nbase_url = \"localhost:11434\"\n");
        assert!(msg.contains("llm.ollama.base_url"), "{msg}");
    }

    #[test]
    fn language_must_be_safe_to_put_in_a_url() {
        for ok in ["en", "de", "pt-BR", "multi"] {
            let cfg = Config::parse(&format!("[stt]\nlanguage = \"{ok}\"\n")).unwrap();
            cfg.validate(ALL).unwrap();
        }
        for bad in ["", "en&model=x", "en us"] {
            let msg = problem(&format!("[stt]\nlanguage = \"{bad}\"\n"));
            assert!(msg.contains("stt.language"), "{msg}");
        }
    }

    #[test]
    fn a_pasted_api_key_is_rejected_without_being_echoed() {
        let msg = problem("[stt.deepgram]\napi_key_env = \"sk-abc123secret\"\n");
        assert!(msg.contains("stt.deepgram.api_key_env"), "{msg}");
        assert!(!msg.contains("abc123secret"), "leaked the value: {msg}");
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let msg = problem("[stt]\nprovider = \"x\"\n[llm]\nprovider = \"y\"\n");
        assert!(msg.contains("stt.provider"), "{msg}");
        assert!(msg.contains("llm.provider"), "{msg}");
    }

    #[test]
    fn unknown_keys_are_found_at_every_depth() {
        let text = r#"
typo_top = 1

[stt]
languge = "de"

[stt.deepgram]
modle = "nova-3"

[stt.deepgrm]
model = "nova-3"
api_key_env = "X"

[integrations]
obsidian = true
"#;
        let mut keys = unknown_keys(text);
        keys.sort();
        assert_eq!(
            keys,
            [
                "integrations",
                "stt.deepgram.modle",
                "stt.deepgrm",
                "stt.languge",
                "typo_top",
            ]
        );
    }

    #[test]
    fn every_real_key_is_known() {
        let text = r#"
[audio]
mic_device = "default"
loopback_device = "default"
retain_raw_audio = true

[stt]
provider = "deepgram"
language = "en"
drop_mic_bleed = false
[stt.whisper-local]
model = "base.en"
use_gpu = true
[stt.deepgram]
api_key_env = "A"
model = "nova-3"
[stt.groq-whisper]
api_key_env = "B"
model = "m"
base_url = "https://example.test/v1"
[stt.openai-compat]
api_key_env = "C"
model = "m"
base_url = "http://localhost:8080/v1"

[llm]
provider = "groq"
[llm.ollama]
base_url = "http://localhost:11434/v1"
model = "m"
[llm.groq]
api_key_env = "D"
base_url = "https://example.test/v1"
model = "m"
[llm.openai-compat]
api_key_env = "E"
base_url = "https://example.test/v1"
model = "m"

[copilot]
transcript_window_min = 5
max_screen_chars = 100
retain_context = true
hotkey_summon = "Ctrl+Shift+Space"
hotkey_quick = "Ctrl+Shift+A"
provider = "groq"

[server]
bind = "127.0.0.1:4820"
token_env = "AURICLE_TOKEN"
"#;
        assert_eq!(unknown_keys(text), Vec::<String>::new());
    }

    #[test]
    fn no_file_anywhere_means_defaults_without_warnings() {
        let root = TempRoot::new("no-file");
        let loaded = Config::discover(None, Some(&root.0), &[]).unwrap();
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.path, None);

        // No data root (its env var unset) is the same situation.
        let loaded = Config::discover(None, None, &[]).unwrap();
        assert_eq!(loaded.config, Config::default());
    }

    #[test]
    fn the_file_under_the_data_root_is_picked_up_without_a_flag() {
        let root = TempRoot::new("default-file");
        let path = root.write_config("[stt]\nlanguage = \"de\"\n");
        let loaded = Config::discover(None, Some(&root.0), &[]).unwrap();
        assert_eq!(loaded.config.stt.language, "de");
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.path, Some(path), "the caller says what it loaded");
    }

    #[test]
    fn an_explicit_path_wins_and_must_exist() {
        let root = TempRoot::new("explicit");
        root.write_config("[stt]\nlanguage = \"de\"\n");
        let explicit = root.0.join("other.toml");

        let err = Config::discover(Some(&explicit), Some(&root.0), &[]).unwrap_err();
        assert!(err.to_string().contains("other.toml"), "{err}");

        std::fs::write(&explicit, "[stt]\nlanguage = \"fr\"\n").unwrap();
        let loaded = Config::discover(Some(&explicit), Some(&root.0), &[]).unwrap();
        assert_eq!(loaded.config.stt.language, "fr");
        assert_eq!(loaded.path, Some(explicit));
    }

    #[test]
    fn a_typoed_key_warns_and_still_loads() {
        let root = TempRoot::new("typo");
        let path = root.write_config("[audio]\nmic_devcie = \"Mic\"\nmic_device = \"Real\"\n");
        let loaded = Config::discover(None, Some(&root.0), &[]).unwrap();
        let warnings = loaded.warnings;
        assert_eq!(loaded.config.audio.mic_device, "Real");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("audio.mic_devcie"), "{warnings:?}");
        assert!(
            warnings[0].contains(&path.display().to_string()),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_value_the_command_needs_fails_naming_the_file_and_key() {
        let root = TempRoot::new("invalid");
        let path = root.write_config("[server]\nbind = \"localhost\"\n");
        let msg = Config::discover(Some(&path), None, &["server.bind"])
            .unwrap_err()
            .to_string();
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        assert!(msg.contains("server.bind"), "{msg}");
    }

    #[test]
    fn a_value_the_command_never_uses_only_warns() {
        let root = TempRoot::new("unused");
        let path = root.write_config("[llm]\nprovider = \"gpt\"\n");
        let loaded = Config::discover(Some(&path), None, &["server.bind"]).unwrap();
        assert_eq!(loaded.config.llm.provider, "gpt", "left for its own error");
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
        assert!(loaded.warnings[0].contains("llm.provider"));
        assert!(loaded.warnings[0].contains(&path.display().to_string()));
    }

    #[test]
    fn a_required_prefix_covers_its_section_and_nothing_else() {
        let cfg = Config::parse("[audio]\nmic_device = \" \"\n[llm]\nprovider = \"x\"\n").unwrap();
        assert!(cfg.validate(&["audio."]).is_err());
        cfg.validate(&["server.bind", "stt."]).unwrap();
    }

    #[test]
    fn every_problem_starts_with_its_key() {
        let text = r#"
[audio]
mic_device = ""
loopback_device = ""
[stt]
provider = "x"
language = ""
[stt.whisper-local]
model = ""
[stt.deepgram]
api_key_env = "1"
model = ""
[stt.groq-whisper]
api_key_env = "1"
model = ""
base_url = "x"
[stt.openai-compat]
api_key_env = "1"
model = ""
base_url = "x"
[llm]
provider = "x"
[llm.ollama]
base_url = "x"
model = ""
[llm.groq]
api_key_env = "1"
base_url = "x"
model = ""
[llm.openai-compat]
api_key_env = "1"
base_url = "x"
model = ""
[copilot]
transcript_window_min = 0
provider = "x"
[server]
bind = "x"
token_env = "1"
"#;
        let problems = Config::parse(text).unwrap().problems();
        assert_eq!(problems.len(), 26, "{problems:#?}");
        let sections = ["audio.", "stt.", "llm.", "copilot.", "server."];
        for problem in &problems {
            assert!(is_required(problem, &sections), "{problem}");
        }
    }

    #[test]
    fn a_zero_transcript_window_still_starts() {
        let root = TempRoot::new("window");
        let path = root.write_config("[copilot]\ntranscript_window_min = 0\n");
        let loaded = Config::discover(Some(&path), None, &["server.bind"]).unwrap();
        assert_eq!(loaded.config.copilot.transcript_window_min, 0);
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
    }

    #[test]
    fn broken_toml_in_an_explicit_file_fails_naming_it() {
        let root = TempRoot::new("broken");
        let path = root.write_config("[audio\n");
        let msg = Config::discover(Some(&path), None, &[])
            .unwrap_err()
            .to_string();
        assert!(msg.contains(&path.display().to_string()), "{msg}");
    }

    /// Older builds never read this file, so nothing in it may stop what
    /// used to start, not even a value the command needs.
    #[test]
    fn a_file_nobody_asked_for_can_never_stop_a_command() {
        for (name, text) in [
            ("found-toml", "[audio\n"),
            ("found-type", "[audio]\nretain_raw_audio = \"yes\"\n"),
            ("found-bind", "[server]\nbind = \"localhost\"\n"),
        ] {
            let root = TempRoot::new(name);
            let path = root.write_config(text);
            let loaded = Config::discover(None, Some(&root.0), &["server.bind"]).unwrap();
            assert_eq!(loaded.config, Config::default(), "{name}");
            assert_eq!(loaded.path, None, "{name}: not reported as loaded");
            assert_eq!(loaded.warnings.len(), 1, "{name}: {:?}", loaded.warnings);
            assert!(loaded.warnings[0].contains(&path.display().to_string()));
            assert!(loaded.warnings[0].contains("ignoring this file"));
        }
    }

    #[test]
    fn loopback_urls_are_told_from_everything_else() {
        for url in [
            "http://localhost:11434/v1",
            "http://LOCALHOST/v1",
            "http://127.0.0.1:8080/v1",
            "http://[::1]:8080/v1",
            "http://[::1]/v1",
        ] {
            assert!(is_loopback_url(url), "{url}");
        }
        for url in [
            "https://api.groq.com/openai/v1",
            "http://192.168.1.20:8080/v1",
            "http://127.0.0.1.evil.test/v1",
            "http://localhost.evil.test/v1",
            "",
        ] {
            assert!(!is_loopback_url(url), "{url}");
        }
    }

    #[test]
    fn a_utf8_bom_does_not_break_loading() {
        // Windows PowerShell's `Out-File -Encoding utf8` writes one.
        let root = TempRoot::new("bom");
        root.write_config("\u{feff}[stt]\nlanguage = \"de\"\n");
        let loaded = Config::discover(None, Some(&root.0), &[]).unwrap();
        assert_eq!(loaded.config.stt.language, "de");
    }

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
