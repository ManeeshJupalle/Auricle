//! Local crash diagnostics.
//!
//! A panic hook appends a structured record (message, location, thread,
//! backtrace, version, OS) to `<data_root>/diagnostics.jsonl` for every
//! panic — including panics in spawned tasks, which tokio catches but the
//! hook still sees. This is **local only**: nothing is ever sent anywhere.
//! `GET /api/v1/diagnostics` exposes recent records + system info so a user
//! can copy a bundle into a bug report on their own terms. No user content
//! (transcripts, prompts, audio) is included — only code-level crash data.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const FILE: &str = "diagnostics.jsonl";

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn crash_record(message: &str, location: &str, thread: &str, backtrace: &str) -> serde_json::Value {
    serde_json::json!({
        "ts": now_secs(),
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "thread": thread,
        "message": message,
        "location": location,
        "backtrace": backtrace,
    })
}

fn append_record(dir: &Path, rec: &serde_json::Value) {
    let _ = std::fs::create_dir_all(dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(FILE))
    {
        let _ = writeln!(f, "{rec}");
    }
}

/// Install a panic hook that records crashes under `dir`, then chains to the
/// previous hook so stderr output is preserved. Must not itself panic.
pub fn install_panic_hook(dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_default();
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        append_record(
            &dir,
            &crash_record(&message, &location, &thread, &backtrace),
        );
        previous(info);
    }));
}

/// The most recent crash records under `dir`, newest first (empty if none).
pub fn recent_crashes(dir: &Path, limit: usize) -> Vec<serde_json::Value> {
    let Ok(content) = std::fs::read_to_string(dir.join(FILE)) else {
        return Vec::new();
    };
    let mut recs: Vec<serde_json::Value> = content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    recs.reverse();
    recs.truncate(limit);
    recs
}

/// Build/runtime identity, shown alongside crashes in a diagnostics bundle.
pub fn system_info() -> serde_json::Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("auricle-diag-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn records_round_trip_newest_first() {
        let dir = temp_dir("rt");
        append_record(&dir, &crash_record("first", "a.rs:1:1", "main", ""));
        append_record(&dir, &crash_record("second", "b.rs:2:2", "worker", ""));
        let recs = recent_crashes(&dir, 10);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0]["message"], "second");
        assert_eq!(recs[1]["message"], "first");
        assert_eq!(recs[0]["thread"], "worker");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn limit_keeps_newest() {
        let dir = temp_dir("limit");
        for i in 0..5 {
            append_record(&dir, &crash_record(&format!("p{i}"), "x", "t", ""));
        }
        let recs = recent_crashes(&dir, 2);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0]["message"], "p4");
        assert_eq!(recs[1]["message"], "p3");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_when_no_file() {
        assert!(recent_crashes(&temp_dir("none"), 10).is_empty());
    }

    #[test]
    fn system_info_has_fields() {
        let s = system_info();
        assert!(s["version"].is_string());
        assert!(s["os"].is_string());
        assert!(s["arch"].is_string());
    }
}
