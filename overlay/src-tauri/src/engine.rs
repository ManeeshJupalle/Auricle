//! Engine child management: attach to a running `auricle serve` via its
//! public /health endpoint, or spawn one as a managed child. The child is
//! deliberately NOT tied to the overlay's lifetime (no job object): an
//! overlay crash must leave a recording engine untouched (acceptance
//! gate), and Quit leaves the engine running for the same reason.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Where the engine executable was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineExe {
    /// Explicit override via the AURICLE_EXE environment variable.
    EnvOverride(PathBuf),
    /// `auricle.exe` next to the overlay executable (the MSI sidecar).
    Adjacent(PathBuf),
    /// Bare name — resolved through PATH by CreateProcess.
    OnPath,
}

impl EngineExe {
    pub fn command(&self) -> Command {
        match self {
            EngineExe::EnvOverride(p) | EngineExe::Adjacent(p) => Command::new(p),
            EngineExe::OnPath => Command::new("auricle"),
        }
    }
}

/// Resolution order: env override → exe-adjacent sidecar → PATH. Pure
/// given its inputs (unit-tested); the caller passes the real env/exe dir.
///
/// The PATH fallback is a deliberate trade-off (audit finding, accepted):
/// MSI installs always have the adjacent sidecar and never reach it; it
/// exists for `cargo install auricle-cli` users running the bare overlay
/// exe. A hostile writable PATH entry is already game over for every
/// program on the machine, not something this lookup can defend.
pub fn resolve_engine_exe(env_override: Option<PathBuf>, exe_dir: Option<&Path>) -> EngineExe {
    if let Some(p) = env_override {
        return EngineExe::EnvOverride(p);
    }
    if let Some(dir) = exe_dir {
        let candidate = dir.join("auricle.exe");
        if candidate.is_file() {
            return EngineExe::Adjacent(candidate);
        }
    }
    EngineExe::OnPath
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attach {
    /// A daemon was already answering /health — nothing spawned.
    Attached,
    /// We spawned `auricle serve` (pid) and it became healthy.
    Spawned(u32),
}

pub async fn health_ok(client: &reqwest::Client, base_url: &str) -> bool {
    let Ok(resp) = client
        .get(format!("{base_url}/api/v1/health"))
        .timeout(Duration::from_millis(1500))
        .send()
        .await
    else {
        return false;
    };
    if !resp.status().is_success() {
        return false;
    }
    // Impersonation guard: any local process can bind the port first (an
    // unrelated dev server squatted :1420 during Phase 9 verification —
    // same failure mode). Before attaching and routing asks here, require
    // the response to carry Auricle's health shape. Not cryptographic
    // (loopback processes are mutually trusted by design, see API.md),
    // but it stops accidental port squatters cold.
    match resp.json::<serde_json::Value>().await {
        Ok(v) => is_engine_health(&v),
        Err(_) => false,
    }
}

/// True when a `/health` body carries Auricle's documented shape
/// (`status: "ok"` + string `version` + string `state`). Shared by the
/// attach probe and the steady-state health poll so a port squatter is
/// rejected in both, not only at attach time.
pub fn is_engine_health(v: &serde_json::Value) -> bool {
    v["status"] == "ok" && v["version"].is_string() && v["state"].is_string()
}

/// Attach to a running engine or spawn one and wait for it to answer.
pub async fn attach_or_spawn(
    client: &reqwest::Client,
    base_url: &str,
    exe: EngineExe,
) -> Result<Attach, String> {
    if health_ok(client, base_url).await {
        return Ok(Attach::Attached);
    }

    let mut cmd = exe.command();
    cmd.arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        // No console window for the background daemon.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("could not start the engine ({exe:?}): {e}"))?;
    let pid = child.id();
    // The Child handle is intentionally dropped: no kill-on-drop, no job
    // object. The daemon owns its own lifetime from here.

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        if health_ok(client, base_url).await {
            return Ok(Attach::Spawned(pid));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(format!(
        "spawned the engine (pid {pid}) but {base_url}/api/v1/health did not answer within 20 s"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("auricle-ov-eng-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn env_override_wins_over_everything() {
        let dir = temp_dir("env");
        std::fs::write(dir.join("auricle.exe"), b"x").unwrap();
        let exe = resolve_engine_exe(Some(PathBuf::from("C:/custom/auricle.exe")), Some(&dir));
        assert_eq!(
            exe,
            EngineExe::EnvOverride(PathBuf::from("C:/custom/auricle.exe"))
        );
    }

    #[test]
    fn adjacent_sidecar_beats_path() {
        let dir = temp_dir("adjacent");
        std::fs::write(dir.join("auricle.exe"), b"x").unwrap();
        let exe = resolve_engine_exe(None, Some(&dir));
        assert_eq!(exe, EngineExe::Adjacent(dir.join("auricle.exe")));
    }

    #[test]
    fn falls_back_to_path_when_nothing_else_exists() {
        let dir = temp_dir("path");
        assert_eq!(resolve_engine_exe(None, Some(&dir)), EngineExe::OnPath);
        assert_eq!(resolve_engine_exe(None, None), EngineExe::OnPath);
    }

    /// Serve a fixed HTTP body on an ephemeral port.
    fn serve_body(body: &'static str) -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for stream in listener.incoming().flatten() {
                let mut s = stream;
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(response.as_bytes());
            }
        });
        addr
    }

    #[tokio::test]
    async fn attach_when_auricle_health_answers_spawns_nothing() {
        // The real health shape (API.md): status + version + state.
        let addr =
            serve_body(r#"{"active_session":null,"state":"idle","status":"ok","version":"0.3.0"}"#);
        let client = reqwest::Client::new();
        // A nonexistent exe path proves nothing is spawned on this path.
        let exe = EngineExe::EnvOverride(PathBuf::from("Z:/does/not/exist.exe"));
        let result = attach_or_spawn(&client, &format!("http://{addr}"), exe).await;
        assert_eq!(result, Ok(Attach::Attached));
    }

    #[tokio::test]
    async fn a_port_squatter_is_not_attached_to() {
        // 200 OK from something that is not Auricle: never attach — fall
        // through to the spawn path (which fails here, proving the fall).
        let addr = serve_body(r#"{"hello":"world"}"#);
        let client = reqwest::Client::new();
        let exe = EngineExe::EnvOverride(PathBuf::from("Z:/does/not/exist.exe"));
        let err = attach_or_spawn(&client, &format!("http://{addr}"), exe)
            .await
            .unwrap_err();
        assert!(err.contains("could not start the engine"), "{err}");
    }

    #[tokio::test]
    async fn spawn_failure_is_a_clean_error() {
        let client = reqwest::Client::new();
        let exe = EngineExe::EnvOverride(PathBuf::from("Z:/does/not/exist.exe"));
        // Nothing listens on this port, so it tries to spawn and fails.
        let err = attach_or_spawn(&client, "http://127.0.0.1:9", exe)
            .await
            .unwrap_err();
        assert!(err.contains("could not start the engine"), "{err}");
    }
}
