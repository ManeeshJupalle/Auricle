//! Summary templates: markdown prompt files. Four defaults ship embedded;
//! a file named `<template>.md` in the templates dir (default
//! `%LOCALAPPDATA%\auricle\templates`) overrides its default, and any
//! extra `.md` file there becomes a new template — no recompile needed.

use std::path::Path;

use auricle_core::{Error, Result};

pub const DEFAULT_TEMPLATES: &[(&str, &str)] = &[
    ("minutes", include_str!("../templates/minutes.md")),
    ("action-items", include_str!("../templates/action-items.md")),
    ("standup", include_str!("../templates/standup.md")),
    ("1on1", include_str!("../templates/1on1.md")),
    // The copilot ask system prompt — not a summary template, but shipped
    // and overridden through the same mechanism (COPILOT_ARCHITECTURE
    // §2.2: "lives with the other templates, user-overridable").
    ("copilot", include_str!("../templates/copilot.md")),
];

#[derive(Debug, Clone, serde::Serialize)]
pub struct TemplateInfo {
    pub name: String,
    /// True when the text comes from a user file, not the built-in.
    pub overridden: bool,
}

/// All available templates: built-ins plus user files in `dir`.
pub fn list_templates(dir: &Path) -> Vec<TemplateInfo> {
    let mut out: Vec<TemplateInfo> = DEFAULT_TEMPLATES
        .iter()
        .map(|(name, _)| TemplateInfo {
            name: (*name).to_string(),
            overridden: dir.join(format!("{name}.md")).is_file(),
        })
        .collect();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if !DEFAULT_TEMPLATES.iter().any(|(n, _)| *n == stem) {
                        out.push(TemplateInfo {
                            name: stem.to_string(),
                            overridden: true,
                        });
                    }
                }
            }
        }
    }
    out
}

/// Load a template by name: user file first, then built-in.
pub fn load_template(dir: &Path, name: &str) -> Result<String> {
    // Template names come from API input and become file names — restrict
    // them so a request can't traverse outside the templates dir.
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::Config(format!("invalid template name \"{name}\"")));
    }
    let user_file = dir.join(format!("{name}.md"));
    if user_file.is_file() {
        return std::fs::read_to_string(&user_file).map_err(Error::Io);
    }
    DEFAULT_TEMPLATES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, text)| (*text).to_string())
        .ok_or_else(|| {
            let names: Vec<_> = list_templates(dir).iter().map(|t| t.name.clone()).collect();
            Error::Config(format!(
                "unknown template \"{name}\" (available: {})",
                names.join(", ")
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("auricle-tpl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn builtins_load_without_a_templates_dir() {
        let dir = std::path::Path::new("Z:/does/not/exist");
        for (name, _) in DEFAULT_TEMPLATES {
            let text = load_template(dir, name).unwrap();
            assert!(text.contains("transcript"), "{name} looks like a prompt");
        }
        assert_eq!(list_templates(dir).len(), 5);
    }

    #[test]
    fn user_file_overrides_builtin_and_adds_new() {
        let dir = temp_dir("override");
        std::fs::write(dir.join("minutes.md"), "CUSTOM MINUTES PROMPT").unwrap();
        std::fs::write(dir.join("retro.md"), "RETRO PROMPT").unwrap();

        assert_eq!(
            load_template(&dir, "minutes").unwrap(),
            "CUSTOM MINUTES PROMPT"
        );
        assert_eq!(load_template(&dir, "retro").unwrap(), "RETRO PROMPT");
        // Untouched built-in still resolves to the embedded text.
        assert!(load_template(&dir, "standup").unwrap().contains("Blockers"));

        let list = list_templates(&dir);
        assert_eq!(list.len(), 6);
        let minutes = list.iter().find(|t| t.name == "minutes").unwrap();
        assert!(minutes.overridden);
        let standup = list.iter().find(|t| t.name == "standup").unwrap();
        assert!(!standup.overridden);
    }

    #[test]
    fn unknown_and_malicious_names_are_clean_errors() {
        let dir = temp_dir("bad");
        let err = load_template(&dir, "nope").unwrap_err();
        assert!(err.to_string().contains("available:"), "{err}");
        let err = load_template(&dir, "../secrets").unwrap_err();
        assert!(err.to_string().contains("invalid template name"), "{err}");
    }
}
