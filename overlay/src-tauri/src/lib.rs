//! Auricle Copilot overlay shell (COPILOT_ARCHITECTURE §2.3).
//!
//! A NORMAL, always-visible window: no display-affinity tricks, no
//! capture concealment of any kind, visible in alt-tab and screen shares
//! by design (Global Rule 3). Global hotkeys come exclusively from the
//! global-shortcut plugin (RegisterHotKey underneath — no low-level
//! keyboard hooks).
//!
//! The overlay talks ONLY to the engine's public HTTP API. All requests
//! go through this Rust side (reqwest): the engine's same-origin defense
//! correctly rejects browser-origin fetches from the webview
//! (tauri.localhost), while a local process speaks plain HTTP without an
//! Origin header — exactly the trust model the engine documents.

mod ask;
mod config;
mod dictate;
mod dictation;
mod engine;
mod sse;
mod ws;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;
use tauri::ipc::Channel;
use tauri::menu::MenuBuilder;
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, PhysicalPosition, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_updater::UpdaterExt;

use config::{Corner, OverlayConfig};

/// How often the engine watcher re-reads /health.
const WATCH_INTERVAL: Duration = Duration::from_secs(3);
/// How long the engine gets to exit cleanly when asked to.
const ENGINE_STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a Windows session end may wait on the engine. Windows allows an
/// app about 5 s here before it starts offering to kill it.
const SESSION_END_BUDGET: Duration = Duration::from_secs(4);
/// How long a confirmed Quit waits for the engine to accept the stop.
const QUIT_STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the notice before the installer stays up. The overlay is gone
/// after the handoff, so this is the last thing it can tell anyone.
const INSTALL_NOTICE: Duration = Duration::from_secs(10);

struct AppState {
    cfg: OverlayConfig,
    http: reqwest::Client,
    hotkey_warnings: Mutex<Vec<String>>,
    asks: ask::AskSlot,
    /// Messages raised before the webview was listening: an event then
    /// would be lost, so get_init hands these over instead. None from that
    /// point on.
    early_notices: Mutex<Option<Vec<String>>>,
    /// Where the overlay last docked itself. A window found anywhere else
    /// was moved by the user.
    docked_at: Mutex<Option<PhysicalPosition<i32>>>,
    /// Set while the engine is being stopped on purpose, so the watcher
    /// does not recover it.
    engine_hold: AtomicBool,
    /// Wakes the watcher to bring an engine up now.
    engine_kick: tokio::sync::Notify,
    quit_guard: Mutex<engine::QuitGuard>,
}

impl AppState {
    fn base_url(&self) -> &str {
        &self.cfg.overlay.engine_url
    }
}

/// The engine's data dir, for its log and pid record.
fn engine_dir() -> PathBuf {
    config::data_dir().unwrap_or_else(std::env::temp_dir)
}

/// The top-left that keeps a window of `size` fully inside the work area,
/// moving it only as far as needed. A window bigger than the area keeps
/// its top-left corner on screen.
fn keep_on_screen(
    pos: (i32, i32),
    size: (i32, i32),
    origin: (i32, i32),
    area: (i32, i32),
) -> (i32, i32) {
    let fit = |p: i32, len: i32, start: i32, span: i32| p.min(start + span - len).max(start);
    (
        fit(pos.0, size.0, origin.0, area.0),
        fit(pos.1, size.1, origin.1, area.1),
    )
}

/// Dock the window to the configured corner of the current monitor's
/// work area (12 logical px margin). Once the user has moved it, it stays
/// where they put it for the session, and only growth that would run off
/// the screen moves it.
///
/// "Moved" is read off the window rather than from Moved events: those
/// also fire for our own docking and for minimizing. `docked_at` is never
/// locked across a window call: those can wait on the main thread, which
/// may itself be here waiting for the lock.
fn position_window(win: &WebviewWindow, state: &AppState) {
    if win.is_minimized().unwrap_or(false) {
        return;
    }
    let Ok(Some(monitor)) = win.current_monitor() else {
        return;
    };
    let Ok(size) = win.outer_size() else { return };
    let Ok(here) = win.outer_position() else {
        return;
    };
    let work = monitor.work_area();
    let docked_at = *state.docked_at.lock().unwrap();
    if docked_at.is_some_and(|docked| docked != here) {
        let (x, y) = keep_on_screen(
            (here.x, here.y),
            (size.width as i32, size.height as i32),
            (work.position.x, work.position.y),
            (work.size.width as i32, work.size.height as i32),
        );
        if (x, y) != (here.x, here.y) {
            let _ = win.set_position(PhysicalPosition::new(x, y));
        }
        return;
    }
    let corner = state.cfg.overlay.corner;
    let margin = (12.0 * monitor.scale_factor()) as i32;
    let x = match corner {
        Corner::TopLeft | Corner::BottomLeft => work.position.x + margin,
        Corner::TopRight | Corner::BottomRight => {
            work.position.x + work.size.width as i32 - size.width as i32 - margin
        }
    };
    let y = match corner {
        Corner::TopLeft | Corner::TopRight => work.position.y + margin,
        Corner::BottomLeft | Corner::BottomRight => {
            work.position.y + work.size.height as i32 - size.height as i32 - margin
        }
    };
    let _ = win.set_position(PhysicalPosition::new(x, y));
    // Read back, so the comparison above can never trip on rounding.
    *state.docked_at.lock().unwrap() = win.outer_position().ok();
}

