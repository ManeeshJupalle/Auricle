//! The dictation pill: a small always-on-top window saying what a dictation
//! is doing. It must never take focus from the app being dictated into, or
//! the paste would land in the pill. So it is built non-focusable
//! (WS_EX_NOACTIVATE) and click-through, it is shown with SW_SHOWNOACTIVATE
//! rather than Tauri's show, and every show checks the foreground after.
//!
//! It sits at the top centre of the screen the user is working on, or at
//! the bottom when the caret is in the top half, so it stays off the line
//! being written.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde::Serialize;
use tauri::{Emitter, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder};

use crate::config::DictationMode;
use crate::dictation::{Pill, PASTING};

const LABEL: &str = "pill";
/// Room for three lines of the longest message; the window is transparent
/// and click-through around the pill itself.
const WIDTH: f64 = 460.0;
const HEIGHT: f64 = 88.0;
const MARGIN: f64 = 8.0;
const DONE_FOR: Duration = Duration::from_millis(1500);

/// What the pill's page renders.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct View {
    pub kind: &'static str,
    pub text: String,
    pub hint: Option<String>,
    pub theme: String,
}

/// The pill's own state, managed by the app.
pub struct PillState {
    theme: String,
    /// Bumped by every show or hide, so a timed hide knows it is stale.
    generation: AtomicU64,
    shown: AtomicBool,
    /// Set if a show ever took focus despite everything: from then on the
    /// pill stays hidden rather than risk it again.
    off: AtomicBool,
}

/// Build the pill hidden, once at startup, so that showing it is instant.
pub fn build(app: &tauri::AppHandle, theme: &str) -> tauri::Result<()> {
    app.manage(PillState {
        theme: theme.to_string(),
        generation: AtomicU64::new(0),
        shown: AtomicBool::new(false),
        off: AtomicBool::new(false),
    });
    let win = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("pill.html".into()))
        .title("Auricle dictation")
        .inner_size(WIDTH, HEIGHT)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focusable(false)
        .focused(false)
        .visible(false)
        .build()?;
    win.set_ignore_cursor_events(true)
}

/// The words for `pill`, or None when it should be hidden.
pub fn view(
    pill: &Pill,
    mode: DictationMode,
    hotkey: &str,
    esc: bool,
    theme: &str,
) -> Option<View> {
    let esc_hint = esc.then(|| "Esc cancels".to_string());
    let (kind, text, hint) = match pill {
        Pill::Hidden => return None,
        Pill::Listening => {
            let hint = match mode {
                // Esc does nothing while the hotkey's modifiers are held.
                DictationMode::Hold => "Let go to paste".to_string(),
                DictationMode::Toggle if esc => format!("Press {hotkey} to paste · Esc cancels"),
                DictationMode::Toggle => format!("Press {hotkey} to paste"),
            };
            ("listening", "Listening".to_string(), Some(hint))
        }
        Pill::Working(text) if *text == PASTING => ("working", text.to_string(), None),
        Pill::Working(text) => ("working", text.to_string(), esc_hint),
        Pill::Done(text) => ("done", text.clone(), None),
        Pill::Failed(text) => ("failed", text.clone(), None),
    };
    Some(View {
        kind,
        text,
        hint,
        theme: theme.to_string(),
    })
}

/// How long a finished pill stays up: a failure long enough to read.
fn lasts(view: &View) -> Option<Duration> {
    match view.kind {
        "done" => Some(DONE_FOR),
        "failed" => Some(Duration::from_millis(
            (view.text.chars().count() as u64 * 70).max(5000),
        )),
        _ => None,
    }
}

/// Where the pill goes on a work area (x, y, width, height): top centre, or
/// bottom centre when the caret is in the top half.
fn spot(
    work: (i32, i32, i32, i32),
    size: (i32, i32),
    margin: i32,
    caret_y: Option<i32>,
) -> (i32, i32) {
    let (x, y, w, h) = work;
    let caret_high = caret_y.is_some_and(|caret| caret < y + h / 2);
    let top = if caret_high {
        y + h - size.1 - margin
    } else {
        y + margin
    };
    (x + (w - size.0) / 2, top)
}

/// What a show needs from the desktop.
pub trait Screen {
    fn foreground(&self) -> isize;
    fn show_without_activating(&self, window: isize);
    fn hide(&self, window: isize);
    fn set_foreground(&self, window: isize);
}

/// Show `pill` and make sure that didn't activate it. If it did, give the
/// foreground straight back and hide again; false then.
fn show_checked(screen: &impl Screen, pill: isize) -> bool {
    let before = screen.foreground();
    screen.show_without_activating(pill);
    if screen.foreground() != pill {
        return true;
    }
    screen.hide(pill);
    if before != 0 && before != pill {
        screen.set_foreground(before);
    }
    false
}

