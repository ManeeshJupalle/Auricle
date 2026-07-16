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

mod config;
mod engine;
mod sse;

use std::sync::Mutex;

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tauri::ipc::Channel;
use tauri::menu::MenuBuilder;
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, PhysicalPosition, WebviewWindow};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use config::{Corner, OverlayConfig};

struct AppState {
    cfg: OverlayConfig,
    http: reqwest::Client,
    hotkey_warnings: Mutex<Vec<String>>,
}

impl AppState {
    fn base_url(&self) -> &str {
        &self.cfg.overlay.engine_url
    }
}

/// Dock the window to the configured corner of the current monitor's
/// work area (12 logical px margin).
fn position_window(win: &WebviewWindow, corner: Corner) {
    let Ok(Some(monitor)) = win.current_monitor() else {
        return;
    };
    let Ok(size) = win.outer_size() else { return };
    let work = monitor.work_area();
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
}

/// Show + focus the overlay and tell the webview why. The hotkey press
/// itself grants foreground rights (Phase 7 finding 14: focus cannot be
/// stolen, only earned — RegisterHotKey earns it).
fn summon(app: &tauri::AppHandle, quick: bool) {
    let state = app.state::<AppState>();
    let corner = state.cfg.overlay.corner;
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    // The overlay is a normal window with a taskbar button, so the user
    // can minimize it — the hotkey must recover from that state too.
    let _ = win.unminimize();
    position_window(&win, corner);
    let _ = win.set_ignore_cursor_events(false);
    let _ = win.show();
    let _ = win.set_focus();
    let _ = app.emit(if quick { "quick-assist" } else { "summon" }, ());
}

#[derive(Debug, Deserialize)]
struct AskRequest {
    question: String,
    include_screen: bool,
    include_transcript: bool,
    follow_up: bool,
}

/// Run one /ask against the engine, forwarding each SSE event to the
/// webview through `on_event`. The overlay's own HWND rides along as
/// exclude_hwnd so the capture never OCRs the overlay itself.
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
    let resp = state
        .http
        .post(format!("{}/api/v1/ask", state.base_url()))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("engine unreachable: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let detail = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["error"].as_str().map(String::from))
            .unwrap_or(text);
        return Err(format!("{detail} (HTTP {status})"));
    }

    let mut parser = sse::SseLineParser::default();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("answer stream broke: {e}"))?;
        for payload in parser.push(&String::from_utf8_lossy(&chunk)) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) {
                let _ = on_event.send(v);
            }
        }
    }
    Ok(())
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
    resp.json().await.map_err(|e| e.to_string())
}

#[tauri::command]
fn get_init(app: tauri::AppHandle) -> serde_json::Value {
    let state = app.state::<AppState>();
    json!({
        "hotkey_warnings": *state.hotkey_warnings.lock().unwrap(),
        "window_min": state.cfg.overlay.transcript_window_min,
        "theme": state.cfg.overlay.theme,
        "summon_hotkey": state.cfg.hotkeys.summon,
        "quick_hotkey": state.cfg.hotkeys.quick,
    })
}

#[tauri::command]
fn hide_overlay(window: tauri::Window) {
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
        position_window(&win, state.cfg.overlay.corner);
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

    let handler_summon = summon_sc;
    let handler_quick = quick_sc;

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
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
        })
        .setup(move |app| {
            // Hotkey registration: a conflict (another app owns the
            // combination) must be visible in the overlay, not silent.
            for (label, sc, text) in [
                ("summon", summon_sc, cfg.hotkeys.summon.clone()),
                ("quick-assist", quick_sc, cfg.hotkeys.quick.clone()),
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

            // Tray: the persistent entry point.
            let menu = MenuBuilder::new(app)
                .text("start", "Start recording")
                .text("stop", "Stop recording")
                .separator()
                .text("dashboard", "Open dashboard")
                .text("summon", "Summon copilot")
                .separator()
                .text("quit", "Quit overlay")
                .build()?;
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Auricle Copilot")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "start" => tray_action(app.clone(), "start"),
                    "stop" => tray_action(app.clone(), "stop"),
                    "dashboard" => {
                        use tauri_plugin_opener::OpenerExt;
                        let url = app.state::<AppState>().base_url().to_string();
                        let _ = app.opener().open_url(url, None::<&str>);
                    }
                    "summon" => summon(app, false),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // Attach to a running engine or spawn one (managed child; not
            // lifetime-tied — an overlay crash must never kill a recording).
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<AppState>();
                let exe = engine::resolve_engine_exe(
                    std::env::var_os("AURICLE_EXE").map(std::path::PathBuf::from),
                    std::env::current_exe()
                        .ok()
                        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                        .as_deref(),
                );
                match engine::attach_or_spawn(&state.http, state.base_url(), exe).await {
                    Ok(engine::Attach::Attached) => {}
                    Ok(engine::Attach::Spawned(pid)) => {
                        eprintln!("overlay spawned auricle serve (pid {pid})");
                    }
                    Err(e) => {
                        let _ = handle.emit("tray-error", format!("engine startup: {e}"));
                    }
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| match event {
            // Frameless windows still get close requests (Alt+F4): the
            // overlay lives in the tray, so close means hide.
            tauri::WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
            }
            // Click-through when idle, interactive when focused: losing
            // focus makes the visible overlay a bystander; the summon
            // hotkey (or alt-tab) makes it interactive again.
            tauri::WindowEvent::Focused(focused) => {
                let _ = window.set_ignore_cursor_events(!focused);
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
        .run(tauri::generate_context!())
        .expect("error while running the overlay");
}