/// Show + focus the overlay and tell the webview why. The hotkey press
/// itself grants foreground rights (Phase 7 finding 14: focus cannot be
/// stolen, only earned — RegisterHotKey earns it).
fn summon(app: &tauri::AppHandle, quick: bool) {
    let state = app.state::<AppState>();
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    // The overlay is a normal window with a taskbar button, so the user
    // can minimize it — the hotkey must recover from that state too.
    let _ = win.unminimize();
    position_window(&win, &state);
    let _ = win.set_ignore_cursor_events(false);
    let _ = win.show();
    let _ = win.set_focus();
    let _ = app.emit(if quick { "quick-assist" } else { "summon" }, ());
}

/// Tell the user something. The toast lives in the overlay, which is hidden
/// by default and which only this side can show.
fn notify(app: &tauri::AppHandle, message: impl Into<String>) {
    summon(app, false);
    let message = message.into();
    if let Some(early) = app
        .state::<AppState>()
        .early_notices
        .lock()
        .unwrap()
        .as_mut()
    {
        early.push(message);
        return;
    }
    let _ = app.emit("tray-error", message);
}

#[derive(Debug, Deserialize)]
struct AskRequest {
    question: String,
    include_screen: bool,
    include_transcript: bool,
    follow_up: bool,
}

/// Run one /ask against the engine, forwarding each SSE event to the
/// webview through `on_event`. Starting it cancels any ask still running.
/// The overlay's own HWND rides along as exclude_hwnd so the capture never
/// OCRs the overlay itself.
#[tauri::command]
async fn ask_stream(
    app: tauri::AppHandle,
    window: tauri::Window,
    req: AskRequest,
    on_event: Channel<serde_json::Value>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    #[cfg(windows)]
    let exclude_hwnd = window.hwnd().ok().map(|h| h.0 as isize);
    #[cfg(not(windows))]
    let exclude_hwnd: Option<isize> = None;

    let body = json!({
        "question": req.question,
        "include_screen": req.include_screen,
        "include_transcript": req.include_transcript,
        "follow_up": req.follow_up,
        "exclude_hwnd": exclude_hwnd,
    });
    let registration = state.asks.begin();
    ask::stream(&state.http, state.base_url(), &body, registration, |v| {
        let _ = on_event.send(v);
    })
    .await
}

#[tauri::command]
async fn get_health(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let resp = state
        .http
        .get(format!("{}/api/v1/health", state.base_url()))
        .timeout(std::time::Duration::from_millis(1500))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("engine health returned HTTP {}", resp.status()));
    }
    // The attach guard only runs once; a process can squat the port after
    // the engine exits. Re-check the health shape on every poll so the
    // overlay never reports "connected" (or routes asks) to a squatter.
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    if !engine::is_engine_health(&body) {
        return Err("response on the engine port is not Auricle's health shape".to_string());
    }
    Ok(body)
}

#[tauri::command]
fn get_init(app: tauri::AppHandle) -> serde_json::Value {
    let state = app.state::<AppState>();
    let notices = state.early_notices.lock().unwrap().take();
    json!({
        "hotkey_warnings": *state.hotkey_warnings.lock().unwrap(),
        "notices": notices.unwrap_or_default(),
        "window_min": state.cfg.overlay.transcript_window_min,
        "theme": state.cfg.overlay.theme,
        "summon_hotkey": state.cfg.hotkeys.summon,
        "quick_hotkey": state.cfg.hotkeys.quick,
    })
}

/// Dismiss: hide the window and abandon any ask still streaming into it.
#[tauri::command]
fn hide_overlay(app: tauri::AppHandle, window: tauri::Window) {
    app.state::<AppState>().asks.cancel();
    let _ = window.hide();
}

/// The webview reports its rendered card height; the window shrinks or
/// grows to match so no invisible (but click-blocking) region exists.
#[tauri::command]
fn resize_overlay(app: tauri::AppHandle, window: tauri::Window, height: f64) {
    let clamped = height.clamp(96.0, 720.0);
    let _ = window.set_size(tauri::LogicalSize::new(420.0, clamped));
    let state = app.state::<AppState>();
    if let Some(win) = app.get_webview_window("main") {
        position_window(&win, &state);
    }
}

