//! The paste layer: puts dictated text into the window it was meant for.
//!
//! Adapted from voicebox (MIT, see THIRD_PARTY_NOTICES.md): the Windows
//! clipboard snapshot and restore of its `clipboard.rs`, Ctrl+V through
//! SendInput from `synthetic_keys.rs`, bringing a window forward from
//! `focus_capture.rs`, and the paste sequence in `main.rs`. Changed here:
//!
//! - It aims at the exact window that had focus at the press, not a
//!   process's first window, and checks that window is still there and
//!   visible before anything is sent.
//! - Text it puts on the clipboard carries the formats that keep it out of
//!   clipboard history and cloud sync, the fallback's included.
//! - The user's clipboard is put back only if it still holds ours, so a copy
//!   made in the meantime is never overwritten.
//! - A window that is gone, won't come forward, or runs with higher rights
//!   (where Windows drops the keystroke without a word) gets nothing typed
//!   into it: the text stays on the clipboard and the user is told.
//! - Ctrl+V waits for every modifier to come up, so the target never sees
//!   Ctrl+Shift+V or the like from the hotkey still being held.
//!
//! All of the Win32 is behind [`Desktop`], so the sequence is tested against
//! a fake. No test touches the real clipboard, keyboard or windows.

use std::time::Duration;

/// Lets the window that was brought forward settle before Ctrl+V.
const SETTLE: Duration = Duration::from_millis(120);
/// How long the target gets to read the clipboard before it is put back. A
/// busy app reads late, and one that reads after the restore would paste
/// the user's old clipboard instead, so this errs long.
const CONSUME: Duration = Duration::from_millis(600);
/// How long the hotkey's modifiers may stay down before the paste gives up.
const KEYS_WAIT: Duration = Duration::from_secs(3);
const KEYS_POLL: Duration = Duration::from_millis(20);

/// The window that had focus when a dictation started. The pid tells it
/// apart from a later window that happens to reuse a closed one's handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub hwnd: isize,
    pub pid: u32,
}

/// Why the text was left on the clipboard rather than pasted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Left {
    NoTarget,
    TargetGone,
    Elevated,
    KeysHeld,
    NoFocus,
    KeystrokeRefused,
}

