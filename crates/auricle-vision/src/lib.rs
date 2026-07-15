//! On-demand screen capture + local OCR (architecture: COPILOT §2.1).
//!
//! One capability behind one trait: grab a single frame of the active
//! window via Windows.Graphics.Capture, OCR it with Windows.Media.Ocr,
//! and return reading-order text. Strictly on demand — no capture loops,
//! no timers, nothing persisted. Never uses (and must never use) any
//! window-concealment API; excluding *our own* window from *our own*
//! capture by HWND is a correctness measure, not concealment.
//!
//! Empirical constraints baked in here (docs/PHASE7_VISION_REPORT.md):
//! the D3D device and OCR engine are cached across captures (~300 ms per
//! call otherwise), minimized windows must be rejected before capture
//! (frames never arrive), and cloaked windows deliver convincing blank
//! frames so they are never eligible.

use std::fmt;

use serde::{Deserialize, Serialize};

#[cfg(windows)]
mod windows_ocr;
#[cfg(windows)]
pub use windows_ocr::WindowsOcrReader;

/// What a single on-demand capture saw.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenContext {
    /// Title of the captured window at capture time.
    pub window_title: String,
    /// Executable name (without extension) that owns the window.
    pub app_name: String,
    /// OCR text flattened to reading order (bands top-to-bottom,
    /// left-to-right within a band, newline-joined).
    pub text: String,
    /// Unix epoch milliseconds when the frame was captured.
    pub captured_at: u64,
    /// Milliseconds spent in OCR recognition (excludes capture).
    pub ocr_ms: u64,
}

/// Typed failures — every OS-level surprise maps to one of these; the
/// implementation never panics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisionError {
    /// Screen capture is unsupported or blocked on this system.
    CaptureDenied(String),
    /// The target window is minimized (frames would never arrive).
    Minimized,
    /// The target window no longer exists.
    WindowGone,
    /// No visible, titled, non-cloaked window was eligible for capture.
    NoWindow,
    /// No OCR language pack is installed for the user's languages.
    OcrLanguageMissing,
    /// Capture or OCR failed for another reason (timeout, D3D error, …).
    CaptureFailed(String),
}

impl fmt::Display for VisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VisionError::CaptureDenied(detail) => {
                write!(f, "screen capture denied: {detail}")
            }
            VisionError::Minimized => write!(f, "the target window is minimized"),
            VisionError::WindowGone => write!(f, "the target window no longer exists"),
            VisionError::NoWindow => write!(f, "no capturable window found"),
            VisionError::OcrLanguageMissing => {
                write!(
                    f,
                    "no OCR language pack is installed (Settings > Time & Language > \
                     Language: add a language with the \"Basic typing\" / OCR feature)"
                )
            }
            VisionError::CaptureFailed(detail) => write!(f, "screen capture failed: {detail}"),
        }
    }
}

impl std::error::Error for VisionError {}

/// A screen-to-text reader. `exclude_hwnd` is skipped when picking the
/// active window (PHASE-9: the copilot overlay passes its own HWND so an
/// answer is never OCR'd back into the next question's context).
pub trait ScreenReader: Send + Sync {
    fn capture_active_window(
        &self,
        exclude_hwnd: Option<isize>,
    ) -> Result<ScreenContext, VisionError>;
}