async fn run_tray_action(http: &reqwest::Client, base: &str, action: &str) -> Result<(), String> {
    match action {
        "start" => {
            let resp = http
                .post(format!("{base}/api/v1/sessions"))
                .json(&json!({}))
                .send()
                .await
                .map_err(|e| format!("engine unreachable: {e}"))?;
            let status = resp.status();
            if status.is_success() {
                Ok(())
            } else {
                let v: serde_json::Value = resp.json().await.unwrap_or_default();
                Err(v["error"].as_str().unwrap_or("start failed").to_string())
            }
        }
        "stop" => {
            let health: serde_json::Value = http
                .get(format!("{base}/api/v1/health"))
                .send()
                .await
                .map_err(|e| format!("engine unreachable: {e}"))?
                .json()
                .await
                .unwrap_or_default();
            let Some(id) = health["active_session"].as_str() else {
                return Err("no active recording session".to_string());
            };
            let resp = http
                .post(format!("{base}/api/v1/sessions/{id}/stop"))
                .send()
                .await
                .map_err(|e| format!("engine unreachable: {e}"))?;
            if resp.status().is_success() {
                Ok(())
            } else {
                let v: serde_json::Value = resp.json().await.unwrap_or_default();
                Err(v["error"].as_str().unwrap_or("stop failed").to_string())
            }
        }
        _ => Ok(()),
    }
}

/// Stop whatever session is recording, if one is.
async fn stop_recording(http: &reqwest::Client, base: &str) -> Result<(), String> {
    match engine::probe(http, base).await {
        engine::Probe::Engine(health) if health["state"] == "recording" => {
            run_tray_action(http, base, "stop").await
        }
        _ => Ok(()),
    }
}