impl Left {
    pub fn reason(&self) -> &'static str {
        match self {
            Left::NoTarget => "No window had focus when the dictation started.",
            Left::TargetGone => "The window you started in is closed or hidden.",
            Left::Elevated => {
                "That window runs as administrator, or its rights can't be read, so Auricle \
                 can't type into it."
            }
            Left::KeysHeld => "Keys were still held down, so Auricle didn't paste.",
            Left::NoFocus => "The window you started in didn't come to the front.",
            Left::KeystrokeRefused => "Windows refused the paste keystroke.",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pasted {
    /// In the target. `restored` is false when the clipboard the user had
    /// could not be put back; a copy they made meanwhile counts as kept.
    Landed {
        restored: bool,
    },
    OnClipboard(Left),
    /// Neither pasted nor on the clipboard.
    Lost(String),
}

/// Everything the user had on the clipboard: (format, bytes) per format.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot(pub Vec<(u32, Vec<u8>)>);

/// The desktop as the paste sees it.
pub trait Desktop {
    /// The same window is still there, visible, and not minimized.
    fn is_open(&self, target: Target) -> bool;
    /// Keystrokes from this process reach it. Windows silently drops them
    /// on a window that runs with higher rights than ours.
    fn takes_our_input(&self, target: Target) -> bool;
    fn modifiers_down(&self) -> bool;
    fn bring_forward(&self, target: Target);
    fn is_foreground(&self, target: Target) -> bool;
    fn snapshot(&self) -> Result<Snapshot, String>;
    /// Put `text` on the clipboard, kept out of history and cloud sync, and
    /// return the clipboard's sequence number once it is there.
    fn put_private(&self, text: &str) -> Result<u32, String>;
    fn sequence(&self) -> u32;
    fn restore(&self, snapshot: &Snapshot) -> Result<(), String>;
    fn send_ctrl_v(&self) -> Result<(), String>;
    fn sleep(&self, how_long: Duration);
}

/// Paste `text` into `target`, or leave it on the clipboard and say why.
/// Dictated text is never typed into any other window.
pub fn paste(desk: &impl Desktop, text: &str, target: Option<Target>) -> Pasted {
    let Some(target) = target else {
        return leave(desk, text, Left::NoTarget);
    };
    if !desk.is_open(target) {
        return leave(desk, text, Left::TargetGone);
    }
    if !desk.takes_our_input(target) {
        return leave(desk, text, Left::Elevated);
    }
    if !keys_up(desk) {
        return leave(desk, text, Left::KeysHeld);
    }
    if !desk.is_foreground(target) {
        desk.bring_forward(target);
        desk.sleep(SETTLE);
        if !desk.is_foreground(target) {
            return leave(desk, text, Left::NoFocus);
        }
    }
    let snapshot = desk.snapshot();
    let ours = match desk.put_private(text) {
        Ok(sequence) => sequence,
        Err(e) => return Pasted::Lost(e),
    };
    // A last look right before the keystroke.
    if !desk.is_foreground(target) {
        return Pasted::OnClipboard(Left::NoFocus);
    }
    if desk.modifiers_down() {
        return Pasted::OnClipboard(Left::KeysHeld);
    }
    if desk.send_ctrl_v().is_err() {
        return Pasted::OnClipboard(Left::KeystrokeRefused);
    }
    desk.sleep(CONSUME);
    let restored = match snapshot {
        Ok(snapshot) if desk.sequence() == ours => desk.restore(&snapshot).is_ok(),
        // Something was copied since: that stays.
        Ok(_) => true,
        Err(_) => false,
    };
    Pasted::Landed { restored }
}

fn leave(desk: &impl Desktop, text: &str, why: Left) -> Pasted {
    match desk.put_private(text) {
        Ok(_) => Pasted::OnClipboard(why),
        Err(e) => Pasted::Lost(e),
    }
}

fn keys_up(desk: &impl Desktop) -> bool {
    let polls = KEYS_WAIT.as_millis() / KEYS_POLL.as_millis();
    for _ in 0..polls {
        if !desk.modifiers_down() {
            return true;
        }
        desk.sleep(KEYS_POLL);
    }
    !desk.modifiers_down()
}

/// The foreground window, read the moment the hotkey goes down, before
/// anything can move focus.
#[cfg(windows)]
pub fn foreground_target() -> Option<Target> {
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    // SAFETY: takes and returns a plain window handle.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return None;
    }
    Some(Target {
        hwnd: hwnd.0 as isize,
        pid: win::pid_of(hwnd),
    })
}

#[cfg(not(windows))]
pub fn foreground_target() -> Option<Target> {
    None
}

#[cfg(windows)]
pub fn paste_text(text: &str, target: Option<Target>) -> Pasted {
    paste(&win::Win32, text, target)
}

#[cfg(not(windows))]
pub fn paste_text(_text: &str, _target: Option<Target>) -> Pasted {
    Pasted::Lost("Pasting needs Windows.".to_string())
}

/// The formats Windows reads to keep an item out of clipboard history and
/// cloud sync. Per Microsoft's "Cloud Clipboard and Clipboard History
/// Formats": any data in the first keeps the item from both; a DWORD of 0 in
/// the others keeps it from history and from sync respectively. All three
/// are set, in case one is ever read without the others.
const PRIVATE_FORMATS: [&str; 3] = [
    "ExcludeClipboardContentFromMonitorProcessing",
    "CanIncludeInClipboardHistory",
    "CanUploadToCloudClipboard",
];

const CF_TEXT: u32 = 1;
const CF_BITMAP: u32 = 2;
const CF_METAFILEPICT: u32 = 3;
const CF_OEMTEXT: u32 = 7;
const CF_PALETTE: u32 = 9;
const CF_UNICODETEXT: u32 = 13;
const CF_ENHMETAFILE: u32 = 14;
const CF_LOCALE: u32 = 16;

/// Formats a snapshot can't carry across: GDI handles and owner-drawn ones
/// hold no plain memory, private ones mean nothing outside their app, and
/// Windows rebuilds the text variants from CF_UNICODETEXT on its own.
fn skipped(format: u32) -> bool {
    matches!(
        format,
        CF_TEXT
            | CF_BITMAP
            | CF_METAFILEPICT
            | CF_OEMTEXT
            | CF_PALETTE
            | CF_ENHMETAFILE
            | CF_LOCALE
    ) || (0x0080..=0x008E).contains(&format)
        || (0x0200..=0x03FF).contains(&format)
}

/// `text` as CF_UNICODETEXT wants it: UTF-16 with a terminating null.
fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(windows)]
mod win {
    use std::time::Duration;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GlobalFree, HANDLE, HGLOBAL, HWND};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
        GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows::Win32::System::Threading::{
        AttachThreadInput, GetCurrentProcess, GetCurrentThreadId, OpenProcess, OpenProcessToken,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT, VK_V,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
        SetForegroundWindow,
    };

    use super::{skipped, utf16z, Desktop, Snapshot, Target, CF_UNICODETEXT, PRIVATE_FORMATS};

    pub struct Win32;

    fn hwnd(target: Target) -> HWND {
        HWND(target.hwnd as *mut core::ffi::c_void)
    }

    pub fn pid_of(hwnd: HWND) -> u32 {
        let mut pid = 0u32;
        // SAFETY: a plain handle in, the pid written to a local.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        pid
    }

    /// Whether `process` runs elevated; None when its token can't be read,
    /// as an elevated one can't from a process that isn't.
    fn elevated(process: HANDLE) -> Option<bool> {
        // SAFETY: the token handle is ours and closed here; the info is
        // written into a local of exactly the size passed.
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
            let mut info = TOKEN_ELEVATION::default();
            let mut len = 0u32;
            let read = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut info as *mut TOKEN_ELEVATION as *mut core::ffi::c_void),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            );
            let _ = CloseHandle(token);
            read.ok()?;
            Some(info.TokenIsElevated != 0)
        }
    }

    /// The clipboard, open until this is dropped.
    struct Open;

    impl Open {
        fn clipboard() -> Result<Open, String> {
            // Another program may hold it for a moment, mid-paste.
            for _ in 0..20 {
                // SAFETY: no owner window; closed again on drop.
                if unsafe { OpenClipboard(None) }.is_ok() {
                    return Ok(Open);
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            Err("Another program is holding the clipboard.".to_string())
        }
    }

    impl Drop for Open {
        fn drop(&mut self) {
            // SAFETY: only reached with the clipboard open.
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    /// One format's bytes off the open clipboard. None for a format that is
    /// listed but never rendered.
    fn read(format: u32) -> Option<Vec<u8>> {
        // SAFETY: the clipboard is open; the memory is read under its lock
        // and copied out before the unlock.
        unsafe {
            let handle = GetClipboardData(format).ok()?;
            if handle.is_invalid() {
                return None;
            }
            let global = HGLOBAL(handle.0);
            let size = GlobalSize(global);
            if size == 0 {
                return Some(Vec::new());
            }
            let ptr = GlobalLock(global);
            if ptr.is_null() {
                return None;
            }
            let bytes = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
            let _ = GlobalUnlock(global);
            Some(bytes)
        }
    }

    /// Put `bytes` on the open clipboard as `format`. On success the
    /// clipboard owns the memory, so it is freed only on failure.
    fn put(format: u32, bytes: &[u8]) -> Result<(), String> {
        // GlobalAlloc of 0 gives a handle SetClipboardData refuses.
        let len = bytes.len().max(1);
        // SAFETY: the copy stays inside the `len` bytes just allocated.
        unsafe {
            let global = GlobalAlloc(GMEM_MOVEABLE, len).map_err(|e| e.to_string())?;
            let ptr = GlobalLock(global);
            if ptr.is_null() {
                let _ = GlobalFree(Some(global));
                return Err("the clipboard memory could not be locked".to_string());
            }
            std::ptr::write_bytes(ptr as *mut u8, 0, len);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
            let _ = GlobalUnlock(global);
            if let Err(e) = SetClipboardData(format, Some(HANDLE(global.0))) {
                let _ = GlobalFree(Some(global));
                return Err(e.to_string());
            }
        }
        Ok(())
    }

    fn register(name: &str) -> Result<u32, String> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is null-terminated and outlives the call.
        match unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) } {
            0 => Err(format!(
                "the {name} clipboard format could not be registered"
            )),
            id => Ok(id),
        }
    }

    fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS(0)
                    },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    impl Desktop for Win32 {
        fn is_open(&self, target: Target) -> bool {
            let h = hwnd(target);
            // SAFETY: each takes a handle and only reads its state.
            unsafe {
                IsWindow(Some(h)).as_bool()
                    && IsWindowVisible(h).as_bool()
                    && !IsIconic(h).as_bool()
                    && pid_of(h) == target.pid
            }
        }

        fn takes_our_input(&self, target: Target) -> bool {
            // SAFETY: the pseudo-handle needs no closing.
            if elevated(unsafe { GetCurrentProcess() }) == Some(true) {
                return true;
            }
            // SAFETY: query-only access, closed below.
            let Ok(process) =
                (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, target.pid) })
            else {
                return false;
            };
            let theirs = elevated(process);
            // SAFETY: opened above.
            unsafe {
                let _ = CloseHandle(process);
            }
            theirs == Some(false)
        }

        fn modifiers_down(&self) -> bool {
            [VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN]
                .iter()
                // SAFETY: reads one key's state.
                .any(|vk| unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000 != 0)
        }

        fn bring_forward(&self, target: Target) {
            // SAFETY: plain handles and thread ids; the attach is undone.
            unsafe {
                let front = GetForegroundWindow();
                let ours = GetCurrentThreadId();
                let theirs = if front.is_invalid() {
                    0
                } else {
                    GetWindowThreadProcessId(front, None)
                };
                // Sharing the front window's input state is the usual way past
                // the foreground lock, which otherwise turns this into a
                // taskbar flash. Whether it worked is checked by the caller.
                let attached = theirs != 0
                    && theirs != ours
                    && AttachThreadInput(ours, theirs, true).as_bool();
                let _ = SetForegroundWindow(hwnd(target));
                if attached {
                    let _ = AttachThreadInput(ours, theirs, false);
                }
            }
        }

        fn is_foreground(&self, target: Target) -> bool {
            // SAFETY: reads the foreground handle.
            let front = unsafe { GetForegroundWindow() };
            front == hwnd(target) && pid_of(front) == target.pid
        }

        fn snapshot(&self) -> Result<Snapshot, String> {
            let _open = Open::clipboard()?;
            let mut formats = Vec::new();
            let mut format = 0;
            loop {
                // SAFETY: the clipboard is open.
                format = unsafe { EnumClipboardFormats(format) };
                if format == 0 {
                    break;
                }
                if skipped(format) {
                    continue;
                }
                if let Some(bytes) = read(format) {
                    formats.push((format, bytes));
                }
            }
            Ok(Snapshot(formats))
        }

        fn put_private(&self, text: &str) -> Result<u32, String> {
            let private = PRIVATE_FORMATS
                .iter()
                .map(|name| register(name))
                .collect::<Result<Vec<_>, _>>()?;
            {
                let _open = Open::clipboard()?;
                // SAFETY: the clipboard is open.
                unsafe { EmptyClipboard() }.map_err(|e| e.to_string())?;
                // Ahead of the text, so nothing ever sees the text without them.
                for format in private {
                    put(format, &0u32.to_le_bytes())?;
                }
                put(CF_UNICODETEXT, &utf16z(text))?;
            }
            // Read once it is closed, when nothing more of ours can change it.
            Ok(self.sequence())
        }

        fn sequence(&self) -> u32 {
            // SAFETY: no arguments, reads a counter.
            unsafe { GetClipboardSequenceNumber() }
        }

        fn restore(&self, snapshot: &Snapshot) -> Result<(), String> {
            let _open = Open::clipboard()?;
            // SAFETY: the clipboard is open.
            unsafe { EmptyClipboard() }.map_err(|e| e.to_string())?;
            for (format, bytes) in &snapshot.0 {
                // One odd format must not cost the user the rest.
                let _ = put(*format, bytes);
            }
            Ok(())
        }

        fn send_ctrl_v(&self) -> Result<(), String> {
            let events = [
                key(VK_CONTROL, false),
                key(VK_V, false),
                key(VK_V, true),
                key(VK_CONTROL, true),
            ];
            // SAFETY: a slice of fully built keyboard inputs.
            let sent = unsafe { SendInput(&events, std::mem::size_of::<INPUT>() as i32) };
            if sent as usize == events.len() {
                Ok(())
            } else {
                Err(format!(
                    "SendInput delivered {sent} of {} keys",
                    events.len()
                ))
            }
        }

        fn sleep(&self, how_long: Duration) {
            std::thread::sleep(how_long);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    const WIN: Target = Target { hwnd: 42, pid: 7 };

    /// A desktop made of switches. `log` records what touched the clipboard
    /// or the keyboard, in order.
    struct Fake {
        open: bool,
        input_ok: bool,
        /// Polls for which a modifier still reads as down.
        held: Cell<u32>,
        /// Polls of is_foreground, in order; the last one repeats.
        front: RefCell<Vec<bool>>,
        comes_forward: bool,
        snapshot: Result<Snapshot, String>,
        put_ok: bool,
        send_ok: bool,
        /// The clipboard changed hands during the paste.
        copied_meanwhile: bool,
        log: RefCell<Vec<String>>,
        slept: Cell<Duration>,
    }

    impl Default for Fake {
        fn default() -> Self {
            Fake {
                open: true,
                input_ok: true,
                held: Cell::new(0),
                front: RefCell::new(vec![true]),
                comes_forward: true,
                snapshot: Ok(Snapshot(vec![(CF_UNICODETEXT, utf16z("the user's"))])),
                put_ok: true,
                send_ok: true,
                copied_meanwhile: false,
                log: RefCell::new(Vec::new()),
                slept: Cell::new(Duration::ZERO),
            }
        }
    }

    impl Fake {
        fn log(&self) -> Vec<String> {
            self.log.borrow().clone()
        }

        fn note(&self, what: &str) {
            self.log.borrow_mut().push(what.to_string());
        }
    }

    impl Desktop for Fake {
        fn is_open(&self, _: Target) -> bool {
            self.open
        }
        fn takes_our_input(&self, _: Target) -> bool {
            self.input_ok
        }
        fn modifiers_down(&self) -> bool {
            let held = self.held.get();
            self.held.set(held.saturating_sub(1));
            held > 0
        }
        fn bring_forward(&self, _: Target) {
            self.note("forward");
            if self.comes_forward {
                *self.front.borrow_mut() = vec![true];
            }
        }
        fn is_foreground(&self, _: Target) -> bool {
            let mut front = self.front.borrow_mut();
            if front.len() > 1 {
                front.remove(0)
            } else {
                front[0]
            }
        }
        fn snapshot(&self) -> Result<Snapshot, String> {
            self.note("snapshot");
            self.snapshot.clone()
        }
        fn put_private(&self, text: &str) -> Result<u32, String> {
            self.note(&format!("put {text}"));
            if self.put_ok {
                Ok(5)
            } else {
                Err("Another program is holding the clipboard.".to_string())
            }
        }
        fn sequence(&self) -> u32 {
            if self.copied_meanwhile {
                6
            } else {
                5
            }
        }
        fn restore(&self, snapshot: &Snapshot) -> Result<(), String> {
            assert_eq!(
                Ok(snapshot),
                self.snapshot.as_ref(),
                "puts back what it took"
            );
            self.note("restore");
            Ok(())
        }
        fn send_ctrl_v(&self) -> Result<(), String> {
            self.note("ctrl+v");
            if self.send_ok {
                Ok(())
            } else {
                Err("refused".to_string())
            }
        }
        fn sleep(&self, how_long: Duration) {
            self.slept.set(self.slept.get() + how_long);
        }
    }

    fn put(text: &str) -> String {
        format!("put {text}")
    }

    #[test]
    fn it_pastes_into_the_target_and_puts_the_clipboard_back() {
        let desk = Fake::default();
        let out = paste(&desk, "hello", Some(WIN));
        assert_eq!(out, Pasted::Landed { restored: true });
        assert_eq!(desk.log(), ["snapshot", &put("hello"), "ctrl+v", "restore"]);
        assert!(
            desk.slept.get() >= CONSUME,
            "the target gets time to read it"
        );
    }

    #[test]
    fn a_copy_made_during_the_paste_is_never_overwritten() {
        let desk = Fake {
            copied_meanwhile: true,
            ..Fake::default()
        };
        assert_eq!(
            paste(&desk, "hi", Some(WIN)),
            Pasted::Landed { restored: true }
        );
        assert!(!desk.log().contains(&"restore".to_string()));
    }

    #[test]
    fn an_unreadable_clipboard_still_pastes_and_says_it_was_not_kept() {
        let desk = Fake {
            snapshot: Err("busy".to_string()),
            ..Fake::default()
        };
        assert_eq!(
            paste(&desk, "hi", Some(WIN)),
            Pasted::Landed { restored: false }
        );
        assert_eq!(desk.log(), ["snapshot", &put("hi"), "ctrl+v"]);
    }

    #[test]
    fn with_no_target_the_text_goes_to_the_clipboard_only() {
        let desk = Fake::default();
        assert_eq!(
            paste(&desk, "hi", None),
            Pasted::OnClipboard(Left::NoTarget)
        );
        assert_eq!(desk.log(), [put("hi")]);
    }

    #[test]
    fn a_closed_target_gets_nothing_typed_anywhere() {
        let desk = Fake {
            open: false,
            ..Fake::default()
        };
        assert_eq!(
            paste(&desk, "hi", Some(WIN)),
            Pasted::OnClipboard(Left::TargetGone)
        );
        assert_eq!(desk.log(), [put("hi")], "nothing brought forward, no keys");
    }

    #[test]
    fn a_window_with_higher_rights_is_not_typed_into() {
        let desk = Fake {
            input_ok: false,
            ..Fake::default()
        };
        assert_eq!(
            paste(&desk, "hi", Some(WIN)),
            Pasted::OnClipboard(Left::Elevated)
        );
        assert_eq!(desk.log(), [put("hi")]);
    }

    #[test]
    fn ctrl_v_waits_for_the_hotkeys_modifiers_to_come_up() {
        let desk = Fake::default();
        desk.held.set(10);
        assert_eq!(
            paste(&desk, "hi", Some(WIN)),
            Pasted::Landed { restored: true }
        );
        assert!(desk.slept.get() >= KEYS_POLL * 10);
        assert!(desk.log().contains(&"ctrl+v".to_string()));
    }

    #[test]
    fn modifiers_held_too_long_leave_the_text_on_the_clipboard() {
        let desk = Fake::default();
        desk.held.set(u32::MAX);
        assert_eq!(
            paste(&desk, "hi", Some(WIN)),
            Pasted::OnClipboard(Left::KeysHeld)
        );
        assert_eq!(desk.log(), [put("hi")]);
        assert!(desk.slept.get() >= KEYS_WAIT);
    }

    #[test]
    fn a_target_in_the_background_is_brought_forward_first() {
        let desk = Fake {
            front: RefCell::new(vec![false]),
            ..Fake::default()
        };
        assert_eq!(
            paste(&desk, "hi", Some(WIN)),
            Pasted::Landed { restored: true }
        );
        assert_eq!(desk.log()[0], "forward");
        assert!(desk.log().contains(&"ctrl+v".to_string()));
    }

    #[test]
    fn a_target_that_wont_come_forward_gets_no_keystroke() {
        let desk = Fake {
            front: RefCell::new(vec![false]),
            comes_forward: false,
            ..Fake::default()
        };
        assert_eq!(
            paste(&desk, "hi", Some(WIN)),
            Pasted::OnClipboard(Left::NoFocus)
        );
        assert_eq!(desk.log(), ["forward", &put("hi")]);
    }

    #[test]
    fn focus_lost_after_the_write_stops_the_keystroke_and_keeps_the_text() {
        // In front for the first look, gone by the last one.
        let desk = Fake {
            front: RefCell::new(vec![true, false]),
            ..Fake::default()
        };
        assert_eq!(
            paste(&desk, "hi", Some(WIN)),
            Pasted::OnClipboard(Left::NoFocus)
        );
        assert_eq!(desk.log(), ["snapshot", &put("hi")], "no keys, no restore");
    }

    #[test]
    fn a_refused_keystroke_keeps_the_text_on_the_clipboard() {
        let desk = Fake {
            send_ok: false,
            ..Fake::default()
        };
        assert_eq!(
            paste(&desk, "hi", Some(WIN)),
            Pasted::OnClipboard(Left::KeystrokeRefused)
        );
        assert!(!desk.log().contains(&"restore".to_string()));
    }

    #[test]
    fn a_clipboard_that_cant_be_written_says_the_text_is_lost() {
        for target in [Some(WIN), None] {
            let desk = Fake {
                put_ok: false,
                ..Fake::default()
            };
            assert!(matches!(paste(&desk, "hi", target), Pasted::Lost(_)));
            assert!(!desk.log().contains(&"ctrl+v".to_string()));
        }
    }

    #[test]
    fn the_snapshot_skips_what_cannot_cross_processes() {
        for format in [
            CF_BITMAP,
            CF_ENHMETAFILE,
            CF_TEXT,
            CF_LOCALE,
            0x0081,
            0x0250,
            0x0300,
        ] {
            assert!(skipped(format), "{format:#x}");
        }
        for format in [CF_UNICODETEXT, 8, 15, 17, 0xC0FE] {
            assert!(!skipped(format), "{format:#x}");
        }
    }

    #[test]
    fn text_is_written_as_null_terminated_utf16() {
        assert_eq!(utf16z("hé"), vec![b'h', 0, 0xE9, 0, 0, 0]);
    }
}
