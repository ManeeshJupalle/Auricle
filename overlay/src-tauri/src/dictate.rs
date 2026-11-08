//! The paste layer: puts dictated text into the window it was meant for.
//!
//! Adapted from voicebox (MIT, see THIRD_PARTY_NOTICES.md): the Windows
//! clipboard snapshot and restore of its `clipboard.rs`, Ctrl+V through
//! SendInput from `synthetic_keys.rs`, and the paste sequence in `main.rs`.
//! Changed here:
//!
//! - It aims at the exact window, and the control in it, that had focus at
//!   the press. If either changed, or the window is gone, nothing is typed:
//!   the text stays on the clipboard and the user is told. It never brings a
//!   window forward.
//! - The paste is confirmed, not assumed. The overlay owns the clipboard
//!   through a message-only window and offers the text by delayed
//!   rendering, so Windows has to ask it for the text when someone reads
//!   it. Only a read that comes after Ctrl+V, from the target's own window
//!   or process, counts as the paste landing, and only then, after a short
//!   grace, is the user's clipboard put back. A read from anyone else after
//!   Ctrl+V is answered, so a paste that goes through a helper process still
//!   works, but it can't be confirmed, so the text stays and the old
//!   clipboard is not put back. With no read at all, the text is rendered
//!   for real and left on the clipboard. Either way the user's old clipboard
//!   (which might be a password) is never what gets pasted.
//! - Reads before Ctrl+V are turned away. Those come from clipboard monitors
//!   reacting to the change, and one that ignores the privacy formats would
//!   otherwise get the text, and take the read meant to confirm the paste.
//! - Text it puts on the clipboard carries the formats that keep it out of
//!   clipboard history, cloud sync and clipboard managers, set as real data
//!   at once, never delay-rendered, so they are there before anyone looks.
//! - The snapshot, the emptying and the writing happen under one open, so
//!   nothing slips in between, and a failed write puts the snapshot back.
//!   The user's clipboard is put back only if it is still ours, checked with
//!   the clipboard held open, so a copy made in the meantime is never
//!   overwritten. A snapshot that missed some formats says so.
//! - A window that runs at a higher integrity level (where Windows drops the
//!   keystroke without a word) gets nothing typed into it.
//! - Ctrl+V waits for every modifier to come up, carries scan codes for
//!   remote desktop and VM clients, and never leaves a key down.
//!
//! All of the Win32 is behind [`Desktop`], so the sequence is tested against
//! a fake, and the decisions the owner window makes are in [`Offer`], also
//! tested. No test touches the real clipboard, keyboard or windows.

use std::time::Duration;

/// How long the hotkey's modifiers may stay down before the paste gives up.
const KEYS_WAIT: Duration = Duration::from_secs(3);
const KEYS_POLL: Duration = Duration::from_millis(20);
/// Between offering the text and Ctrl+V: clipboard monitors react to the
/// change in this time, and are turned away.
const QUIET: Duration = Duration::from_millis(250);
/// How long after Ctrl+V the target gets to read the text before Auricle
/// stops waiting and leaves it on the clipboard.
const READ_WAIT: Duration = Duration::from_secs(3);
/// After the target read the text, before its clipboard goes back: room for
/// an app that reads twice.
const GRACE: Duration = Duration::from_millis(500);

/// The window that had focus when a dictation started. The pid tells it
/// apart from a later window that happens to reuse a closed one's handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub hwnd: isize,
    pub pid: u32,
    /// The control with the keyboard focus, and its process, which can be
    /// another one (a store app's content, a WebView). 0 when Windows didn't
    /// say.
    pub focus: isize,
    pub focus_pid: u32,
}

impl Target {
    /// Whether a clipboard reader, known by the window it opened the
    /// clipboard with and that window's process, is this target. A console
    /// reads through its host's process but with its own window; a reader
    /// that opened the clipboard with no window can't be told (0, 0).
    fn is_reader(&self, hwnd: isize, pid: u32) -> bool {
        (hwnd != 0 && (hwnd == self.hwnd || hwnd == self.focus))
            || (pid != 0 && (pid == self.pid || pid == self.focus_pid))
    }
}

/// Why the text was left on the clipboard rather than pasted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Left {
    NoTarget,
    TargetGone,
    Elevated,
    KeysHeld,
    Moved,
    KeystrokeRefused,
    /// Ctrl+V went, but the window never asked for the text: a read-only
    /// field, or an app too busy to read in time.
    NotRead,
}

impl Left {
    pub fn reason(&self) -> &'static str {
        match self {
            Left::NoTarget => "No window had focus when the dictation started.",
            Left::TargetGone => "The window you started in is closed or hidden.",
            Left::Elevated => {
                "That window runs with higher rights than Auricle (as administrator, say), \
                 so Auricle can't type into it."
            }
            Left::KeysHeld => "Keys were still held down, so Auricle didn't paste.",
            Left::Moved => "You moved to another window or field, so Auricle didn't paste.",
            Left::KeystrokeRefused => "Windows refused the paste keystroke.",
            Left::NotRead => "The window didn't take the paste within 3 seconds.",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pasted {
    /// The target read the text. `restored` is false when the clipboard the
    /// user had could not be put back in full; a copy they made meanwhile
    /// counts as kept.
    Landed {
        restored: bool,
    },
    /// Something other than the target read the text after Ctrl+V. It is
    /// probably in the target, but that can't be confirmed, so the text
    /// stays on the clipboard and the old clipboard is not put back.
    Unconfirmed,
    OnClipboard(Left),
    /// Neither pasted nor on the clipboard.
    Lost(String),
}

/// What the user had on the clipboard: (format, bytes) per format, and how
/// many formats it couldn't carry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub formats: Vec<(u32, Vec<u8>)>,
    pub dropped: usize,
}

/// Who read the offered text after Ctrl+V, if anyone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Read {
    Target,
    Other,
    /// Nobody, within the wait.
    Nothing,
    /// The clipboard changed hands before anyone read it.
    Replaced,
}

