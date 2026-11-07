//! The paste layer: puts dictated text into the window it was meant for.

/// The window that had focus when a dictation started. The pid tells it
/// apart from a later window that happens to reuse a closed one's handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub hwnd: isize,
    pub pid: u32,
}

/// Why the text was left on the clipboard rather than pasted.
#[allow(dead_code)] // sent by the paste layer, which comes next
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Left {
    NoTarget,
    TargetGone,
}

impl Left {
    pub fn reason(&self) -> &'static str {
        match self {
            Left::NoTarget => "No window had focus when the dictation started.",
            Left::TargetGone => "The window you started in is closed or hidden.",
        }
    }
}

#[allow(dead_code)] // sent by the paste layer, which comes next
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pasted {
    /// In the target. `restored` is false when the clipboard the user had
    /// before could not be put back.
    Landed {
        restored: bool,
    },
    OnClipboard(Left),
    /// Neither pasted nor on the clipboard.
    Lost(String),
}

/// The foreground window, read the moment the hotkey goes down, before
/// anything can move focus.
#[cfg(windows)]
pub fn foreground_target() -> Option<Target> {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    // SAFETY: both take and return plain window handles and ids.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        Some(Target {
            hwnd: hwnd.0 as isize,
            pid,
        })
    }
}

#[cfg(not(windows))]
pub fn foreground_target() -> Option<Target> {
    None
}

pub fn paste_text(_text: &str, _target: Option<Target>) -> Pasted {
    Pasted::Lost("Pasting is not built yet.".to_string())
}
