//! POST /api/v1/peek against a mocked ScreenReader: response shape and
//! the VisionError → HTTP status mapping (real capture is exercised by
//! `auricle peek` / the auricle-vision example, not in CI).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use auricle_core::Config;
use auricle_server::{build_router_with_reader, Engine, EngineOptions};
use auricle_vision::{ScreenContext, ScreenReader, VisionError};

/// Plays back scripted results and records the exclude arg it was given.
struct MockReader {
    script: Mutex<VecDeque<Result<ScreenContext, VisionError>>>,
    excludes_seen: Mutex<Vec<Option<isize>>>,
}

impl ScreenReader for MockReader {
    fn capture_active_window(
        &self,
        exclude_hwnd: Option<isize>,
    ) -> Result<ScreenContext, VisionError> {
        self.excludes_seen.lock().unwrap().push(exclude_hwnd);
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .expect("unscripted peek call")
    }
}

async fn spawn_with_script(
    script: Vec<Result<ScreenContext, VisionError>>,
) -> (String, Arc<MockReader>) {
    let dir = std::env::temp_dir().join(format!("auricle-peek-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(EngineOptions {
        cfg: Config::default(),
        data_root: dir,
        provider_override: None,
    })
    .unwrap();
    let reader = Arc::new(MockReader {
        script: Mutex::new(script.into()),
        excludes_seen: Mutex::new(Vec::new()),
    });
    let app = build_router_with_reader(engine, None, reader.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), reader)
}

#[tokio::test]
async fn peek_returns_the_screen_context_and_maps_errors() {
    let ctx = ScreenContext {
        window_title: "Quarterly Review - Notes".to_string(),
        app_name: "msedge".to_string(),
        text: "Agenda\nQ3 numbers Q4 plan".to_string(),
        captured_at: 1_784_000_000_000,
        ocr_ms: 142,
    };
    let (base, reader) = spawn_with_script(vec![
        Ok(ctx),
        Err(VisionError::Minimized),
        Err(VisionError::NoWindow),
        Err(VisionError::OcrLanguageMissing),
        Err(VisionError::CaptureDenied("blocked by policy".to_string())),
    ])
    .await;
    let client = reqwest::Client::new();
    let peek = || client.post(format!("{base}/api/v1/peek")).send();

    // Success: the ScreenContext comes back verbatim as JSON.
    let resp = peek().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["window_title"], "Quarterly Review - Notes");
    assert_eq!(body["app_name"], "msedge");
    assert_eq!(body["text"], "Agenda\nQ3 numbers Q4 plan");
    assert_eq!(body["captured_at"], 1_784_000_000_000u64);
    assert_eq!(body["ocr_ms"], 142);

    // Transient desktop states are 409 (retryable), machine-level
    // problems are 500 — each with the typed message.
    for (status, needle) in [
        (409, "minimized"),
        (409, "no capturable window"),
        (500, "language pack"),
        (500, "blocked by policy"),
    ] {
        let resp = peek().await.unwrap();
        assert_eq!(resp.status(), status, "for expected {needle:?}");
        let body: serde_json::Value = resp.json().await.unwrap();
        let msg = body["error"].as_str().unwrap();
        assert!(msg.contains(needle), "{msg:?} should contain {needle:?}");
    }

    // Phase 7 passes no exclusion; PHASE-8 plumbs the overlay HWND here.
    let seen = reader.excludes_seen.lock().unwrap();
    assert_eq!(seen.as_slice(), &[None; 5]);
}
