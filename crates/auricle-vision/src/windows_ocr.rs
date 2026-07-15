//! Windows implementation: Windows.Graphics.Capture (one frame, session
//! closed immediately) + Windows.Media.Ocr. No concealment APIs — the
//! only window filtering here decides what WE capture, never how our
//! windows appear to anyone else's capture.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::{factory, Interface, PWSTR};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind,
    GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::Imaging::SoftwareBitmap;
use windows::Media::Ocr::OcrEngine;
use windows::Win32::Foundation::{E_ACCESSDENIED, E_INVALIDARG, HWND};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Com::CoIncrementMTAUsage;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetTopWindow, GetWindow, GetWindowLongW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, GWL_EXSTYLE, GW_HWNDNEXT,
    WS_EX_TOOLWINDOW,
};

use crate::{flatten_reading_order, OcrLine, ScreenContext, ScreenReader, VisionError};

/// A window that renders content can deliver its first frame within
/// milliseconds; probing showed ~30 ms with a warm device. Anything that
/// takes this long is not going to produce one (see the minimized-window
/// finding in docs/PHASE7_VISION_REPORT.md).
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(2);

/// WinRT objects cached across captures: creating the D3D11 device per
/// call costs ~300 ms — most of the <500 ms budget (probe-measured).
struct Cached {
    device: IDirect3DDevice,
    engine: OcrEngine,
    /// Whether the OS granted borderless capture (suppresses the yellow
    /// capture-indicator border around the target for our single frame;
    /// auto-granted without a prompt for desktop processes on this OS).
    borderless: bool,
}

// SAFETY: both cached objects are free-threaded — the D3D11 device is
// created without D3D11_CREATE_DEVICE_SINGLETHREADED (thread-safe per
// D3D11 contract) and OcrEngine is an agile WinRT class — and the
// `Mutex<Option<Cached>>` in the reader serializes all use anyway. The
// windows crate stopped blanket-implementing Send for interface pointers;
// this restores it for exactly these two.
unsafe impl Send for Cached {}

/// `ScreenReader` backed by Windows.Graphics.Capture + Windows.Media.Ocr.
#[derive(Default)]
pub struct WindowsOcrReader {
    // One capture at a time; also guards lazy init of the cached state.
    cached: Mutex<Option<Cached>>,
}

impl WindowsOcrReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-create the D3D device + OCR engine (~300 ms) so the first
    /// capture runs at warm speed. `auricle peek` calls this during its
    /// countdown; captures initialize lazily without it.
    pub fn warm_up(&self) -> Result<(), VisionError> {
        init_winrt();
        let mut guard = self.cached.lock().unwrap();
        if guard.is_none() {
            *guard = Some(init_cached()?);
        }
        Ok(())
    }

    /// Capture and OCR one specific window. Public so error behavior
    /// (minimized / gone) is verifiable against real windows; the
    /// product path is `capture_active_window`.
    pub fn capture_window(&self, hwnd: isize) -> Result<ScreenContext, VisionError> {
        init_winrt();
        let hwnd = HWND(hwnd as *mut _);
        let mut guard = self.cached.lock().unwrap();
        self.capture_hwnd_locked(&mut guard, hwnd)
    }

    fn capture_hwnd_locked(
        &self,
        guard: &mut Option<Cached>,
        hwnd: HWND,
    ) -> Result<ScreenContext, VisionError> {
        // Pre-flight: these states never produce a frame (probe finding:
        // a minimized window "captures" successfully and then times out).
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Err(VisionError::WindowGone);
        }
        if unsafe { IsIconic(hwnd) }.as_bool() {
            return Err(VisionError::Minimized);
        }
        if is_cloaked(hwnd) {
            // Cloaked (suspended UWP) windows deliver blank frames that
            // would read as a convincingly empty screen.
            return Err(VisionError::CaptureFailed(
                "the window is cloaked (suspended) and renders no content".to_string(),
            ));
        }

        if guard.is_none() {
            *guard = Some(init_cached()?);
        }
        let cached = guard.as_ref().expect("initialized above");

        let result = capture_and_ocr(cached, hwnd);
        if matches!(result, Err(VisionError::CaptureFailed(_))) {
            // A lost D3D device (driver reset, sleep/resume) poisons the
            // cache; rebuild on the next call.
            *guard = None;
        }
        result
    }
}

impl ScreenReader for WindowsOcrReader {
    fn capture_active_window(
        &self,
        exclude_hwnd: Option<isize>,
    ) -> Result<ScreenContext, VisionError> {
        init_winrt();
        let hwnd = pick_active_window(exclude_hwnd)?;
        let mut guard = self.cached.lock().unwrap();
        self.capture_hwnd_locked(&mut guard, hwnd)
    }
}