/// How putting the user's clipboard back went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutBack {
    Done,
    /// Some formats would not go back.
    Partly,
    /// The clipboard isn't ours any more: something copied since stays.
    LeftNewer,
    Failed,
}

/// The desktop as the paste sees it.
pub trait Desktop {
    /// The same window is still there, visible, and not minimized.
    fn is_open(&self, target: Target) -> bool;
    /// Keystrokes from this process reach it. Windows silently drops them
    /// on a window at a higher integrity level than ours.
    fn takes_our_input(&self, target: Target) -> bool;
    fn modifiers_down(&self) -> bool;
    /// The target is the foreground window, and the control focused in it
    /// is the one focused at the press.
    fn in_front(&self, target: Target) -> bool;
    /// Put `text` on the clipboard as real data, kept out of history, sync
    /// and clipboard managers.
    fn put_private(&self, text: &str) -> Result<(), String>;
    /// In one open of the clipboard: snapshot it, empty it, set the privacy
    /// formats, and offer `text` by delayed rendering for `target` to read.
    /// The snapshot is what the user had. On a failed write the snapshot is
    /// put back and the error returned.
    fn offer(&self, text: &str, target: Target) -> Result<Snapshot, String>;
    /// The offered text is still what the clipboard holds.
    fn still_ours(&self) -> bool;
    /// Ctrl+V. From the moment before it goes, a read can be the paste.
    fn send_ctrl_v(&self) -> Result<(), String>;
    fn wait_for_read(&self, wait: Duration) -> Read;
    /// Render the offered text for real, so it stays on the clipboard. False
    /// when the clipboard is no longer ours, so the text isn't there.
    fn keep(&self) -> bool;
    /// Put `snapshot` back, if the clipboard is still ours.
    fn restore(&self, snapshot: &Snapshot) -> PutBack;
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
    if !desk.in_front(target) {
        return leave(desk, text, Left::Moved);
    }
    let snapshot = match desk.offer(text, target) {
        Ok(snapshot) => snapshot,
        Err(e) => return Pasted::Lost(e),
    };
    desk.sleep(QUIET);
    // A last look right before the keystroke.
    if !desk.in_front(target) {
        return kept(desk, Left::Moved);
    }
    if desk.modifiers_down() {
        return kept(desk, Left::KeysHeld);
    }
    // Ctrl+V now would paste whatever replaced it.
    if !desk.still_ours() {
        return Pasted::Lost(REPLACED.to_string());
    }
    if desk.send_ctrl_v().is_err() {
        return kept(desk, Left::KeystrokeRefused);
    }
    match desk.wait_for_read(READ_WAIT) {
        Read::Target => {
            desk.sleep(GRACE);
            let restored = match desk.restore(&snapshot) {
                PutBack::Done => snapshot.dropped == 0,
                PutBack::LeftNewer => true,
                PutBack::Partly | PutBack::Failed => false,
            };
            Pasted::Landed { restored }
        }
        Read::Other => Pasted::Unconfirmed,
        Read::Nothing => kept(desk, Left::NotRead),
        Read::Replaced => Pasted::Lost(REPLACED.to_string()),
    }
}

const REPLACED: &str = "Something else was copied before the paste was read.";

fn leave(desk: &impl Desktop, text: &str, why: Left) -> Pasted {
    match desk.put_private(text) {
        Ok(()) => Pasted::OnClipboard(why),
        Err(e) => Pasted::Lost(e),
    }
}