/// Show, update or hide the pill.
pub fn show(app: &tauri::AppHandle, pill: &Pill, mode: DictationMode, hotkey: &str, esc: bool) {
    let state = app.state::<PillState>();
    let generation = state.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let Some(view) = view(pill, mode, hotkey, esc, &state.theme) else {
        hide(app);
        return;
    };
    if state.off.load(Ordering::SeqCst) {
        return;
    }
    let _ = app.emit_to(LABEL, "pill", &view);
    if !state.shown.swap(true, Ordering::SeqCst) {
        place(app);
        let app = app.clone();
        let _ = app.clone().run_on_main_thread(move || {
            let Some(pill) = window_handle(&app) else {
                return;
            };
            if !show_checked(&os::Os, pill) {
                let state = app.state::<PillState>();
                state.off.store(true, Ordering::SeqCst);
                state.shown.store(false, Ordering::SeqCst);
                eprintln!("the dictation pill took focus when shown, so it stays hidden");
            }
        });
    }
    if let Some(after) = lasts(&view) {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(after).await;
            let state = app.state::<PillState>();
            if state.generation.load(Ordering::SeqCst) == generation {
                hide(&app);
            }
        });
    }
}

fn hide(app: &tauri::AppHandle) {
    if !app.state::<PillState>().shown.swap(false, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        if let Some(pill) = window_handle(&app) {
            os::Os.hide(pill);
        }
    });
}

#[cfg(windows)]
fn window_handle(app: &tauri::AppHandle) -> Option<isize> {
    let win = app.get_webview_window(LABEL)?;
    win.hwnd().ok().map(|h| h.0 as isize)
}

#[cfg(not(windows))]
fn window_handle(_: &tauri::AppHandle) -> Option<isize> {
    None
}