/// The foreground window if it is a real, capturable app window;
/// otherwise the first eligible window in Z-order. `GetForegroundWindow`
/// is not always a user window (probe: an untitled explorer.exe shell
/// window right after a launch), and the excluded window is skipped the
/// same way — "active" means "what the user is looking at behind us".
fn pick_active_window(exclude_hwnd: Option<isize>) -> Result<HWND, VisionError> {
    let eligible = |hwnd: HWND| -> bool {
        if hwnd.is_invalid() || exclude_hwnd == Some(hwnd.0 as isize) {
            return false;
        }
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
                return false;
            }
            if (GetWindowLongW(hwnd, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW.0 != 0 {
                return false;
            }
            // Never capture our own windows (PHASE-9: the overlay also
            // passes its HWND explicitly; this is belt-and-braces).
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == GetCurrentProcessId() {
                return false;
            }
        }
        if window_title(hwnd).is_empty() || is_cloaked(hwnd) {
            return false;
        }
        // The desktop shell (wallpaper) windows are titled and visible
        // but are backdrop, not content.
        !matches!(class_name(hwnd).as_str(), "Progman" | "WorkerW")
    };

    let foreground = unsafe { GetForegroundWindow() };
    if eligible(foreground) {
        return Ok(foreground);
    }
    let mut cursor = unsafe { GetTopWindow(None) }.ok();
    while let Some(hwnd) = cursor {
        if eligible(hwnd) {
            return Ok(hwnd);
        }
        cursor = unsafe { GetWindow(hwnd, GW_HWNDNEXT) }.ok();
    }
    Err(VisionError::NoWindow)
}

/// Create the cached device + engine (first capture, or after loss).
fn init_cached() -> Result<Cached, VisionError> {
    if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
        return Err(VisionError::CaptureDenied(
            "Windows.Graphics.Capture is not supported on this system".to_string(),
        ));
    }

    let mut d3d: Option<ID3D11Device> = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut d3d),
            None,
            None,
        )
        .map_err(|e| VisionError::CaptureFailed(format!("D3D11CreateDevice: {e}")))?;
    }
    let d3d = d3d.ok_or_else(|| {
        VisionError::CaptureFailed("D3D11CreateDevice returned no device".to_string())
    })?;
    let dxgi: IDXGIDevice = d3d
        .cast()
        .map_err(|e| VisionError::CaptureFailed(format!("IDXGIDevice cast: {e}")))?;
    let device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
        .and_then(|inspectable| inspectable.cast())
        .map_err(|e| VisionError::CaptureFailed(format!("WinRT device interop: {e}")))?;

    let engine = create_ocr_engine()?;

    // Ask once whether the capture-indicator border may be suppressed for
    // our on-demand single frame (probe: auto-granted, no prompt). This
    // is the OS's indicator for OUR capture — not window concealment.
    let borderless =
        GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
            .and_then(|op| op.join())
            .map(
                |status| status.0 == 4, /* AppCapabilityAccessStatus::Allowed */
            )
            .unwrap_or(false);

    Ok(Cached {
        device,
        engine,
        borderless,
    })
}

fn create_ocr_engine() -> Result<OcrEngine, VisionError> {
    match OcrEngine::TryCreateFromUserProfileLanguages() {
        Ok(engine) => Ok(engine),
        // TryCreate returns null (an error in Rust) when no user-profile
        // language has OCR support; tell that apart from other failures.
        Err(e) => {
            let none_available = OcrEngine::AvailableRecognizerLanguages()
                .and_then(|langs| langs.Size())
                .map(|n| n == 0)
                .unwrap_or(true);
            if none_available {
                Err(VisionError::OcrLanguageMissing)
            } else {
                Err(VisionError::CaptureFailed(format!("OCR engine: {e}")))
            }
        }
    }
}