/// Ask the engine to exit if it is confirmed ours, and hold the session end
/// until it has, or until `budget` runs out: nothing past this point is ours
/// to wait for, and an engine still stopping is left for crash recovery.
/// `owner` runs off the runtime so that it counts against the budget, since
/// against an older engine it runs `tasklist`, which can't be cut short. For
/// the same reason this watches only the port, not `stop_engine`'s process.
async fn stop_engine_for_session_end(
    http: &reqwest::Client,
    base: &str,
    budget: Duration,
    owner: impl FnOnce(&engine::Probe) -> engine::Owned + Send + 'static,
) {
    let stop = async {
        let probe = engine::probe(http, base).await;
        let owned = tauri::async_runtime::spawn_blocking(move || owner(&probe)).await;
        if !matches!(owned, Ok(engine::Owned::Alive)) {
            return;
        }
        let asked = http.post(format!("{base}/api/v1/shutdown")).send().await;
        if asked.is_ok_and(|resp| resp.status().is_success()) {
            while engine::probe(http, base).await != engine::Probe::Down {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    let _ = tokio::time::timeout(budget, stop).await;
}

/// Windows is logging off or shutting down. An engine this overlay spawned is
/// a plain background process that gets killed without hearing about it, so
/// its recording would end as a crash. One that isn't ours is left alone: a
/// service hears these events itself, and may be meant to outlive the logoff.
fn on_session_end(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    state.engine_hold.store(true, Ordering::SeqCst);
    let dir = engine_dir();
    tauri::async_runtime::block_on(stop_engine_for_session_end(
        &state.http,
        state.base_url(),
        SESSION_END_BUDGET,
        move |probe| engine::owner(&dir, probe),
    ));
}

/// One press of Quit, judged on what the engine says right now.
async fn quit_step(
    http: &reqwest::Client,
    base: &str,
    dir: &std::path::Path,
    guard: &Mutex<engine::QuitGuard>,
) -> engine::QuitStep {
    let probe = engine::probe(http, base).await;
    let recording =
        matches!(&probe, engine::Probe::Engine(health) if health["state"] == "recording");
    let owned = engine::owner(dir, &probe);
    guard
        .lock()
        .unwrap()
        .press(owned, recording, Instant::now())
}

/// Quit leaves the engine running by design, so a recording of ours has to
/// end first: nothing else would show it or be able to stop it. The stop only
/// has to be accepted; the engine finishes saving the session on its own.
async fn quit(app: tauri::AppHandle) {
    let state = app.state::<AppState>();
    let (http, base) = (&state.http, state.base_url());
    match quit_step(http, base, &engine_dir(), &state.quit_guard).await {
        engine::QuitStep::Exit => app.exit(0),
        engine::QuitStep::Warn => notify(
            &app,
            format!(
                "A recording is running. Choose Quit overlay again within {} seconds to stop it \
                 and exit. Do nothing to keep recording.",
                engine::QUIT_CONFIRM_WINDOW.as_secs()
            ),
        ),
        engine::QuitStep::StopThenExit => {
            let stopped = tokio::time::timeout(QUIT_STOP_TIMEOUT, stop_recording(http, base))
                .await
                .unwrap_or_else(|_| Err("The engine did not answer.".to_string()));
            match stopped {
                Ok(()) => app.exit(0),
                Err(e) => notify(
                    &app,
                    format!("Still recording, so Auricle stayed open. {e}"),
                ),
            }
        }
    }
}

/// Download `update`, get the engine out of its way, and install. The
/// engine must exit first: a running auricle.exe stays locked against the
/// installer, and the relaunched overlay would attach to the old version.
/// Download comes before the stop so a failed one costs the user nothing.
async fn install_update(
    app: &tauri::AppHandle,
    update: &tauri_plugin_updater::Update,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let (http, base, dir) = (&state.http, state.base_url(), engine_dir());
    let probe = engine::probe(http, base).await;
    if let engine::UpdatePlan::Refuse(why) =
        engine::plan_update(&probe, engine::owner(&dir, &probe))
    {
        return Err(why.to_string());
    }
    notify(app, format!("Downloading update {}…", update.version));
    let bytes = update
        .download(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;

    state.engine_hold.store(true, Ordering::SeqCst);
    let stopped = match engine::stop_for_update(http, base, &dir, ENGINE_STOP_TIMEOUT).await {
        Ok(stopped) => stopped,
        Err(e) => {
            state.engine_hold.store(false, Ordering::SeqCst);
            return Err(e);
        }
    };
    // install() hands off to msiexec and ends this process, so a failure
    // past that point has nobody left to report it.
    notify(
        app,
        format!(
            "Installing update {}. Auricle will close and should reopen on its own. If it hasn't \
             after a few minutes, the install failed: start Auricle from the Start menu.",
            update.version
        ),
    );
    tokio::time::sleep(INSTALL_NOTICE).await;
    let installed = update.install(bytes).map_err(|e| e.to_string());
    if installed.is_err() {
        // Staying on this version, so an engine we stopped has to come back.
        state.engine_hold.store(false, Ordering::SeqCst);
        if stopped {
            state.engine_kick.notify_one();
        }
    }
    installed
}

/// Check GitHub for a newer signed release. On the tray action
/// (`interactive`) this downloads, installs, and relaunches; on the silent
/// startup check it only nudges via a toast so nothing installs behind the
/// user's back. Update integrity is verified against the bundled public key
/// (tauri.conf.json → plugins.updater.pubkey); a tampered artifact is
/// rejected before it can run.
async fn check_for_updates(app: tauri::AppHandle, interactive: bool) {
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => {
            if interactive {
                notify(&app, format!("updater unavailable: {e}"));
            }
            return;
        }
    };
    match updater.check().await {
        Ok(Some(update)) => {
            let version = update.version.clone();
            if !interactive {
                notify(
                    &app,
                    format!("Update {version} is available — Tray → Check for updates to install."),
                );
                return;
            }
            match install_update(&app, &update).await {
                Ok(()) => app.restart(),
                Err(e) => notify(&app, format!("Update {version} was not installed: {e}")),
            }
        }
        Ok(None) => {
            if interactive {
                notify(&app, "You're on the latest version.");
            }
        }
        Err(e) => {
            if interactive {
                notify(&app, format!("update check failed: {e}"));
            }
        }
    }
}

/// Runs once per healthy engine after an outage (and at launch): settle who
/// owns it and whether its version matches. True if it was stopped so that
/// the current binary can replace it. `warned` keeps a mismatch to one
/// message, since a health timeout under load also counts as an outage and
/// the warning takes focus.
async fn on_attach(
    app: &tauri::AppHandle,
    dir: &std::path::Path,
    health: &serde_json::Value,
    owned: engine::Owned,
    spawned_here: bool,
    warned: &mut Option<String>,
) -> bool {
    if owned == engine::Owned::Dead {
        // Our engine is gone and something else answers: not ours to manage.
        engine::clear_pid(dir);
    }
    let version = health["version"].as_str().unwrap_or_default();
    let overlay = app.package_info().version.to_string();
    let restartable = owned == engine::Owned::Alive && !spawned_here;
    match engine::check_version(version, &overlay, restartable, engine::is_busy(health)) {
        engine::VersionCheck::Match => false,
        engine::VersionCheck::Warn(message) => {
            if warned.as_deref() != Some(version) {
                *warned = Some(version.to_string());
                notify(app, message);
            }
            false
        }
        engine::VersionCheck::Restart => {
            let state = app.state::<AppState>();
            let (http, base) = (&state.http, state.base_url());
            match engine::stop_engine(http, base, dir, ENGINE_STOP_TIMEOUT).await {
                Ok(()) => true,
                Err(e) => {
                    notify(
                        app,
                        format!(
                            "The engine is version {version} but this overlay is {overlay}, \
                             and restarting it failed: {e}"
                        ),
                    );
                    false
                }
            }
        }
    }
}

/// The engine's supervisor, for the life of the overlay. It is the only
/// code that spawns, so launch, crash recovery, and relaunch can never race
/// each other for the port; the same health read keeps the tray honest.
async fn watch_engine(app: tauri::AppHandle) {
    let state = app.state::<AppState>();
    let (http, base, dir) = (&state.http, state.base_url(), engine_dir());
    let exe = engine::resolve_engine_exe(
        std::env::var_os("AURICLE_EXE").map(PathBuf::from),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .as_deref(),
    );
    // A record that is already dead here is from before a reboot or a
    // crash nobody saw: nothing to report, nothing to own.
    if engine::owned(&dir) == engine::Owned::Dead {
        engine::clear_pid(&dir);
    }

    let mut watch = engine::Watch::default();
    let mut attached = false;
    let mut spawned_here = false;
    let mut warned = None;
    let mut last_error = String::new();
    let mut tooltip = "";

    loop {
        let probe = engine::probe(http, base).await;
        let wanted_tooltip = engine::tray_tooltip(&probe);
        if wanted_tooltip != tooltip {
            if let Some(tray) = app.tray_by_id("main") {
                let _ = tray.set_tooltip(Some(wanted_tooltip));
            }
            tooltip = wanted_tooltip;
        }

        if !state.engine_hold.load(Ordering::SeqCst) {
            if let engine::Probe::Engine(health) = &probe {
                watch.healthy(Instant::now());
                if !attached {
                    attached = true;
                    let owned = engine::owner(&dir, &probe);
                    if on_attach(&app, &dir, health, owned, spawned_here, &mut warned).await {
                        watch.kick();
                        attached = false;
                        continue;
                    }
                }
            } else {
                attached = false;
                let owned = engine::owned(&dir);
                let squatted = probe == engine::Probe::Squatter;
                match watch.unhealthy(squatted, owned, Instant::now()) {
                    engine::Step::Wait => {}
                    engine::Step::Squatted => notify(&app, engine::squatter_message(base)),
                    engine::Step::Hung => notify(&app, engine::HUNG_MESSAGE),
                    engine::Step::GaveUp => {
                        if owned == engine::Owned::Dead {
                            last_error = engine::log_tail(&dir);
                            engine::clear_pid(&dir);
                        }
                        notify(
                            &app,
                            format!(
                                "The engine could not be kept running after several tries. \
                                 {last_error} Launch Auricle again to retry."
                            ),
                        );
                    }
                    engine::Step::Spawn => {
                        if owned == engine::Owned::Dead {
                            // Before the respawn rotates the log away.
                            last_error = engine::log_tail(&dir);
                            engine::clear_pid(&dir);
                            if watch.notice_due(Instant::now()) {
                                notify(
                                    &app,
                                    format!(
                                        "The engine stopped unexpectedly. Restarting it… \
                                         {last_error}"
                                    ),
                                );
                            }
                        }
                        spawned_here = true;
                        match engine::attach_or_spawn(http, base, &exe, &dir).await {
                            Ok(engine::Attach::Attached) => {}
                            Ok(engine::Attach::Spawned(pid)) => {
                                eprintln!("overlay spawned auricle serve (pid {pid})");
                            }
                            Err(e) => {
                                if watch.first_attempt() && watch.notice_due(Instant::now()) {
                                    notify(&app, format!("engine startup: {e}"));
                                }
                                last_error = e;
                            }
                        }
                    }
                }
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(WATCH_INTERVAL) => {}
            _ = state.engine_kick.notified() => {
                watch.kick();
                // Re-judge a running engine too: launching again is how a
                // user follows up on a version warning.
                attached = false;
            }
        }
    }
}

/// Open (or refocus) the dashboard window: the engine's web dashboard in
/// its own webview instead of a browser tab, so the whole product is one
/// app. The page keeps its engine origin (http://127.0.0.1:4820), so the
/// daemon's same-origin defense applies to it exactly as to a browser
/// tab; the webview gets no Tauri IPC (capabilities target "main" only).
fn open_dashboard(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        if let Some(win) = app.get_webview_window("dashboard") {
            let _ = win.unminimize();
            let _ = win.show();
            let _ = win.set_focus();
            return;
        }
        let (http, base) = {
            let state = app.state::<AppState>();
            (state.http.clone(), state.base_url().to_string())
        };
        // The engine may still be booting (attach_or_spawn allows 20 s
        // after a spawn) — don't point a webview at a dead port.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        while !engine::health_ok(&http, &base).await {
            if tokio::time::Instant::now() >= deadline {
                summon(&app, false);
                let _ = app.emit(
                    "tray-error",
                    "dashboard: the engine is not answering".to_string(),
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        let url: tauri::Url = match base.parse() {
            Ok(u) => u,
            Err(e) => {
                summon(&app, false);
                let _ = app.emit(
                    "tray-error",
                    format!("dashboard: bad engine URL {base}: {e}"),
                );
                return;
            }
        };
        let built = WebviewWindowBuilder::new(&app, "dashboard", WebviewUrl::External(url))
            .title("Auricle Dashboard")
            .inner_size(1200.0, 800.0)
            .build();
        if let Err(e) = built {
            summon(&app, false);
            let _ = app.emit("tray-error", format!("dashboard window: {e}"));
        }
    });
}

/// True when the OS foreground window is this window (or a child of it,
/// like the WebView2 controller). Distinguishes "the user switched to
/// another app" from "focus moved inside our own window" on a host
/// Focused(false) event.
#[cfg(windows)]
fn foreground_is_window(window: &tauri::Window) -> bool {
    #[link(name = "user32")]
    extern "system" {
        fn GetForegroundWindow() -> isize;
        fn GetAncestor(hwnd: isize, ga_flags: u32) -> isize;
    }
    const GA_ROOT: u32 = 2;
    let Ok(hwnd) = window.hwnd() else {
        return false;
    };
    // SAFETY: both calls take and return plain window handles.
    unsafe {
        let fg = GetForegroundWindow();
        fg != 0 && GetAncestor(fg, GA_ROOT) == hwnd.0 as isize
    }
}

#[cfg(not(windows))]
fn foreground_is_window(_window: &tauri::Window) -> bool {
    false
}

/// Tray actions hit the same public REST API as any other client.
/// Failures surface in the overlay itself (summon + toast event).
fn tray_action(app: tauri::AppHandle, action: &'static str) {
    tauri::async_runtime::spawn(async move {
        let (http, base) = {
            let state = app.state::<AppState>();
            (state.http.clone(), state.base_url().to_string())
        };
        if let Err(message) = run_tray_action(&http, &base, action).await {
            summon(&app, false);
            let _ = app.emit("tray-error", message);
        }
    });
}

pub fn run() {
    let cfg = OverlayConfig::load();
    let mut warnings: Vec<String> = Vec::new();

    // Parse hotkeys up front so registration failures are visible, not
    // silent no-ops (Global Rule: conflicts surface a warning).
    let summon_sc: Option<Shortcut> = match cfg.hotkeys.summon.parse() {
        Ok(sc) => Some(sc),
        Err(e) => {
            warnings.push(format!(
                "summon hotkey \"{}\" is invalid: {e}",
                cfg.hotkeys.summon
            ));
            None
        }
    };
    let quick_sc: Option<Shortcut> = match cfg.hotkeys.quick.parse() {
        Ok(sc) => Some(sc),
        Err(e) => {
            warnings.push(format!(
                "quick hotkey \"{}\" is invalid: {e}",
                cfg.hotkeys.quick
            ));
            None
        }
    };

    let dictate_sc: Option<Shortcut> = match cfg.hotkeys.dictate.parse() {
        Ok(sc) => Some(sc),
        Err(e) => {
            warnings.push(format!(
                "dictate hotkey \"{}\" is invalid: {e}",
                cfg.hotkeys.dictate
            ));
            None
        }
    };
    // Registered only while a dictation runs (dictation::run).
    let esc_sc = Shortcut::new(None, Code::Escape);

    let handler_summon = summon_sc;
    let handler_quick = quick_sc;
    let handler_dictate = dictate_sc;
    let (dictation_tx, dictation_rx) = tokio::sync::mpsc::unbounded_channel();
    let handler_dictation = dictation_tx.clone();

    let app = tauri::Builder::default()
        // Single instance, registered first: without it, users who launch
        // the app again stack up instances that fight over the hotkeys.
        // A relaunch focuses the dashboard, same as the first launch, and
        // is the user asking for a working app: the watcher brings an
        // engine up even if it had given up or the last one wasn't ours.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            app.state::<AppState>().engine_kick.notify_one();
            open_dashboard(app.clone());
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                // This runs with the plugin's shortcut table locked, so it
                // must never register or unregister one itself.
                .with_handler(move |app, shortcut, event| {
                    if handler_dictate.as_ref() == Some(shortcut) {
                        let input = match event.state() {
                            // First, before anything can move focus.
                            ShortcutState::Pressed => {
                                dictation::Input::Press(dictate::foreground_target())
                            }
                            ShortcutState::Released => dictation::Input::Release,
                        };
                        let _ = handler_dictation.send(input);
                        return;
                    }
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    if *shortcut == esc_sc {
                        let _ = handler_dictation.send(dictation::Input::Escape);
                        return;
                    }
                    if handler_summon.as_ref() == Some(shortcut) {
                        summon(app, false);
                    } else if handler_quick.as_ref() == Some(shortcut) {
                        summon(app, true);
                    }
                })
                .build(),
        )
        .manage(AppState {
            cfg: cfg.clone(),
            http: reqwest::Client::new(),
            hotkey_warnings: Mutex::new(warnings),
            asks: ask::AskSlot::default(),
            early_notices: Mutex::new(Some(Vec::new())),
            docked_at: Mutex::new(None),
            engine_hold: AtomicBool::new(false),
            engine_kick: tokio::sync::Notify::new(),
            quit_guard: Mutex::default(),
        })
        .setup(move |app| {
            // Hotkey registration: a conflict (another app owns the
            // combination) must be visible in the overlay, not silent.
            for (label, sc, text) in [
                ("summon", summon_sc, cfg.hotkeys.summon.clone()),
                ("quick-assist", quick_sc, cfg.hotkeys.quick.clone()),
                ("dictate", dictate_sc, cfg.hotkeys.dictate.clone()),
            ] {
                if let Some(sc) = sc {
                    if let Err(e) = app.global_shortcut().register(sc) {
                        app.state::<AppState>()
                            .hotkey_warnings
                            .lock()
                            .unwrap()
                            .push(format!(
                                "{label} hotkey {text} could not be registered: {e}"
                            ));
                    }
                }
            }
            // The overlay starts hidden, and the warnings render inside it.
            let hotkeys_ok = app
                .state::<AppState>()
                .hotkey_warnings
                .lock()
                .unwrap()
                .is_empty();
            if !hotkeys_ok {
                summon(app.handle(), false);
            }

            // Tray: the persistent entry point.
            let menu = MenuBuilder::new(app)
                .text("start", "Start recording")
                .text("stop", "Stop recording")
                .separator()
                .text("dashboard", "Open dashboard")
                .text("summon", "Summon copilot")
                .separator()
                .text("check-updates", "Check for updates")
                .text("quit", "Quit overlay")
                .build()?;
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Auricle Copilot")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "start" => tray_action(app.clone(), "start"),
                    "stop" => tray_action(app.clone(), "stop"),
                    "dashboard" => open_dashboard(app.clone()),
                    "summon" => summon(app, false),
                    "check-updates" => {
                        tauri::async_runtime::spawn(check_for_updates(app.clone(), true));
                    }
                    "quit" => {
                        tauri::async_runtime::spawn(quit(app.clone()));
                    }
                    _ => {}
                })
                .build(app)?;

            // Attach to a running engine or spawn one, then keep it alive
            // (managed child; not lifetime-tied — an overlay crash must
            // never kill a recording).
            tauri::async_runtime::spawn(watch_engine(app.handle().clone()));

            let state = app.state::<AppState>();
            tauri::async_runtime::spawn(dictation::run(
                app.handle().clone(),
                cfg.dictation.mode,
                state.http.clone(),
                state.base_url().to_string(),
                esc_sc,
                dictation_rx,
                dictation_tx,
            ));

            // Silent update check on launch: nudge (never auto-install) if a
            // newer signed release is waiting.
            let update_handle = app.handle().clone();
            tauri::async_runtime::spawn(check_for_updates(update_handle, false));

            // Launching the app shows the dashboard (it waits for the
            // engine spawned above to answer /health). The copilot overlay
            // stays hotkey-summoned — a tray-only launch looked like
            // nothing happened.
            open_dashboard(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| match event {
            // Both windows live under the tray, so close means hide: the
            // frameless overlay still gets close requests (Alt+F4), and
            // closing the dashboard must not quit the resident app.
            tauri::WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
            }
            // Overlay only — click-through when idle, interactive when
            // focused: losing focus makes the visible overlay a
            // bystander; the summon hotkey (or alt-tab) makes it
            // interactive again.
            //
            // Focused(false) ALSO fires when focus merely moves into the
            // WebView2 child (the ask input autofocuses on summon) — the
            // overlay is still the foreground window then, and going
            // click-through would kill every mouse target (the × button)
            // while the caret blinks in the input. Keep the window
            // interactive while the OS foreground window is still us.
            tauri::WindowEvent::Focused(focused) if window.label() == "main" => {
                let interactive = *focused || foreground_is_window(window);
                let _ = window.set_ignore_cursor_events(!interactive);
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            ask_stream,
            get_health,
            get_init,
            hide_overlay,
            resize_overlay
        ])
        .build(tauri::generate_context!())
        .expect("error while running the overlay");

    // Every exit of ours goes through ExitRequested. An Exit without one can
    // only be WM_ENDSESSION, which the event loop reports this way right
    // before it ends the process.
    let mut exit_requested = false;
    app.run(move |app, event| match event {
        tauri::RunEvent::ExitRequested { .. } => exit_requested = true,
        tauri::RunEvent::Exit if !exit_requested => on_session_end(app),
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::tests::{
        closed_port, fake_engine, record_pid, temp_dir, IDLE, OUR_PID, RECORDING, RECORDING_OURS,
    };
    use engine::{Owned, QuitStep};

    fn ours(_: &engine::Probe) -> Owned {
        Owned::Alive
    }

    #[test]
    fn a_window_inside_the_work_area_stays_put() {
        let moved = keep_on_screen((100, 200), (420, 300), (0, 0), (1920, 1040));
        assert_eq!(moved, (100, 200));
    }

    #[test]
    fn growth_past_the_bottom_edge_pushes_the_window_back_up() {
        let moved = keep_on_screen((1000, 900), (420, 300), (0, 0), (1920, 1040));
        assert_eq!(moved, (1000, 740));
    }

    #[test]
    fn a_second_monitor_left_of_the_primary_is_respected() {
        let moved = keep_on_screen((-2000, 100), (420, 300), (-1920, 0), (1920, 1040));
        assert_eq!(moved, (-1920, 100));
    }

    #[test]
    fn a_window_too_big_for_the_area_keeps_its_top_left_on_screen() {
        let moved = keep_on_screen((50, 50), (420, 900), (0, 0), (1920, 720));
        assert_eq!(moved, (50, 0));
    }

    #[tokio::test]
    async fn stopping_on_quit_stops_the_active_recording() {
        let engine = fake_engine(RECORDING, 202, false);
        let http = reqwest::Client::new();
        stop_recording(&http, &engine.base).await.unwrap();
        let posts = engine.posts.lock().unwrap();
        assert_eq!(posts.len(), 1);
        assert!(
            posts[0].starts_with("POST /api/v1/sessions/s1/stop"),
            "{posts:?}"
        );
    }

    #[tokio::test]
    async fn stopping_on_quit_leaves_an_idle_engine_alone() {
        let engine = fake_engine(IDLE, 202, false);
        let http = reqwest::Client::new();
        stop_recording(&http, &engine.base).await.unwrap();
        assert!(engine.posts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stopping_on_quit_is_fine_with_no_engine_at_all() {
        let http = reqwest::Client::new();
        assert_eq!(stop_recording(&http, &closed_port()).await, Ok(()));
    }

    #[tokio::test]
    async fn quit_leaves_a_recording_it_does_not_own_running_however_often_it_is_pressed() {
        let engine = fake_engine(RECORDING, 202, false);
        let http = reqwest::Client::new();
        let dir = temp_dir("quit-foreign");
        let guard = Mutex::default();
        for _ in 0..3 {
            let step = quit_step(&http, &engine.base, &dir, &guard).await;
            assert_eq!(step, QuitStep::Exit);
        }
        assert!(engine.posts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn quit_goes_by_the_engine_answering_not_a_stale_record() {
        let engine = fake_engine(RECORDING_OURS, 202, false);
        let http = reqwest::Client::new();
        let dir = temp_dir("quit-stale");
        record_pid(&dir, 7);
        let guard = Mutex::default();
        let step = quit_step(&http, &engine.base, &dir, &guard).await;
        assert_eq!(step, QuitStep::Exit);
    }

    #[tokio::test]
    async fn quit_warns_before_it_stops_a_recording_of_ours() {
        let engine = fake_engine(RECORDING_OURS, 202, false);
        let http = reqwest::Client::new();
        let dir = temp_dir("quit-ours");
        record_pid(&dir, OUR_PID);
        let guard = Mutex::default();

        let first = quit_step(&http, &engine.base, &dir, &guard).await;
        assert_eq!(first, QuitStep::Warn);
        assert!(engine.posts.lock().unwrap().is_empty());

        let second = quit_step(&http, &engine.base, &dir, &guard).await;
        assert_eq!(second, QuitStep::StopThenExit);
    }

    #[tokio::test]
    async fn quit_just_exits_when_nothing_answers() {
        let http = reqwest::Client::new();
        let dir = temp_dir("quit-down");
        let guard = Mutex::default();
        let step = quit_step(&http, &closed_port(), &dir, &guard).await;
        assert_eq!(step, QuitStep::Exit);
    }

    #[tokio::test]
    async fn a_session_end_stops_the_engine_and_waits_for_it_to_go() {
        let engine = fake_engine(RECORDING, 202, true);
        let http = reqwest::Client::new();

        stop_engine_for_session_end(&http, &engine.base, Duration::from_secs(10), ours).await;

        let after = engine::probe(&http, &engine.base).await;
        assert_eq!(after, engine::Probe::Down);
        let posts = engine.posts.lock().unwrap();
        assert_eq!(posts.len(), 1, "{posts:?}");
        assert!(posts[0].starts_with("POST /api/v1/shutdown"), "{posts:?}");
    }

    #[tokio::test]
    async fn a_session_end_never_outlasts_its_budget() {
        // Takes the request and keeps answering /health: it never exits.
        let engine = fake_engine(RECORDING, 202, false);
        let http = reqwest::Client::new();
        let budget = Duration::from_millis(300);

        let started = Instant::now();
        stop_engine_for_session_end(&http, &engine.base, budget, ours).await;

        assert!(started.elapsed() < budget * 4, "{:?}", started.elapsed());
        assert_eq!(engine.posts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_session_end_does_not_wait_on_an_engine_that_refuses() {
        let engine = fake_engine(IDLE, 500, false);
        let http = reqwest::Client::new();

        let started = Instant::now();
        stop_engine_for_session_end(&http, &engine.base, Duration::from_secs(10), ours).await;

        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_session_end_is_fine_with_no_engine_at_all() {
        let http = reqwest::Client::new();
        let started = Instant::now();
        stop_engine_for_session_end(&http, &closed_port(), Duration::from_secs(10), ours).await;
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_session_end_only_stops_an_engine_confirmed_ours() {
        let http = reqwest::Client::new();
        for owned in [Owned::No, Owned::Dead] {
            let engine = fake_engine(RECORDING, 202, true);
            stop_engine_for_session_end(&http, &engine.base, Duration::from_secs(10), move |_| {
                owned
            })
            .await;
            assert!(engine.posts.lock().unwrap().is_empty(), "{owned:?}");
        }
    }

    #[tokio::test]
    async fn a_session_end_leaves_alone_an_engine_a_stale_record_does_not_name() {
        let engine = fake_engine(RECORDING_OURS, 202, true);
        let http = reqwest::Client::new();
        let dir = temp_dir("end-stale");
        record_pid(&dir, 7);

        stop_engine_for_session_end(&http, &engine.base, Duration::from_secs(10), move |probe| {
            engine::owner(&dir, probe)
        })
        .await;

        assert!(engine.posts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_slow_ownership_check_cannot_outlast_the_session_end_budget() {
        let engine = fake_engine(RECORDING, 202, true);
        let http = reqwest::Client::new();
        let budget = Duration::from_millis(300);

        let started = Instant::now();
        stop_engine_for_session_end(&http, &engine.base, budget, |_| {
            std::thread::sleep(Duration::from_secs(10));
            Owned::Alive
        })
        .await;

        assert!(started.elapsed() < budget * 4, "{:?}", started.elapsed());
        assert!(engine.posts.lock().unwrap().is_empty());
    }
}
