//! Dev harness for acceptance checks: capture the active window, or a
//! specific HWND (hex) to exercise the typed error paths (minimized,
//! destroyed window) against real windows.
//!
//!   cargo run -p auricle-vision --example capture_hwnd            # active
//!   cargo run -p auricle-vision --example capture_hwnd -- 0x1234  # by HWND

use std::time::Instant;

use auricle_vision::{ScreenReader, WindowsOcrReader};

fn main() {
    let reader = WindowsOcrReader::new();
    let arg = std::env::args().nth(1);
    let started = Instant::now();
    let result = match &arg {
        Some(hex) => {
            let raw = isize::from_str_radix(hex.trim_start_matches("0x"), 16)
                .expect("HWND must be hex, e.g. 0x1234");
            reader.capture_window(raw)
        }
        None => reader.capture_active_window(None),
    };
    let total_ms = started.elapsed().as_millis();
    match result {
        Ok(ctx) => {
            println!(
                "window: {:?} app: {:?} ocr: {} ms total: {} ms chars: {}",
                ctx.window_title,
                ctx.app_name,
                ctx.ocr_ms,
                total_ms,
                ctx.text.chars().count()
            );
            println!("{}", ctx.text);
        }
        Err(e) => {
            println!("error ({total_ms} ms): {e}");
            std::process::exit(1);
        }
    }
}