/// One recognized OCR line with its bounding box in captured-pixel space.
#[derive(Debug, Clone)]
pub struct OcrLine {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Flatten OCR lines into reading order: group lines into horizontal
/// bands (same band ⇔ vertical centers within half the taller line's
/// height), sort bands top-to-bottom, lines left-to-right within a band.
/// Band members join with spaces, bands with newlines.
///
/// Windows OCR does not guarantee this order itself: on a real capture it
/// returned a bullet's continuation text *before* the bullet on the same
/// visual line (see docs/PHASE7_VISION_REPORT.md).
pub fn flatten_reading_order(lines: &[OcrLine]) -> String {
    let mut sorted: Vec<&OcrLine> = lines.iter().filter(|l| !l.text.is_empty()).collect();
    sorted.sort_by(|a, b| {
        let (ca, cb) = (a.y + a.height / 2.0, b.y + b.height / 2.0);
        ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut bands: Vec<Vec<&OcrLine>> = Vec::new();
    let mut band_center = 0.0f32;
    let mut band_max_h = 0.0f32;
    for line in sorted {
        let center = line.y + line.height / 2.0;
        let same_band = bands.last().is_some_and(|band| {
            !band.is_empty() && (center - band_center).abs() <= band_max_h.max(line.height) / 2.0
        });
        if same_band {
            let band = bands.last_mut().expect("checked non-empty");
            band.push(line);
            // Running mean keeps the band anchored to its members, so a
            // gradual stair-step cannot chain distant lines together.
            let n = band.len() as f32;
            band_center += (center - band_center) / n;
            band_max_h = band_max_h.max(line.height);
        } else {
            bands.push(vec![line]);
            band_center = center;
            band_max_h = line.height;
        }
    }

    let mut out = String::new();
    for band in &mut bands {
        band.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal));
        if !out.is_empty() {
            out.push('\n');
        }
        for (i, line) in band.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(line.text.trim());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, x: f32, y: f32, width: f32, height: f32) -> OcrLine {
        OcrLine {
            text: text.to_string(),
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn rows_stay_top_to_bottom() {
        let text = flatten_reading_order(&[
            line("second", 10.0, 40.0, 100.0, 14.0),
            line("first", 10.0, 10.0, 100.0, 14.0),
            line("third", 10.0, 70.0, 100.0, 14.0),
        ]);
        assert_eq!(text, "first\nsecond\nthird");
    }

    #[test]
    fn same_visual_line_sorts_left_to_right() {
        // The real-world shape from the probe: Windows OCR split
        // "• 1962 — IBM's ..." into two lines and returned the
        // continuation (x=91, y=557, h=19) before the bullet
        // (x=35, y=561, h=12).
        let text = flatten_reading_order(&[
            line("— IBM's 16-word \"Shoebox\"", 91.0, 557.0, 669.0, 19.0),
            line("• 1962", 35.0, 561.0, 51.0, 12.0),
        ]);
        assert_eq!(text, "• 1962 — IBM's 16-word \"Shoebox\"");
    }

    #[test]
    fn band_membership_uses_the_taller_line() {
        // Centers 8 px apart: outside the short line's half-height (5)
        // but inside the tall line's (10) — the taller line decides.
        let text = flatten_reading_order(&[
            line("tall", 200.0, 100.0, 40.0, 20.0),
            line("short", 10.0, 113.0, 40.0, 10.0),
        ]);
        assert_eq!(text, "short tall");
    }

    #[test]
    fn distinct_rows_stay_distinct() {
        // Centers 20 px apart, half-height 5: no overlap, three bands.
        let text = flatten_reading_order(&[
            line("a", 10.0, 0.0, 20.0, 10.0),
            line("b", 10.0, 20.0, 20.0, 10.0),
            line("c", 10.0, 40.0, 20.0, 10.0),
        ]);
        assert_eq!(text, "a\nb\nc");
    }

    #[test]
    fn stair_step_does_not_chain_across_rows() {
        // Centers at 8, 16, 24 with height 16 (half = 8): b overlaps a
        // and c overlaps b, but the running band mean (12 after a+b)
        // leaves c (center 24) out — adjacent overlap must not chain
        // arbitrarily far down the page.
        let text = flatten_reading_order(&[
            line("a", 10.0, 0.0, 20.0, 16.0),
            line("b", 40.0, 8.0, 20.0, 16.0),
            line("c", 70.0, 16.0, 20.0, 16.0),
        ]);
        assert_eq!(text, "a b\nc");
    }

    #[test]
    fn empty_and_blank_lines_vanish() {
        assert_eq!(flatten_reading_order(&[]), "");
        let text = flatten_reading_order(&[
            line("", 0.0, 0.0, 10.0, 10.0),
            line("only", 0.0, 30.0, 10.0, 10.0),
        ]);
        assert_eq!(text, "only");
    }
}