/// Move the pill onto the monitor of the window being dictated into.
fn place(app: &tauri::AppHandle) {
    let Some(win) = app.get_webview_window(LABEL) else {
        return;
    };
    let (centre, caret_y) = os::focus_hint();
    let monitor = centre
        .and_then(|(x, y)| app.monitor_from_point(x as f64, y as f64).ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else {
        return;
    };
    let scale = monitor.scale_factor();
    let work = monitor.work_area();
    let (x, y) = spot(
        (
            work.position.x,
            work.position.y,
            work.size.width as i32,
            work.size.height as i32,
        ),
        ((WIDTH * scale) as i32, (HEIGHT * scale) as i32),
        (MARGIN * scale) as i32,
        caret_y,
    );
    let _ = win.set_position(PhysicalPosition::new(x, y));
}

#[cfg(windows)]
mod os {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetGUIThreadInfo, GetWindowRect, SetForegroundWindow, SetWindowPos,
        ShowWindow, GUITHREADINFO, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE,
        SW_SHOWNOACTIVATE,
    };

    use super::Screen;

    pub struct Os;

    fn hwnd(window: isize) -> HWND {
        HWND(window as *mut core::ffi::c_void)
    }

    impl Screen for Os {
        fn foreground(&self) -> isize {
            // SAFETY: reads the foreground handle.
            unsafe { GetForegroundWindow() }.0 as isize
        }

        fn show_without_activating(&self, window: isize) {
            // SAFETY: our own window; neither call activates it.
            unsafe {
                let _ = ShowWindow(hwnd(window), SW_SHOWNOACTIVATE);
                let _ = SetWindowPos(
                    hwnd(window),
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }

        fn hide(&self, window: isize) {
            // SAFETY: our own window.
            unsafe {
                let _ = ShowWindow(hwnd(window), SW_HIDE);
            }
        }

        fn set_foreground(&self, window: isize) {
            // SAFETY: a plain handle; failure leaves focus where it is.
            unsafe {
                let _ = SetForegroundWindow(hwnd(window));
            }
        }
    }

    fn rect(window: HWND) -> Option<RECT> {
        let mut r = RECT::default();
        // SAFETY: writes into a local.
        unsafe { GetWindowRect(window, &mut r) }.ok()?;
        Some(r)
    }

    /// The centre of the foreground window, and roughly where its caret is
    /// on screen: the caret's window top plus its offset, which ignores the
    /// frame but is close enough to tell the top half from the bottom.
    pub fn focus_hint() -> (Option<(i32, i32)>, Option<i32>) {
        // SAFETY: reads handles and fills locals of the right size.
        unsafe {
            let front = GetForegroundWindow();
            if front.is_invalid() {
                return (None, None);
            }
            let centre = rect(front).map(|r| ((r.left + r.right) / 2, (r.top + r.bottom) / 2));
            let mut info = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            let caret = GetGUIThreadInfo(0, &mut info)
                .ok()
                .filter(|_| !info.hwndCaret.is_invalid())
                .and_then(|_| rect(info.hwndCaret))
                .map(|r| r.top + info.rcCaret.top);
            (centre, caret)
        }
    }
}

#[cfg(not(windows))]
mod os {
    use super::Screen;

    pub struct Os;

    impl Screen for Os {
        fn foreground(&self) -> isize {
            0
        }
        fn show_without_activating(&self, _: isize) {}
        fn hide(&self, _: isize) {}
        fn set_foreground(&self, _: isize) {}
    }

    pub fn focus_hint() -> (Option<(i32, i32)>, Option<i32>) {
        (None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictation::{STARTING, TRANSCRIBING};
    use std::cell::{Cell, RefCell};

    const PILL: isize = 9;
    const APP: isize = 4;

    struct FakeScreen {
        front: Cell<isize>,
        /// A broken show that activates the window anyway.
        show_activates: bool,
        calls: RefCell<Vec<String>>,
    }

    impl FakeScreen {
        fn new(show_activates: bool) -> Self {
            FakeScreen {
                front: Cell::new(APP),
                show_activates,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Screen for FakeScreen {
        fn foreground(&self) -> isize {
            self.front.get()
        }
        fn show_without_activating(&self, window: isize) {
            self.calls.borrow_mut().push("show".into());
            if self.show_activates {
                self.front.set(window);
            }
        }
        fn hide(&self, _: isize) {
            self.calls.borrow_mut().push("hide".into());
        }
        fn set_foreground(&self, window: isize) {
            self.calls.borrow_mut().push(format!("foreground {window}"));
            self.front.set(window);
        }
    }

    #[test]
    fn a_show_that_keeps_focus_where_it_was_is_left_alone() {
        let screen = FakeScreen::new(false);
        assert!(show_checked(&screen, PILL));
        assert_eq!(*screen.calls.borrow(), ["show"]);
        assert_eq!(screen.foreground(), APP);
    }

    #[test]
    fn a_show_that_took_focus_gives_it_back_and_hides() {
        let screen = FakeScreen::new(true);
        assert!(!show_checked(&screen, PILL));
        assert_eq!(*screen.calls.borrow(), ["show", "hide", "foreground 4"]);
        assert_eq!(screen.foreground(), APP);
    }

    #[test]
    fn the_pill_sits_top_centre_unless_the_caret_is_up_there() {
        let work = (0, 0, 1920, 1040);
        let size = (460, 88);
        assert_eq!(spot(work, size, 8, None), (730, 8));
        assert_eq!(spot(work, size, 8, Some(900)), (730, 8));
        assert_eq!(spot(work, size, 8, Some(120)), (730, 1040 - 88 - 8));
    }

    #[test]
    fn a_second_monitor_offsets_the_spot() {
        assert_eq!(
            spot((-1920, 100, 1920, 1000), (400, 80), 10, None),
            (-1160, 110)
        );
    }

    #[test]
    fn listening_tells_each_mode_how_to_finish() {
        let hold = view(
            &Pill::Listening,
            DictationMode::Hold,
            "Ctrl+Shift+D",
            true,
            "dark",
        )
        .unwrap();
        assert_eq!(hold.kind, "listening");
        assert_eq!(hold.hint.as_deref(), Some("Let go to paste"));
        let toggle = view(
            &Pill::Listening,
            DictationMode::Toggle,
            "Ctrl+Shift+D",
            true,
            "dark",
        )
        .unwrap();
        assert_eq!(
            toggle.hint.as_deref(),
            Some("Press Ctrl+Shift+D to paste · Esc cancels")
        );
        let no_esc = view(
            &Pill::Listening,
            DictationMode::Toggle,
            "Ctrl+Shift+D",
            false,
            "dark",
        )
        .unwrap();
        assert_eq!(no_esc.hint.as_deref(), Some("Press Ctrl+Shift+D to paste"));
    }

    #[test]
    fn esc_is_offered_only_while_it_can_still_cancel() {
        let hint = |pill: Pill, esc| {
            view(&pill, DictationMode::Hold, "D", esc, "light")
                .unwrap()
                .hint
        };
        assert_eq!(
            hint(Pill::Working(STARTING), true).as_deref(),
            Some("Esc cancels")
        );
        assert_eq!(
            hint(Pill::Working(TRANSCRIBING), true).as_deref(),
            Some("Esc cancels")
        );
        assert_eq!(hint(Pill::Working(TRANSCRIBING), false), None);
        assert_eq!(hint(Pill::Working(PASTING), true), None);
    }

    #[test]
    fn finished_pills_go_away_on_their_own_and_failures_last_to_be_read() {
        let done = view(
            &Pill::Done("Pasted.".into()),
            DictationMode::Hold,
            "D",
            false,
            "dark",
        );
        assert_eq!(lasts(&done.unwrap()), Some(DONE_FOR));
        let long = "x".repeat(200);
        let failed = view(&Pill::Failed(long), DictationMode::Hold, "D", false, "dark").unwrap();
        assert_eq!(lasts(&failed), Some(Duration::from_millis(14_000)));
        let listening = view(&Pill::Listening, DictationMode::Hold, "D", false, "dark").unwrap();
        assert_eq!(lasts(&listening), None);
        assert_eq!(
            view(&Pill::Hidden, DictationMode::Hold, "D", false, "dark"),
            None
        );
    }
}
