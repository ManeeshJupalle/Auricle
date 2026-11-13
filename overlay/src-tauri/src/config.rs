//! Overlay configuration: `%LOCALAPPDATA%\auricle\overlay.toml`.
//!
//! The engine reads `auricle.toml` from this same directory when one exists.
//! The overlay doesn't share it: it keeps its own small TOML alongside.
//! Defaults mirror the `[copilot]` section of the architecture. Missing file
//! or unparseable content falls back to defaults — a broken config must never
//! keep the copilot from starting.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct OverlayConfig {
    pub hotkeys: Hotkeys,
    pub overlay: OverlaySection,
    pub dictation: DictationSection,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub summon: String,
    pub quick: String,
    /// Dictation: talk, and the text lands where the cursor was.
    pub dictate: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Hotkeys {
            summon: "Ctrl+Shift+Space".to_string(),
            quick: "Ctrl+Shift+A".to_string(),
            dictate: "Ctrl+Shift+D".to_string(),
        }
    }
}

/// How the dictation hotkey works.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DictationMode {
    /// Hold the keys while talking; letting go pastes.
    #[default]
    Hold,
    /// One press starts, the next one pastes.
    Toggle,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct DictationSection {
    pub mode: DictationMode,
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
    /// Keep the copilot card out of screen shares and recordings
    /// (`WDA_EXCLUDEFROMCAPTURE`). Off unless the user turns it on.
    pub hide_from_screen_share: bool,
}

impl Default for OverlaySection {
    fn default() -> Self {
        OverlaySection {
            corner: Corner::BottomRight,
            theme: "dark".to_string(),
            engine_url: "http://127.0.0.1:4820".to_string(),
            transcript_window_min: 10,
            hide_from_screen_share: false,
        }
    }
}

/// `%LOCALAPPDATA%\auricle`: the engine's data root, which the overlay's
/// own files share.
pub fn data_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| std::path::PathBuf::from(base).join("auricle"))
}

impl OverlayConfig {
    pub fn parse(text: &str) -> OverlayConfig {
        toml::from_str(text).unwrap_or_default()
    }

    /// Load from `%LOCALAPPDATA%\auricle\overlay.toml`; defaults when the
    /// file is absent or invalid.
    pub fn load() -> OverlayConfig {
        let Some(dir) = data_dir() else {
            return OverlayConfig::default();
        };
        match std::fs::read_to_string(dir.join("overlay.toml")) {
            Ok(text) => OverlayConfig::parse(&text),
            Err(_) => OverlayConfig::default(),
        }
    }
}

/// `text` (an overlay.toml) with `[overlay] hide_from_screen_share` set, and
/// everything else, comments included, as the user wrote it. Refuses a file
/// the overlay would not read the setting back from: one that isn't TOML, or
/// one [`OverlayConfig::parse`] rejects and so ignores whole.
pub fn with_hide_from_screen_share(text: &str, hide: bool) -> Result<String, String> {
    let unreadable = "overlay.toml has something in it Auricle can't read";
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|_| unreadable.to_string())?;
    doc.entry("overlay")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| unreadable.to_string())?
        .insert("hide_from_screen_share", toml_edit::value(hide));
    let edited = doc.to_string();
    if OverlayConfig::parse(&edited).overlay.hide_from_screen_share != hide {
        return Err(unreadable.to_string());
    }
    Ok(edited)
}

/// Remember the tray's screen-share choice in overlay.toml, creating the
/// file if there is none.
pub fn save_hide_from_screen_share(hide: bool) -> Result<(), String> {
    let dir = data_dir().ok_or("LOCALAPPDATA is not set")?;
    let path = dir.join("overlay.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.to_string()),
    };
    let edited = with_hide_from_screen_share(&text, hide)?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(&path, edited).map_err(|e| e.to_string())
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
        assert_eq!(cfg.hotkeys.dictate, "Ctrl+Shift+D");
        assert_eq!(cfg.dictation.mode, DictationMode::Hold);
    }

    #[test]
    fn hiding_from_screen_share_is_off_until_asked_for() {
        assert!(!OverlayConfig::parse("").overlay.hide_from_screen_share);
        let cfg = OverlayConfig::parse("[overlay]\nhide_from_screen_share = true\n");
        assert!(cfg.overlay.hide_from_screen_share);
    }

    #[test]
    fn the_tray_choice_is_written_without_disturbing_the_rest_of_the_file() {
        let text = "# my hotkeys\n[hotkeys]\nsummon = \"Alt+Space\"\n\n[overlay]\ntheme = \"light\" # easier on my eyes\n";
        let hidden = with_hide_from_screen_share(text, true).unwrap();
        assert!(hidden.contains("# my hotkeys"), "{hidden}");
        assert!(hidden.contains("# easier on my eyes"), "{hidden}");
        let cfg = OverlayConfig::parse(&hidden);
        assert!(cfg.overlay.hide_from_screen_share);
        assert_eq!(cfg.overlay.theme, "light");
        assert_eq!(cfg.hotkeys.summon, "Alt+Space");

        let shown = with_hide_from_screen_share(&hidden, false).unwrap();
        assert!(!OverlayConfig::parse(&shown).overlay.hide_from_screen_share);
        assert_eq!(
            shown.matches("hide_from_screen_share").count(),
            1,
            "{shown}"
        );
    }

    #[test]
    fn the_tray_choice_makes_the_section_when_there_is_none() {
        for text in ["", "[hotkeys]\nsummon = \"Alt+Space\"\n"] {
            let hidden = with_hide_from_screen_share(text, true).unwrap();
            assert!(hidden.contains("[overlay]"), "{hidden}");
            assert!(OverlayConfig::parse(&hidden).overlay.hide_from_screen_share);
        }
    }

    #[test]
    fn the_tray_choice_is_not_written_into_a_file_that_would_be_ignored() {
        // Not TOML at all, and TOML the overlay rejects (and so ignores whole).
        assert!(with_hide_from_screen_share("[hotkeys\nsummon=", true).is_err());
        assert!(with_hide_from_screen_share("[overlay]\ncorner = 5\n", true).is_err());
        assert!(with_hide_from_screen_share("overlay = 5\n", true).is_err());
    }

    #[test]
    fn dictation_can_be_switched_to_toggle_and_rebound() {
        let cfg = OverlayConfig::parse(
            "[hotkeys]\ndictate = \"Alt+D\"\n\n[dictation]\nmode = \"toggle\"\n",
        );
        assert_eq!(cfg.hotkeys.dictate, "Alt+D");
        assert_eq!(cfg.dictation.mode, DictationMode::Toggle);
        assert_eq!(cfg.hotkeys.summon, "Ctrl+Shift+Space");
    }

    #[test]
    fn an_unknown_dictation_mode_falls_back_to_defaults() {
        let cfg = OverlayConfig::parse("[dictation]\nmode = \"sometimes\"\n");
        assert_eq!(cfg, OverlayConfig::default());
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
