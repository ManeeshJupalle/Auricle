//! Overlay configuration: `%LOCALAPPDATA%\auricle\overlay.toml`.
//!
//! The engine reads no config file by default (`auricle serve` runs on
//! built-in defaults unless --config is passed), so the overlay keeps its
//! own small TOML next to the engine's data. Defaults mirror the
//! `[copilot]` section of the architecture. Missing file or unparseable
//! content falls back to defaults — a broken config must never keep the
//! copilot from starting.

use serde::Deserialize;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct OverlayConfig {
    pub hotkeys: Hotkeys,
    pub overlay: OverlaySection,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub summon: String,
    pub quick: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Hotkeys {
            summon: "Ctrl+Shift+Space".to_string(),
            quick: "Ctrl+Shift+A".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct OverlaySection {
    /// Which corner of the work area the overlay docks to.
    pub corner: Corner,
    /// "dark" | "light" — passed to the webview as data-theme.
    pub theme: String,
    /// The engine's base URL (public API).
    pub engine_url: String,
    /// Shown on the transcript context chip ("last N min"). The engine
    /// does not expose its `copilot.transcript_window_min` over the API,
    /// so this mirrors it (default matches the engine default).
    pub transcript_window_min: u64,
}

impl Default for OverlaySection {
    fn default() -> Self {
        OverlaySection {
            corner: Corner::BottomRight,
            theme: "dark".to_string(),
            engine_url: "http://127.0.0.1:4820".to_string(),
            transcript_window_min: 10,
        }
    }
}

impl OverlayConfig {
    pub fn parse(text: &str) -> OverlayConfig {
        toml::from_str(text).unwrap_or_default()
    }

    /// Load from `%LOCALAPPDATA%\auricle\overlay.toml`; defaults when the
    /// file is absent or invalid.
    pub fn load() -> OverlayConfig {
        let Some(base) = std::env::var_os("LOCALAPPDATA") else {
            return OverlayConfig::default();
        };
        let path = std::path::PathBuf::from(base)
            .join("auricle")
            .join("overlay.toml");
        match std::fs::read_to_string(path) {
            Ok(text) => OverlayConfig::parse(&text),
            Err(_) => OverlayConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_architecture() {
        let cfg = OverlayConfig::parse("");
        assert_eq!(cfg.hotkeys.summon, "Ctrl+Shift+Space");
        assert_eq!(cfg.hotkeys.quick, "Ctrl+Shift+A");
        assert_eq!(cfg.overlay.corner, Corner::BottomRight);
        assert_eq!(cfg.overlay.engine_url, "http://127.0.0.1:4820");
        assert_eq!(cfg.overlay.transcript_window_min, 10);
        assert_eq!(cfg.overlay.theme, "dark");
    }

    #[test]
    fn partial_config_overrides_only_what_it_names() {
        let cfg = OverlayConfig::parse(
            "[hotkeys]\nsummon = \"Alt+Space\"\n\n[overlay]\ncorner = \"top-left\"\ntheme = \"light\"\n",
        );
        assert_eq!(cfg.hotkeys.summon, "Alt+Space");
        assert_eq!(
            cfg.hotkeys.quick, "Ctrl+Shift+A",
            "unnamed key keeps default"
        );
        assert_eq!(cfg.overlay.corner, Corner::TopLeft);
        assert_eq!(cfg.overlay.theme, "light");
        assert_eq!(cfg.overlay.engine_url, "http://127.0.0.1:4820");
    }

    #[test]
    fn invalid_toml_falls_back_to_defaults() {
        let cfg = OverlayConfig::parse("[hotkeys\nsummon=");
        assert_eq!(cfg, OverlayConfig::default());
        // Wrong types too — never panic, never half-apply.
        let cfg = OverlayConfig::parse("[overlay]\ncorner = 5\n");
        assert_eq!(cfg, OverlayConfig::default());
    }
}