/// The text is on offer but won't be pasted: make it stay.
fn kept(desk: &impl Desktop, why: Left) -> Pasted {
    if desk.keep() {
        Pasted::OnClipboard(why)
    } else {
        Pasted::Lost(REPLACED.to_string())
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

/// The text on offer, as the owner window's thread sees it. Every decision
/// that window makes is here, so it is tested without a clipboard.
#[derive(Debug, Default)]
#[cfg_attr(not(windows), allow(dead_code))]
struct Offer {
    target: Option<Target>,
    /// UTF-16 text not yet handed to anyone.
    pending: Option<Vec<u8>>,
    /// Ctrl+V is on its way: a read from now on can be the paste.
    armed: bool,
    read: Option<Read>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Offer {
    const fn none() -> Offer {
        Offer {
            target: None,
            pending: None,
            armed: false,
            read: None,
        }
    }

    fn new(text: Vec<u8>, target: Target) -> Offer {
        Offer {
            target: Some(target),
            pending: Some(text),
            ..Offer::none()
        }
    }

    /// Someone asked for the text; `hwnd` and `pid` are the window they
    /// opened the clipboard with and its process, 0 when they used none.
    /// The bytes to hand over, or None to turn them away.
    fn on_request(&mut self, hwnd: isize, pid: u32) -> Option<Vec<u8>> {
        if !self.armed {
            return None;
        }
        let text = self.pending.take()?;
        let ours = self.target.is_some_and(|t| t.is_reader(hwnd, pid));
        self.read = Some(if ours { Read::Target } else { Read::Other });
        Some(text)
    }

    /// The text not yet handed out, to render for real.
    fn take(&mut self) -> Option<Vec<u8>> {
        self.pending.take()
    }
}

/// The foreground window and its focused control, read the moment the
/// hotkey goes down, before anything can move focus.
#[cfg(windows)]
pub fn foreground_target() -> Option<Target> {
    win::foreground_target()
}

#[cfg(not(windows))]
pub fn foreground_target() -> Option<Target> {
    None
}

#[cfg(windows)]
pub fn paste_text(text: &str, target: Option<Target>) -> Pasted {
    win::paste_text(text, target)
}

#[cfg(not(windows))]
pub fn paste_text(_text: &str, _target: Option<Target>) -> Pasted {
    Pasted::Lost("Pasting needs Windows.".to_string())
}

/// The overlay is exiting: text still on offer is rendered for real, so a
/// paste cut short leaves it on the clipboard rather than losing it.
#[cfg(windows)]
pub fn flush() {
    win::flush();
}

#[cfg(not(windows))]
pub fn flush() {}

/// The formats that keep an item out of clipboard history, cloud sync and
/// clipboard managers. Per Microsoft's "Cloud Clipboard and Clipboard
/// History Formats": any data in the first keeps the item from both; a DWORD
/// of 0 in the next two keeps it from history and from sync respectively.
/// The last is the convention clipboard managers such as Ditto honour. All
/// are set, as real data, in case one is ever read without the others.
#[cfg_attr(not(windows), allow(dead_code))]
const PRIVATE_FORMATS: [&str; 4] = [
    "ExcludeClipboardContentFromMonitorProcessing",
    "CanIncludeInClipboardHistory",
    "CanUploadToCloudClipboard",
    "Clipboard Viewer Ignore",
];

const CF_TEXT: u32 = 1;
const CF_BITMAP: u32 = 2;
const CF_METAFILEPICT: u32 = 3;
const CF_OEMTEXT: u32 = 7;
const CF_PALETTE: u32 = 9;
const CF_UNICODETEXT: u32 = 13;
const CF_ENHMETAFILE: u32 = 14;
const CF_LOCALE: u32 = 16;

/// Formats Windows rebuilds from another one on the clipboard (the text
/// variants from CF_UNICODETEXT, the bitmap and palette from the DIB), so a
/// snapshot needn't carry them.
#[cfg_attr(not(windows), allow(dead_code))]
fn rebuilt(format: u32) -> bool {
    matches!(
        format,
        CF_TEXT | CF_OEMTEXT | CF_LOCALE | CF_BITMAP | CF_PALETTE
    )
}

/// Formats a snapshot can't carry across: metafiles are GDI handles,
/// owner-display ones are drawn by their owner, and private ones mean
/// nothing outside their app. They count as lost from the snapshot.
#[cfg_attr(not(windows), allow(dead_code))]
fn unportable(format: u32) -> bool {
    matches!(format, CF_METAFILEPICT | CF_ENHMETAFILE)
        || (0x0080..=0x008E).contains(&format)
        || (0x0200..=0x03FF).contains(&format)
}

/// The most a snapshot holds. A format past it is dropped rather than risk
/// an allocation that takes the overlay down.
#[cfg_attr(not(windows), allow(dead_code))]
const SNAPSHOT_CAP: usize = 128 << 20;

/// `text` as CF_UNICODETEXT wants it: UTF-16 with a terminating null.
fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
enum Key {
    Ctrl,
    V,
}

/// Ctrl+V as key events: (key, up).
#[cfg_attr(not(windows), allow(dead_code))]
const CTRL_V: [(Key, bool); 4] = [
    (Key::Ctrl, false),
    (Key::V, false),
    (Key::V, true),
    (Key::Ctrl, true),
];

/// The keys left down when only the first `sent` of `keys` went in, to be
/// let up last-pressed first.
#[cfg_attr(not(windows), allow(dead_code))]
fn owed_ups(keys: &[(Key, bool)], sent: usize) -> Vec<Key> {
    let mut down = Vec::new();
    for &(key, up) in &keys[..sent.min(keys.len())] {
        if up {
            down.retain(|k| *k != key);
        } else {
            down.push(key);
        }
    }
    down.reverse();
    down
}

#[cfg(windows)]
mod win {
    use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
    use std::time::{Duration, Instant};

    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, GlobalFree, SetLastError, HANDLE, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT,
        WIN32_ERROR, WPARAM,
    };
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardOwner,
        GetOpenClipboardWindow, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
        KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN,
        VK_SHIFT, VK_V,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetForegroundWindow, GetGUIThreadInfo,
        GetMessageW, GetWindowThreadProcessId, IsHungAppWindow, IsIconic, IsWindow,
        IsWindowVisible, PeekMessageW, RegisterClassExW, SendMessageTimeoutW, GUITHREADINFO,
        HWND_MESSAGE, MSG, PM_NOREMOVE, SMTO_ABORTIFHUNG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
        WM_DESTROYCLIPBOARD, WM_RENDERALLFORMATS, WM_RENDERFORMAT, WNDCLASSEXW,
    };

    use super::{
        owed_ups, paste, rebuilt, unportable, utf16z, Desktop, Key, Offer, Pasted, PutBack, Read,
        Snapshot, Target, CF_UNICODETEXT, CTRL_V, PRIVATE_FORMATS, SNAPSHOT_CAP,
    };

    /// The text on offer, shared with the owner window's thread. Never held
    /// across a clipboard call that may message that window.
    static OFFER: Mutex<Offer> = Mutex::new(Offer::none());
    /// Woken when a read may have happened or the clipboard changed hands.
    static CHANGED: Condvar = Condvar::new();
    /// The owner window, made on the first paste and kept for the overlay's
    /// life, on a thread that does nothing but answer its messages.
    static OWNER: OnceLock<Result<isize, String>> = OnceLock::new();
    /// One paste at a time, so two never share the offer.
    static PASTING: Mutex<()> = Mutex::new(());

    /// Sent at exit: render what is still on offer.
    const WM_FLUSH: u32 = WM_APP + 1;
    const CLASS: PCWSTR = w!("AuricleDictationClipboard");
    /// How long to keep trying for a clipboard another program has open.
    const OPEN_FOR: Duration = Duration::from_secs(2);
    const OPEN_RETRY: Duration = Duration::from_millis(15);
    /// Losing the clipboard sends WM_DESTROYCLIPBOARD, but that can't be
    /// matched to the copy it is about (ours, an earlier one), so a wait
    /// also looks at the owner this often.
    const OWNER_POLL: Duration = Duration::from_millis(25);

    fn lock() -> MutexGuard<'static, Offer> {
        OFFER.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn handle(raw: isize) -> HWND {
        HWND(raw as *mut core::ffi::c_void)
    }

    fn hwnd(target: Target) -> HWND {
        handle(target.hwnd)
    }

    pub fn pid_of(hwnd: HWND) -> u32 {
        let mut pid = 0u32;
        // SAFETY: a plain handle in, the pid written to a local.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        pid
    }

    /// The window with the keyboard focus on `thread`'s input queue.
    fn focus_of(thread: u32) -> HWND {
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: fills a local of the size given.
        match unsafe { GetGUIThreadInfo(thread, &mut info) } {
            Ok(()) => info.hwndFocus,
            Err(_) => HWND::default(),
        }
    }

    pub fn foreground_target() -> Option<Target> {
        // SAFETY: takes and returns plain window handles.
        let front = unsafe { GetForegroundWindow() };
        if front.is_invalid() {
            return None;
        }
        let mut pid = 0u32;
        // SAFETY: as pid_of.
        let thread = unsafe { GetWindowThreadProcessId(front, Some(&mut pid)) };
        let focus = focus_of(thread);
        Some(Target {
            hwnd: front.0 as isize,
            pid,
            focus: focus.0 as isize,
            focus_pid: if focus.is_invalid() { 0 } else { pid_of(focus) },
        })
    }

    pub fn paste_text(text: &str, target: Option<Target>) -> Pasted {
        let _one = PASTING.lock().unwrap_or_else(|e| e.into_inner());
        match owner() {
            Ok(owner) => paste(&Win32 { owner }, text, target),
            Err(e) => Pasted::Lost(e),
        }
    }

    pub fn flush() {
        let Some(Ok(owner)) = OWNER.get() else {
            return;
        };
        // SAFETY: our own window; the call gives up if its thread is stuck.
        unsafe {
            let _ = SendMessageTimeoutW(
                handle(*owner),
                WM_FLUSH,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                3000,
                None,
            );
        }
    }

    fn owner() -> Result<HWND, String> {
        let made = OWNER.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("clipboard-owner".into())
                .spawn(move || {
                    let made = make_window();
                    let ok = made.is_ok();
                    let _ = tx.send(made.map(|h| h.0 as isize));
                    if ok {
                        pump();
                    }
                })
                .map_err(|e| e.to_string())?;
            rx.recv().map_err(|e| e.to_string())?
        });
        made.clone().map(handle)
    }

    fn make_window() -> Result<HWND, String> {
        let fail = |e: windows::core::Error| format!("the clipboard window could not be made: {e}");
        // SAFETY: registers a class whose window procedure is below and
        // makes one message-only window of it, owned by this thread.
        unsafe {
            let instance = HINSTANCE(GetModuleHandleW(PCWSTR::null()).map_err(fail)?.0);
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: CLASS,
                ..Default::default()
            };
            if RegisterClassExW(&class) == 0 {
                return Err(fail(windows::core::Error::from_thread()));
            }
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS,
                w!(""),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance),
                None,
            )
            .map_err(fail)
        }
    }

    fn pump() {
        let mut msg = MSG::default();
        // SAFETY: the message loop of the thread that owns the window.
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                DispatchMessageW(&msg);
            }
        }
    }

    unsafe extern "system" fn window_proc(
        window: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_RENDERFORMAT => {
                if wparam.0 as u32 == CF_UNICODETEXT {
                    render_for_reader();
                }
                LRESULT(0)
            }
            // Before this window goes, or before the overlay exits.
            WM_RENDERALLFORMATS | WM_FLUSH => {
                settle(window, true);
                LRESULT(0)
            }
            WM_DESTROYCLIPBOARD => {
                CHANGED.notify_all();
                LRESULT(0)
            }
            // SAFETY: the default handling for everything else.
            _ => unsafe { DefWindowProcW(window, msg, wparam, lparam) },
        }
    }

    /// Someone holds the clipboard open and asked for the text. Per
    /// WM_RENDERFORMAT's rules, the clipboard is not opened here.
    fn render_for_reader() {
        // SAFETY: reads which window the reader opened the clipboard with.
        let reader = unsafe { GetOpenClipboardWindow() }.unwrap_or_default();
        let pid = if reader.is_invalid() {
            0
        } else {
            pid_of(reader)
        };
        let text = lock().on_request(reader.0 as isize, pid);
        if let Some(text) = text {
            let _ = put(CF_UNICODETEXT, &text);
        }
        CHANGED.notify_all();
    }

    /// Render the text still on offer for real. False when the clipboard is
    /// no longer ours. On the owner's own thread, `pumping` answers a reader
    /// that is waiting on this window while the clipboard is being opened.
    fn settle(owner: HWND, pumping: bool) -> bool {
        let Ok(_open) = Open::clipboard(owner, pumping) else {
            return false;
        };
        if !owns(owner) {
            return false;
        }
        let text = lock().take();
        match text {
            Some(text) => put(CF_UNICODETEXT, &text).is_ok(),
            None => true,
        }
    }

    fn owns(owner: HWND) -> bool {
        // SAFETY: reads the owner handle.
        unsafe { GetClipboardOwner() }.is_ok_and(|o| o == owner)
    }

    /// The integrity level of `process`: the last part of its token's label.
    fn integrity(process: HANDLE) -> Option<u32> {
        // SAFETY: the token is ours and closed here; the label is read from
        // an aligned local big enough for it and its SID.
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
            let mut buf = [0u64; 16];
            let mut len = 0u32;
            let read = GetTokenInformation(
                token,
                TokenIntegrityLevel,
                Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                std::mem::size_of_val(&buf) as u32,
                &mut len,
            );
            let _ = CloseHandle(token);
            read.ok()?;
            let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            let count = GetSidSubAuthorityCount(label.Label.Sid);
            if count.is_null() || *count == 0 {
                return None;
            }
            let rid = GetSidSubAuthority(label.Label.Sid, *count as u32 - 1);
            (!rid.is_null()).then(|| *rid)
        }
    }

    fn integrity_of(pid: u32) -> Option<u32> {
        // SAFETY: query-only access, closed below.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        let level = integrity(process);
        // SAFETY: opened above.
        unsafe {
            let _ = CloseHandle(process);
        }
        level
    }

    /// The clipboard, open until this is dropped.
    struct Open;

    impl Open {
        fn clipboard(owner: HWND, pumping: bool) -> Result<Open, String> {
            let until = Instant::now() + OPEN_FOR;
            loop {
                // SAFETY: opened for our window; closed again on drop.
                if unsafe { OpenClipboard(Some(owner)) }.is_ok() {
                    return Ok(Open);
                }
                if Instant::now() >= until {
                    return Err("Another program is holding the clipboard.".to_string());
                }
                if pumping {
                    let mut msg = MSG::default();
                    // SAFETY: lets messages sent to this thread through.
                    unsafe {
                        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
                    }
                }
                std::thread::sleep(OPEN_RETRY);
            }
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

    /// One format's bytes off the open clipboard, if it has plain memory and
    /// fits in `room`.
    fn read(format: u32, room: usize) -> Option<Vec<u8>> {
        // SAFETY: the clipboard is open; the memory is read under its lock
        // and copied out before the unlock.
        unsafe {
            let handle = GetClipboardData(format).ok()?;
            if handle.is_invalid() {
                return None;
            }
            let global = HGLOBAL(handle.0);
            let size = GlobalSize(global);
            if size > room {
                return None;
            }
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

    /// Everything the open clipboard holds that can be carried.
    fn snapshot() -> Snapshot {
        let mut snap = Snapshot::default();
        // A hung owner never answers for data it renders late, and asking
        // would stall here with the clipboard held, so nothing is read.
        // SAFETY: reads the owner and asks whether it responds.
        let hung =
            unsafe { GetClipboardOwner() }.is_ok_and(|o| unsafe { IsHungAppWindow(o) }.as_bool());
        let mut room = SNAPSHOT_CAP;
        let mut format = 0;
        loop {
            // SAFETY: the clipboard is open.
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                break;
            }
            if rebuilt(format) {
                continue;
            }
            if hung || unportable(format) {
                snap.dropped += 1;
                continue;
            }
            match read(format, room) {
                Some(bytes) => {
                    room -= bytes.len();
                    snap.formats.push((format, bytes));
                }
                None => snap.dropped += 1,
            }
        }
        snap
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

    /// Offer `format` by delayed rendering on the open clipboard.
    fn put_later(format: u32) -> Result<(), String> {
        // SAFETY: a null handle asks for delayed rendering. Success returns
        // that null handle, which the wrapper reads as an error, so the last
        // error, cleared first, tells them apart.
        unsafe {
            SetLastError(WIN32_ERROR(0));
            match SetClipboardData(format, None) {
                Ok(_) => Ok(()),
                Err(e) if e.code().is_ok() => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        }
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

    /// Snapshot, empty and write the clipboard under one open: the privacy
    /// formats, then the text, real or (with `offer`) on offer for that
    /// target. A failed write puts the snapshot back.
    fn write(owner: HWND, text: &str, offer: Option<Target>) -> Result<Snapshot, String> {
        let private = PRIVATE_FORMATS
            .iter()
            .map(|name| register(name))
            .collect::<Result<Vec<_>, _>>()?;
        let _open = Open::clipboard(owner, false)?;
        let snapshot = snapshot();
        // SAFETY: the clipboard is open, for our window, which becomes the
        // owner. The lock on the offer is not held across this, as it may
        // message that window.
        unsafe { EmptyClipboard() }.map_err(|e| e.to_string())?;
        let written = fill(&private, utf16z(text), offer);
        if written.is_err() {
            *lock() = Offer::none();
            // SAFETY: still open; rather the user's clipboard than neither.
            unsafe {
                let _ = EmptyClipboard();
            }
            for (format, bytes) in &snapshot.formats {
                let _ = put(*format, bytes);
            }
        }
        written.map(|()| snapshot)
    }

    fn fill(private: &[u32], text: Vec<u8>, offer: Option<Target>) -> Result<(), String> {
        // Ahead of the text, so nothing ever sees the text without them.
        for format in private {
            put(*format, &0u32.to_le_bytes())?;
        }
        match offer {
            Some(target) => {
                // No one can ask for it before the clipboard is closed.
                *lock() = Offer::new(text, target);
                put_later(CF_UNICODETEXT)
            }
            None => {
                *lock() = Offer::none();
                put(CF_UNICODETEXT, &text)
            }
        }
    }

    fn key(key: Key, up: bool) -> INPUT {
        let vk = match key {
            Key::Ctrl => VK_CONTROL,
            Key::V => VK_V,
        };
        // Remote desktop and VM clients pass the scan code on, not the key.
        // SAFETY: a lookup in the keyboard layout.
        let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
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

    fn send(keys: &[(Key, bool)]) -> usize {
        let events: Vec<INPUT> = keys.iter().map(|&(k, up)| key(k, up)).collect();
        // SAFETY: a slice of fully built keyboard inputs.
        unsafe { SendInput(&events, std::mem::size_of::<INPUT>() as i32) as usize }
    }

    struct Win32 {
        owner: HWND,
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
            let Some(ours) = integrity(unsafe { GetCurrentProcess() }) else {
                return false;
            };
            let focus = (target.focus_pid != 0 && target.focus_pid != target.pid)
                .then_some(target.focus_pid);
            std::iter::once(target.pid)
                .chain(focus)
                .all(|pid| integrity_of(pid).is_some_and(|theirs| theirs <= ours))
        }

        fn modifiers_down(&self) -> bool {
            [VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN]
                .iter()
                // SAFETY: reads one key's state.
                .any(|vk| unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000 != 0)
        }

        fn in_front(&self, target: Target) -> bool {
            // SAFETY: reads the foreground handle and its thread.
            let front = unsafe { GetForegroundWindow() };
            if front != hwnd(target) || pid_of(front) != target.pid {
                return false;
            }
            // SAFETY: as above.
            let thread = unsafe { GetWindowThreadProcessId(front, None) };
            // Only another control counts as moved: a focus Windows doesn't
            // report gets no keystroke's worth of text anyway.
            let now = focus_of(thread);
            target.focus == 0 || now.is_invalid() || now.0 as isize == target.focus
        }

        fn put_private(&self, text: &str) -> Result<(), String> {
            write(self.owner, text, None).map(|_| ())
        }

        fn offer(&self, text: &str, target: Target) -> Result<Snapshot, String> {
            write(self.owner, text, Some(target))
        }

        fn still_ours(&self) -> bool {
            owns(self.owner)
        }

        fn send_ctrl_v(&self) -> Result<(), String> {
            // Before the keys go: the target may read before SendInput returns.
            lock().armed = true;
            let sent = send(&CTRL_V);
            if sent == CTRL_V.len() {
                return Ok(());
            }
            let ups: Vec<(Key, bool)> = owed_ups(&CTRL_V, sent)
                .into_iter()
                .map(|k| (k, true))
                .collect();
            if !ups.is_empty() {
                send(&ups);
            }
            Err(format!(
                "SendInput delivered {sent} of {} keys",
                CTRL_V.len()
            ))
        }

        fn wait_for_read(&self, wait: Duration) -> Read {
            let until = Instant::now() + wait;
            let mut offer = lock();
            loop {
                if let Some(read) = offer.read {
                    return read;
                }
                if !owns(self.owner) {
                    return Read::Replaced;
                }
                let now = Instant::now();
                if now >= until {
                    return Read::Nothing;
                }
                offer = CHANGED
                    .wait_timeout(offer, (until - now).min(OWNER_POLL))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }

        fn keep(&self) -> bool {
            settle(self.owner, false)
        }

        fn restore(&self, snapshot: &Snapshot) -> PutBack {
            let Ok(_open) = Open::clipboard(self.owner, false) else {
                return PutBack::Failed;
            };
            // Checked with the clipboard held, so no copy can land between
            // the look and the overwrite.
            if !owns(self.owner) {
                return PutBack::LeftNewer;
            }
            // SAFETY: the clipboard is open.
            if unsafe { EmptyClipboard() }.is_err() {
                return PutBack::Failed;
            }
            let mut all = true;
            for (format, bytes) in &snapshot.formats {
                // One odd format must not cost the user the rest.
                all &= put(*format, bytes).is_ok();
            }
            if all {
                PutBack::Done
            } else {
                PutBack::Partly
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

    const WIN: Target = Target {
        hwnd: 42,
        pid: 7,
        focus: 43,
        focus_pid: 7,
    };

    /// A desktop made of switches. `log` records what touched the clipboard
    /// or the keyboard, in order.
    struct Fake {
        open: bool,
        input_ok: bool,
        /// Polls for which a modifier still reads as down.
        held: Cell<u32>,
        /// Polls of in_front, in order; the last one repeats.
        front: RefCell<Vec<bool>>,
        snapshot: Snapshot,
        put_ok: bool,
        offer_ok: bool,
        send_ok: bool,
        read: Read,
        /// The clipboard is still ours, for still_ours() and keep().
        still_ours: bool,
        put_back: PutBack,
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
                snapshot: Snapshot {
                    formats: vec![(CF_UNICODETEXT, utf16z("the user's"))],
                    dropped: 0,
                },
                put_ok: true,
                offer_ok: true,
                send_ok: true,
                read: Read::Target,
                still_ours: true,
                put_back: PutBack::Done,
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
        fn in_front(&self, _: Target) -> bool {
            let mut front = self.front.borrow_mut();
            if front.len() > 1 {
                front.remove(0)
            } else {
                front[0]
            }
        }
        fn put_private(&self, text: &str) -> Result<(), String> {
            self.note(&format!("put {text}"));
            if self.put_ok {
                Ok(())
            } else {
                Err("Another program is holding the clipboard.".to_string())
            }
        }
        fn offer(&self, text: &str, _: Target) -> Result<Snapshot, String> {
            self.note(&format!("offer {text}"));
            if self.offer_ok {
                Ok(self.snapshot.clone())
            } else {
                Err("Another program is holding the clipboard.".to_string())
            }
        }
        fn still_ours(&self) -> bool {
            self.still_ours
        }
        fn send_ctrl_v(&self) -> Result<(), String> {
            self.note("ctrl+v");
            if self.send_ok {
                Ok(())
            } else {
                Err("refused".to_string())
            }
        }
        fn wait_for_read(&self, wait: Duration) -> Read {
            assert_eq!(wait, READ_WAIT);
            self.note("wait");
            self.read
        }
        fn keep(&self) -> bool {
            self.note("keep");
            self.still_ours
        }
        fn restore(&self, snapshot: &Snapshot) -> PutBack {
            assert_eq!(snapshot, &self.snapshot, "puts back what it took");
            self.note("restore");
            self.put_back
        }
        fn sleep(&self, how_long: Duration) {
            self.slept.set(self.slept.get() + how_long);
        }
    }

    fn put(text: &str) -> String {
        format!("put {text}")
    }

    fn offer(text: &str) -> String {
        format!("offer {text}")
    }

    #[test]
    fn it_puts_the_clipboard_back_only_once_the_target_has_read_the_text() {
        let desk = Fake::default();
        let out = paste(&desk, "hello", Some(WIN));
        assert_eq!(out, Pasted::Landed { restored: true });
        assert_eq!(
            desk.log(),
            [&offer("hello"), "ctrl+v", "wait", "restore"],
            "offered, never written outright"
        );
        assert!(desk.slept.get() >= QUIET + GRACE);
    }

    #[test]
    fn no_read_leaves_the_text_on_the_clipboard_and_the_old_one_gone() {
        let desk = Fake {
            read: Read::Nothing,
            ..Fake::default()
        };
        assert_eq!(
            paste(&desk, "hi", Some(WIN)),
            Pasted::OnClipboard(Left::NotRead)
        );
        assert_eq!(desk.log(), [&offer("hi"), "ctrl+v", "wait", "keep"]);
    }

    #[test]
    fn a_read_by_someone_else_is_not_taken_as_the_paste() {
        let desk = Fake {
            read: Read::Other,
            ..Fake::default()
        };
        assert_eq!(paste(&desk, "hi", Some(WIN)), Pasted::Unconfirmed);
        assert!(
            !desk.log().contains(&"restore".to_string()),
            "the target may read later: the old clipboard must not be there"
        );
    }

    #[test]
    fn a_copy_made_before_the_read_is_left_alone_and_the_loss_is_said() {
        let desk = Fake {
            read: Read::Replaced,
            ..Fake::default()
        };
        let out = paste(&desk, "hi", Some(WIN));
        assert_eq!(out, Pasted::Lost(REPLACED.to_string()));
        let log = desk.log();
        assert!(!log.contains(&"restore".to_string()));
        assert!(!log.contains(&"keep".to_string()));
    }

    #[test]
    fn a_copy_made_after_the_read_counts_as_kept() {
        let desk = Fake {
            put_back: PutBack::LeftNewer,
            ..Fake::default()
        };
        assert_eq!(
            paste(&desk, "hi", Some(WIN)),
            Pasted::Landed { restored: true }
        );
    }

    #[test]
    fn a_clipboard_that_went_back_in_part_says_so() {
        for (dropped, put_back) in [
            (1, PutBack::Done),
            (0, PutBack::Partly),
            (0, PutBack::Failed),
        ] {
            let desk = Fake {
                snapshot: Snapshot {
                    formats: vec![],
                    dropped,
                },
                put_back,
                ..Fake::default()
            };
            assert_eq!(
                paste(&desk, "hi", Some(WIN)),
                Pasted::Landed { restored: false },
                "{dropped} {put_back:?}"
            );
        }
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
        assert_eq!(desk.log(), [put("hi")], "no keys");
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
    fn a_target_that_lost_the_foreground_or_its_field_is_never_brought_back() {
        let desk = Fake {
            front: RefCell::new(vec![false]),
            ..Fake::default()
        };
        assert_eq!(
            paste(&desk, "hi", Some(WIN)),
            Pasted::OnClipboard(Left::Moved)
        );
        assert_eq!(desk.log(), [put("hi")]);
    }

    #[test]
    fn focus_lost_after_the_offer_stops_the_keystroke_and_keeps_the_text() {
        // In front for the first look, gone by the last one.
        let desk = Fake {
            front: RefCell::new(vec![true, false]),
            ..Fake::default()
        };
        assert_eq!(
            paste(&desk, "hi", Some(WIN)),
            Pasted::OnClipboard(Left::Moved)
        );
        assert_eq!(desk.log(), [&offer("hi"), "keep"], "no keys, no restore");
    }

    #[test]
    fn text_that_cant_be_kept_after_a_late_copy_says_it_is_gone() {
        let desk = Fake {
            front: RefCell::new(vec![true, false]),
            still_ours: false,
            ..Fake::default()
        };
        assert_eq!(
            paste(&desk, "hi", Some(WIN)),
            Pasted::Lost(REPLACED.to_string())
        );
    }

    #[test]
    fn a_clipboard_taken_over_before_ctrl_v_gets_no_keystroke() {
        let desk = Fake {
            still_ours: false,
            ..Fake::default()
        };
        assert_eq!(
            paste(&desk, "hi", Some(WIN)),
            Pasted::Lost(REPLACED.to_string())
        );
        assert_eq!(desk.log(), [offer("hi")], "the replacement is not pasted");
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
        assert_eq!(desk.log(), [&offer("hi"), "ctrl+v", "keep"]);
    }

    #[test]
    fn a_clipboard_that_cant_be_written_says_the_text_is_lost() {
        let desk = Fake {
            offer_ok: false,
            ..Fake::default()
        };
        assert!(matches!(paste(&desk, "hi", Some(WIN)), Pasted::Lost(_)));
        assert!(!desk.log().contains(&"ctrl+v".to_string()));
        let desk = Fake {
            put_ok: false,
            ..Fake::default()
        };
        assert!(matches!(paste(&desk, "hi", None), Pasted::Lost(_)));
    }

    fn armed(target: Target) -> Offer {
        let mut offer = Offer::new(utf16z("words"), target);
        offer.armed = true;
        offer
    }

    #[test]
    fn reads_before_ctrl_v_are_turned_away_and_the_text_still_waits() {
        let mut offer = Offer::new(utf16z("words"), WIN);
        // A clipboard monitor, then the target's own app reacting to the change.
        assert_eq!(offer.on_request(900, 99), None);
        assert_eq!(offer.on_request(WIN.hwnd, WIN.pid), None);
        assert_eq!(offer.read, None);
        offer.armed = true;
        assert_eq!(offer.on_request(WIN.hwnd, WIN.pid), Some(utf16z("words")));
        assert_eq!(offer.read, Some(Read::Target));
    }

    #[test]
    fn the_target_is_known_by_its_window_its_field_or_their_processes() {
        let store_app = Target {
            focus: 60,
            focus_pid: 61,
            ..WIN
        };
        for (hwnd, pid) in [
            (WIN.hwnd, 0),              // a console host reading with the console window
            (0, store_app.pid),         // a helper window of the app's own process
            (store_app.focus, 0),       // the focused control itself
            (500, store_app.focus_pid), // the content's process, as a WebView's
        ] {
            let mut offer = armed(store_app);
            assert!(offer.on_request(hwnd, pid).is_some());
            assert_eq!(offer.read, Some(Read::Target), "{hwnd} {pid}");
        }
    }

    #[test]
    fn a_reader_that_cant_be_told_gets_the_text_but_no_credit() {
        // Another process, and one that opened the clipboard with no window.
        for (hwnd, pid) in [(900, 99), (0, 0)] {
            let mut offer = armed(WIN);
            assert_eq!(offer.on_request(hwnd, pid), Some(utf16z("words")));
            assert_eq!(offer.read, Some(Read::Other));
        }
    }

    #[test]
    fn the_text_is_handed_out_once_and_then_there_is_nothing_left_to_flush() {
        let mut offer = armed(WIN);
        assert!(offer.on_request(WIN.hwnd, WIN.pid).is_some());
        assert_eq!(offer.on_request(WIN.hwnd, WIN.pid), None);
        assert_eq!(offer.take(), None);
    }

    #[test]
    fn text_still_on_offer_at_exit_is_rendered_not_lost() {
        let mut offer = Offer::new(utf16z("words"), WIN);
        assert_eq!(offer.take(), Some(utf16z("words")));
        assert_eq!(offer.on_request(WIN.hwnd, WIN.pid), None);
        assert_eq!(Offer::none().take(), None);
    }

    #[test]
    fn a_partly_sent_ctrl_v_lets_every_key_it_pressed_up() {
        assert_eq!(owed_ups(&CTRL_V, 0), []);
        assert_eq!(owed_ups(&CTRL_V, 1), [Key::Ctrl]);
        assert_eq!(owed_ups(&CTRL_V, 2), [Key::V, Key::Ctrl]);
        assert_eq!(owed_ups(&CTRL_V, 3), [Key::Ctrl]);
        assert_eq!(owed_ups(&CTRL_V, 4), []);
    }

    #[test]
    fn the_snapshot_skips_what_windows_rebuilds_and_counts_what_it_cant_carry() {
        for format in [CF_TEXT, CF_OEMTEXT, CF_LOCALE, CF_BITMAP, CF_PALETTE] {
            assert!(rebuilt(format), "{format:#x}");
            assert!(!unportable(format), "{format:#x}");
        }
        for format in [CF_ENHMETAFILE, CF_METAFILEPICT, 0x0081, 0x0250, 0x0300] {
            assert!(unportable(format), "{format:#x}");
        }
        // CF_UNICODETEXT, CF_DIB, CF_HDROP, CF_DIBV5, a registered format.
        for format in [CF_UNICODETEXT, 8, 15, 17, 0xC0FE] {
            assert!(!rebuilt(format) && !unportable(format), "{format:#x}");
        }
    }

    #[test]
    fn text_is_written_as_null_terminated_utf16() {
        assert_eq!(utf16z("hé"), vec![b'h', 0, 0xE9, 0, 0, 0]);
    }
}