/// One frame in, reading-order text out. The capture session lives only
/// inside this function — on-demand rule.
fn capture_and_ocr(cached: &Cached, hwnd: HWND) -> Result<ScreenContext, VisionError> {
    let window_title = window_title(hwnd);
    let app_name = process_stem(hwnd);

    let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(|e| VisionError::CaptureFailed(format!("capture interop: {e}")))?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd) }.map_err(|e| {
        if e.code() == E_INVALIDARG {
            VisionError::WindowGone
        } else if e.code() == E_ACCESSDENIED {
            VisionError::CaptureDenied(e.message().to_string())
        } else {
            VisionError::CaptureFailed(format!("CreateForWindow: {e}"))
        }
    })?;
    let size = item
        .Size()
        .map_err(|e| VisionError::CaptureFailed(format!("item size: {e}")))?;

    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &cached.device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )
    .map_err(|e| VisionError::CaptureFailed(format!("frame pool: {e}")))?;
    let session = pool
        .CreateCaptureSession(&item)
        .map_err(|e| VisionError::CaptureFailed(format!("capture session: {e}")))?;
    let _ = session.SetIsCursorCaptureEnabled(false);
    if cached.borderless {
        let _ = session.SetIsBorderRequired(false);
    }
    session.StartCapture().map_err(|e| {
        if e.code() == E_ACCESSDENIED {
            VisionError::CaptureDenied(e.message().to_string())
        } else {
            VisionError::CaptureFailed(format!("StartCapture: {e}"))
        }
    })?;

    // Free-threaded pool: poll for the first frame (TryGetNextFrame
    // "fails" on null until one arrives).
    let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
    let frame = loop {
        match pool.TryGetNextFrame() {
            Ok(frame) => break frame,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(4));
            }
            Err(_) => {
                let _ = session.Close();
                let _ = pool.Close();
                return Err(VisionError::CaptureFailed(format!(
                    "no frame arrived within {FIRST_FRAME_TIMEOUT:?} \
                     (window not rendering?)"
                )));
            }
        }
    };
    let captured_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let bitmap = frame
        .Surface()
        .and_then(|surface| SoftwareBitmap::CreateCopyFromSurfaceAsync(&surface))
        .and_then(|op| op.join())
        .map_err(|e| VisionError::CaptureFailed(format!("frame copy: {e}")));

    // Single frame taken: close everything before OCR (on-demand rule).
    let _ = frame.Close();
    let _ = session.Close();
    let _ = pool.Close();
    let bitmap = bitmap?;

    let (text, ocr_ms) = ocr_bitmap(&cached.engine, &bitmap)?;
    Ok(ScreenContext {
        window_title,
        app_name,
        text,
        captured_at,
        ocr_ms,
    })
}

fn ocr_bitmap(engine: &OcrEngine, bitmap: &SoftwareBitmap) -> Result<(String, u64), VisionError> {
    let max_dim = OcrEngine::MaxImageDimension().unwrap_or(10_000) as i32;
    let (w, h) = (
        bitmap.PixelWidth().unwrap_or(0),
        bitmap.PixelHeight().unwrap_or(0),
    );
    if w > max_dim || h > max_dim {
        return Err(VisionError::CaptureFailed(format!(
            "captured frame {w}x{h} exceeds the OCR engine limit of {max_dim} px"
        )));
    }

    let started = Instant::now();
    let result = engine
        .RecognizeAsync(bitmap)
        .and_then(|op| op.join())
        .map_err(|e| VisionError::CaptureFailed(format!("OCR: {e}")))?;
    let mut lines: Vec<OcrLine> = Vec::new();
    let ocr_lines = result
        .Lines()
        .map_err(|e| VisionError::CaptureFailed(format!("OCR lines: {e}")))?;
    for line in ocr_lines {
        let Ok(text) = line.Text() else { continue };
        // A line's box is the union of its word boxes (OcrLine itself
        // exposes no rect).
        let (mut x0, mut y0) = (f32::MAX, f32::MAX);
        let (mut x1, mut y1) = (f32::MIN, f32::MIN);
        let Ok(words) = line.Words() else { continue };
        for word in words {
            if let Ok(r) = word.BoundingRect() {
                x0 = x0.min(r.X);
                y0 = y0.min(r.Y);
                x1 = x1.max(r.X + r.Width);
                y1 = y1.max(r.Y + r.Height);
            }
        }
        if x0 > x1 {
            continue; // no word boxes
        }
        lines.push(OcrLine {
            text: text.to_string_lossy(),
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        });
    }
    let ocr_ms = started.elapsed().as_millis() as u64;
    Ok((flatten_reading_order(&lines), ocr_ms))
}

/// WinRT init. The MTA is pinned process-wide exactly once: the cached
/// device/engine must outlive the thread that created them, and without
/// the pin the apartment dies with its last initialized thread — later
/// use of the cached objects is then an access violation. Observed for
/// real twice-removed from obvious: peek's warm-up thread exits before
/// the capture, and tokio reaps idle spawn_blocking threads between
/// daemon peeks. The per-thread `RoInitialize` on top is belt-and-braces
/// (`RPC_E_CHANGED_MODE` from an STA thread is fine — these classes are
/// agile).
fn init_winrt() {
    static MTA_PIN: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    MTA_PIN.get_or_init(|| unsafe {
        // The cookie is intentionally leaked: the pin lasts for the
        // process lifetime.
        let _ = CoIncrementMTAUsage();
    });
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
}

fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        )
        .map(|_| cloaked != 0)
        .unwrap_or(false)
    }
}

/// Executable stem of the window's owning process ("msedge", "Cursor").
fn process_stem(hwnd: HWND) -> String {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return String::new();
    }
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return String::new();
    };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let name = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .map(|_| String::from_utf16_lossy(&buf[..len as usize]))
    .unwrap_or_default();
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
    std::path::Path::new(&name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}
