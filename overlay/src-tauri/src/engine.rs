//! Engine child management: attach to a running `auricle serve` via its
//! public /health endpoint, or spawn one as a managed child. The child is
//! deliberately NOT tied to the overlay's lifetime (no job object): an
//! overlay crash must leave a recording engine untouched (acceptance
//! gate), and Quit leaves the engine running for the same reason.
//!
//! Because the child outlives the overlay, "an engine we spawned" has to
//! survive an overlay restart too: `engine.pid` in the data dir records it.
//! Only an engine recorded there is ever restarted. Anything else on the
//! port is someone's own `auricle serve`, or not Auricle at all.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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

/// What is on the engine port right now.
#[derive(Debug, Clone, PartialEq)]
pub enum Probe {
    /// Auricle answered; the value is its /health body.
    Engine(serde_json::Value),
    /// Something answered, but it is not Auricle.
    Squatter,
    /// Nothing answered.
    Down,
}

pub async fn probe(client: &reqwest::Client, base_url: &str) -> Probe {
    let Ok(resp) = client
        .get(format!("{base_url}/api/v1/health"))
        .timeout(Duration::from_millis(1500))
        .send()
        .await
    else {
        return Probe::Down;
    };
    if !resp.status().is_success() {
        return Probe::Squatter;
    }
    // Impersonation guard: any local process can bind the port first (an
    // unrelated dev server squatted :1420 during Phase 9 verification —
    // same failure mode). Before attaching and routing asks here, require
    // the response to carry Auricle's health shape. Not cryptographic
    // (loopback processes are mutually trusted by design, see API.md),
    // but it stops accidental port squatters cold.
    match resp.json::<serde_json::Value>().await {
        Ok(v) if is_engine_health(&v) => Probe::Engine(v),
        _ => Probe::Squatter,
    }
}

pub async fn health_ok(client: &reqwest::Client, base_url: &str) -> bool {
    matches!(probe(client, base_url).await, Probe::Engine(_))
}

/// True when a `/health` body carries Auricle's documented shape
/// (`status: "ok"` + string `version` + string `state`). Shared by the
/// attach probe and the steady-state health poll so a port squatter is
/// rejected in both, not only at attach time.
pub fn is_engine_health(v: &serde_json::Value) -> bool {
    v["status"] == "ok" && v["version"].is_string() && v["state"].is_string()
}

/// True while a session holds the engine: recording, or still stopping.
pub fn is_busy(health: &serde_json::Value) -> bool {
    health["state"] != "idle"
}

/// The tray tooltip for what a probe found.
pub fn tray_tooltip(probe: &Probe) -> &'static str {
    match probe {
        Probe::Engine(health) if health["state"] == "recording" => "Auricle Copilot: recording",
        Probe::Engine(health) if is_busy(health) => "Auricle Copilot: finishing a recording",
        Probe::Engine(_) => "Auricle Copilot",
        _ => "Auricle Copilot: engine offline",
    }
}

pub fn squatter_message(base_url: &str) -> String {
    format!(
        "{base_url} is held by something that is not Auricle, so the engine cannot start. \
         Free that port."
    )
}

fn log_path(dir: &Path) -> PathBuf {
    dir.join("engine.log")
}

fn pid_path(dir: &Path) -> PathBuf {
    dir.join("engine.pid")
}

fn read_pid(dir: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_path(dir))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn clear_pid(dir: &Path) {
    let _ = std::fs::remove_file(pid_path(dir));
}

/// Open a fresh log for one spawn. One run per file means the tail is
/// always this child's words; the previous run stays in engine.log.old.
fn open_log(dir: &Path) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(dir)?;
    let path = log_path(dir);
    let _ = std::fs::rename(&path, path.with_extension("log.old"));
    std::fs::File::create(path)
}

/// The end of the engine's stderr: enough to say why it stopped, short
/// enough for a toast.
pub fn log_tail(dir: &Path) -> String {
    const MAX_CHARS: usize = 300;
    let bytes = std::fs::read(log_path(dir)).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(3)..].join(" | ");
    let start = tail
        .char_indices()
        .rev()
        .nth(MAX_CHARS - 1)
        .map_or(0, |(i, _)| i);
    tail[start..].to_string()
}

fn exit_reason(status: ExitStatus, dir: &Path) -> String {
    match log_tail(dir).as_str() {
        "" => format!("the engine exited at startup ({status})"),
        tail => format!("the engine exited at startup ({status}): {tail}"),
    }
}

/// True when `pid` is a running `auricle.exe`. Matching the image name
/// keeps a recycled pid from passing for the engine. `tasklist` costs a
/// process, so this only runs while the engine is not answering.
#[cfg(windows)]
pub fn is_engine_process(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;
    Command::new("tasklist")
        .args(["/NH", "/FO", "CSV", "/FI", &format!("PID eq {pid}")])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .is_ok_and(|out| {
            String::from_utf8_lossy(&out.stdout)
                .to_ascii_lowercase()
                .starts_with("\"auricle.exe\"")
        })
}

#[cfg(not(windows))]
pub fn is_engine_process(_pid: u32) -> bool {
    false
}

/// Whether the engine this overlay is responsible for exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owned {
    /// No record of spawning one: whatever runs here is not ours.
    No,
    /// The engine we spawned is still a live process.
    Alive,
    /// We spawned one and it is gone.
    Dead,
}

pub fn owned(dir: &Path) -> Owned {
    match read_pid(dir) {
        None => Owned::No,
        Some(pid) if is_engine_process(pid) => Owned::Alive,
        Some(_) => Owned::Dead,
    }
}

/// Who owns what `probe` found. A pid in the /health answer settles it: the
/// image name alone can't, since a recycled pid can land on another
/// auricle.exe. A different pid answering means the record is out of date,
/// which reads as `Dead`. Older engines report no pid and get the `owned` check.
pub fn owner(dir: &Path, probe: &Probe) -> Owned {
    let answered = match probe {
        Probe::Engine(health) => health["pid"].as_u64(),
        _ => None,
    };
    match (read_pid(dir), answered) {
        (None, _) => Owned::No,
        (Some(recorded), Some(pid)) if u64::from(recorded) == pid => Owned::Alive,
        (Some(_), Some(_)) => Owned::Dead,
        (Some(_), None) => owned(dir),
    }
}

/// How long the warning from a first Quit stays good for a second one.
pub const QUIT_CONFIRM_WINDOW: Duration = Duration::from_secs(10);

/// What one press of Quit does.
#[derive(Debug, PartialEq, Eq)]
pub enum QuitStep {
    /// Nothing of ours is recording: exit and leave the engine as it is.
    Exit,
    /// Our engine is recording: say so, and do nothing else yet.
    Warn,
    /// The user was warned and chose Quit again.
    StopThenExit,
}

/// Quit's two-step confirm. Quit leaves the engine running, and a recording
/// nothing shows anymore is one nobody can stop, so a recording of ours has
/// to end first. That is the user's call to make. One that isn't ours is
/// never touched: whoever started it still has it.
#[derive(Debug, Default)]
pub struct QuitGuard {
    warned_at: Option<Instant>,
}

impl QuitGuard {
    /// Judged afresh on every press, so a recording that ended or an engine
    /// that changed hands since the warning counts for what it is now.
    pub fn press(&mut self, owned: Owned, recording: bool, now: Instant) -> QuitStep {
        let warned_at = self.warned_at.take();
        if owned != Owned::Alive || !recording {
            return QuitStep::Exit;
        }
        if warned_at.is_some_and(|at| now.duration_since(at) <= QUIT_CONFIRM_WINDOW) {
            return QuitStep::StopThenExit;
        }
        self.warned_at = Some(now);
        QuitStep::Warn
    }
}

/// Attach to a running engine or spawn one and wait for it to answer.
/// `dir` is the data dir, home of the child's stderr log and pid record.
pub async fn attach_or_spawn(
    client: &reqwest::Client,
    base_url: &str,
    exe: &EngineExe,
    dir: &Path,
) -> Result<Attach, String> {
    match probe(client, base_url).await {
        Probe::Engine(_) => return Ok(Attach::Attached),
        Probe::Squatter => return Err(squatter_message(base_url)),
        Probe::Down => {}
    }

    let mut cmd = exe.command();
    cmd.arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // A log problem must not stop the engine from starting.
        .stderr(open_log(dir).map_or_else(|_| Stdio::null(), Stdio::from));
    #[cfg(windows)]
    {
        // No console window for the background daemon.
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not start the engine ({exe:?}): {e}"))?;
    let pid = child.id();
    let _ = std::fs::write(pid_path(dir), pid.to_string());
    // The Child is dropped when this returns: no kill-on-drop, no job
    // object. The daemon owns its own lifetime from here.

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        // Before the health check: a child that exited can't be what is
        // answering, so an answer now would be an engine that beat it to
        // the port, and an attach rather than a spawn.
        if let Ok(Some(status)) = child.try_wait() {
            clear_pid(dir);
            if health_ok(client, base_url).await {
                return Ok(Attach::Attached);
            }
            return Err(exit_reason(status, dir));
        }
        if health_ok(client, base_url).await {
            return Ok(Attach::Spawned(pid));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(format!(
        "spawned the engine (pid {pid}) but {base_url}/api/v1/health did not answer within 20 s"
    ))
}

/// Ask the engine to exit and wait until it has: the update installer needs
/// the process gone, not just the port, or auricle.exe stays locked. Only for
/// an engine we own.
pub async fn stop_engine(
    client: &reqwest::Client,
    base_url: &str,
    dir: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let pid = read_pid(dir);
    let resp = client
        .post(format!("{base_url}/api/v1/shutdown"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("could not reach the engine to stop it: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(ENGINE_TOO_OLD_TO_STOP.to_string());
    }
    if !resp.status().is_success() {
        return Err(format!(
            "the engine refused to shut down (HTTP {})",
            resp.status()
        ));
    }
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let port_closed = probe(client, base_url).await == Probe::Down;
        if port_closed && !pid.is_some_and(is_engine_process) {
            clear_pid(dir);
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(format!(
        "the engine did not exit within {} s",
        timeout.as_secs()
    ))
}

/// What an update does about whatever is on the engine port.
#[derive(Debug, PartialEq, Eq)]
pub enum UpdatePlan {
    /// No engine stands in the way.
    Install,
    /// Our own idle engine has to exit first.
    StopOurs,
    /// Leave everything as it is, and say why.
    Refuse(&'static str),
}

/// `/shutdown` would end a recording cleanly, but ending one is the user's
/// call, not an update's. An engine that isn't ours is the user's to stop, and
/// left running it would hold auricle.exe against the installer and greet the
/// relaunch at its old version. A failed probe says nothing about ours: one
/// starved of CPU mid-recording looks just like one that is gone.
pub fn plan_update(probe: &Probe, owned: Owned) -> UpdatePlan {
    match (probe, owned) {
        (Probe::Engine(health), _) if is_busy(health) => {
            UpdatePlan::Refuse(RECORDING_BLOCKS_UPDATE)
        }
        (Probe::Engine(_), Owned::Alive) => UpdatePlan::StopOurs,
        (Probe::Engine(_), _) => UpdatePlan::Refuse(FOREIGN_ENGINE_BLOCKS_UPDATE),
        (_, Owned::Alive) => UpdatePlan::Refuse(SILENT_ENGINE_BLOCKS_UPDATE),
        _ => UpdatePlan::Install,
    }
}

/// Clear the way for an update. True if an engine of ours was stopped, so a
/// failed install knows there is one to bring back.
pub async fn stop_for_update(
    client: &reqwest::Client,
    base_url: &str,
    dir: &Path,
    timeout: Duration,
) -> Result<bool, String> {
    let probe = probe(client, base_url).await;
    match plan_update(&probe, owner(dir, &probe)) {
        UpdatePlan::Install => Ok(false),
        UpdatePlan::StopOurs => stop_engine(client, base_url, dir, timeout)
            .await
            .map(|()| true),
        UpdatePlan::Refuse(why) => Err(why.to_string()),
    }
}

pub const RECORDING_BLOCKS_UPDATE: &str =
    "a recording is running. Stop it, then choose Check for updates again";

pub const FOREIGN_ENGINE_BLOCKS_UPDATE: &str =
    "an Auricle engine this overlay did not start is running, so it was left alone. Stop it \
     yourself with Ctrl+C in its terminal, or End task on auricle.exe in Task Manager's Details \
     tab, then choose Check for updates again";

pub const SILENT_ENGINE_BLOCKS_UPDATE: &str =
    "the engine is running but not answering, and it may be in the middle of a recording. Choose \
     Check for updates again once it responds";

/// Engines from before `/shutdown` existed have no way to be asked.
const ENGINE_TOO_OLD_TO_STOP: &str =
    "this engine is too old to stop on request. Finish any recording, use End task on auricle.exe \
     in Task Manager's Details tab, then try again";

/// What to do about the engine's version once attached.
#[derive(Debug, PartialEq, Eq)]
pub enum VersionCheck {
    Match,
    Restart,
    Warn(String),
}

/// `restartable` means an engine a previous overlay left running, so
/// restarting it swaps in the current binary. Never during a session, and
/// never for an engine spawned by this run: that one came from the binary
/// beside us and a restart would only repeat the mismatch.
pub fn check_version(engine: &str, overlay: &str, restartable: bool, busy: bool) -> VersionCheck {
    if engine == overlay {
        VersionCheck::Match
    } else if restartable && !busy {
        VersionCheck::Restart
    } else if restartable {
        VersionCheck::Warn(format!(
            "The engine is version {engine} but this overlay is {overlay}. It is recording, so \
             it was left running. Stop the recording, then launch Auricle again to update it."
        ))
    } else {
        VersionCheck::Warn(format!(
            "The engine is version {engine} but this overlay is {overlay}, so features may \
             misbehave, and Auricle can't restart this one. Finish any recording, then stop it \
             with Ctrl+C in its terminal, or End task on auricle.exe in Task Manager's Details \
             tab. Then launch Auricle again."
        ))
    }
}

const MAX_RESPAWNS: u32 = 5;
/// An engine must stay up this long before a crash counts as a new problem
/// rather than the same crash loop.
const STABLE_AFTER: Duration = Duration::from_secs(60);
/// A loop slow enough to pass STABLE_AFTER every time is still a loop: past
/// this many crashes inside CRASH_WINDOW, stop restarting.
const MAX_CRASHES: usize = 5;
const CRASH_WINDOW: Duration = Duration::from_secs(60 * 60);
/// The least time between engine notices. Each one takes focus, so one per
/// crash would keep pulling the user out of whatever they are doing.
const NOTICE_GAP: Duration = Duration::from_secs(10 * 60);
/// How long a live engine may stay silent before the user hears about it.
/// Long enough that a busy machine doesn't read as a hang.
const HUNG_AFTER: Duration = Duration::from_secs(120);

/// What to do about a live engine that won't answer. Never a kill from here:
/// a recording it may hold is the user's to give up.
pub const HUNG_MESSAGE: &str =
    "The engine is running but has not answered for 2 minutes, and it may be in the middle of a \
     recording, so Auricle is leaving it alone. If it stays stuck, use End task on auricle.exe in \
     Task Manager's Details tab. Auricle then restarts it, and what was already saved is kept.";

fn backoff(attempts: u32) -> Duration {
    Duration::from_secs(1 << attempts.min(5))
}

/// What the watcher should do about an engine that is not answering.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Wait,
    Spawn,
    /// Something else holds the port; say so once.
    Squatted,
    /// Our engine is alive but stuck; say so once.
    Hung,
    /// Out of retries; say so once.
    GaveUp,
}

/// Respawn policy, kept apart from the loop that acts on it so it can be
/// tested without a real process.
#[derive(Debug)]
pub struct Watch {
    /// An engine should be running: at launch, on a relaunch, or after ours
    /// died. Never set by a foreign engine stopping: a user who stops their
    /// own `auricle serve` is not asking the overlay to take the port.
    wanted: bool,
    attempts: u32,
    retry_at: Option<Instant>,
    healthy_since: Option<Instant>,
    reported: bool,
    /// When our engine died, within CRASH_WINDOW. Unlike `attempts`, staying
    /// up for a while does not clear these.
    crashes: Vec<Instant>,
    last_notice: Option<Instant>,
    silent_since: Option<Instant>,
    hang_reported: bool,
}

impl Default for Watch {
    fn default() -> Self {
        Watch {
            wanted: true,
            attempts: 0,
            retry_at: None,
            healthy_since: None,
            reported: false,
            crashes: Vec::new(),
            last_notice: None,
            silent_since: None,
            hang_reported: false,
        }
    }
}

impl Watch {
    /// The user launched the app, or something stopped the engine on
    /// purpose and wants it back: start over, past any earlier give-up.
    pub fn kick(&mut self) {
        *self = Watch::default();
    }

    pub fn healthy(&mut self, now: Instant) {
        self.wanted = false;
        self.reported = false;
        self.silent_since = None;
        self.hang_reported = false;
        let since = *self.healthy_since.get_or_insert(now);
        if now.duration_since(since) >= STABLE_AFTER {
            self.attempts = 0;
        }
    }

    pub fn unhealthy(&mut self, squatted: bool, owned: Owned, now: Instant) -> Step {
        self.healthy_since = None;
        if owned == Owned::Alive {
            return self.silent(now);
        }
        self.silent_since = None;
        self.hang_reported = false;
        if owned == Owned::Dead {
            self.wanted = true;
        }
        if !self.wanted {
            return Step::Wait;
        }
        if squatted {
            return self.report(Step::Squatted);
        }
        if self.attempts >= MAX_RESPAWNS {
            return self.report(Step::GaveUp);
        }
        if self.retry_at.is_some_and(|at| now < at) {
            return Step::Wait;
        }
        if owned == Owned::Dead && self.crash_limit_passed(now) {
            // Holds until a relaunch, however long ago the crashes were.
            self.attempts = MAX_RESPAWNS;
            return self.report(Step::GaveUp);
        }
        self.attempts += 1;
        self.retry_at = Some(now + backoff(self.attempts));
        Step::Spawn
    }

    /// Booting, or too busy to answer. A second engine could only lose the
    /// port to it, so it gets all the time it needs, but not in silence.
    fn silent(&mut self, now: Instant) -> Step {
        let since = *self.silent_since.get_or_insert(now);
        let hung = now.duration_since(since) >= HUNG_AFTER;
        if hung && !self.hang_reported && self.notice_due(now) {
            self.hang_reported = true;
            return Step::Hung;
        }
        Step::Wait
    }

    fn crash_limit_passed(&mut self, now: Instant) -> bool {
        self.crashes
            .retain(|at| now.duration_since(*at) < CRASH_WINDOW);
        self.crashes.push(now);
        self.crashes.len() > MAX_CRASHES
    }

    /// True for the first failure of a run of attempts: the one worth
    /// telling the user about before the retries play out.
    pub fn first_attempt(&self) -> bool {
        self.attempts == 1
    }

    /// Whether a notice may take focus now. True means it is being sent:
    /// the next has to wait out NOTICE_GAP. A give-up is exempt, being final.
    pub fn notice_due(&mut self, now: Instant) -> bool {
        let due = self
            .last_notice
            .is_none_or(|at| now.duration_since(at) >= NOTICE_GAP);
        if due {
            self.last_notice = Some(now);
        }
        due
    }

    fn report(&mut self, step: Step) -> Step {
        if std::mem::replace(&mut self.reported, true) {
            Step::Wait
        } else {
            step
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    pub(crate) fn temp_dir(name: &str) -> PathBuf {
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

    /// A stand-in for the engine's API, recording every POST it gets.
    pub(crate) struct FakeEngine {
        pub base: String,
        pub posts: Arc<Mutex<Vec<String>>>,
    }

    /// `health` is the /health body and `shutdown_status` the answer to
    /// POST /shutdown; with `exits`, a 202 then closes the port, as a real
    /// exit would. Any other POST gets 200 and an empty object.
    pub(crate) fn fake_engine(
        health: &'static str,
        shutdown_status: u16,
        exits: bool,
    ) -> FakeEngine {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let posts = Arc::new(Mutex::new(Vec::new()));
        let seen = posts.clone();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]);
                let line = head.lines().next().unwrap_or_default().to_string();
                let shutdown = line.starts_with("POST /api/v1/shutdown");
                let (status, body) = if shutdown {
                    (shutdown_status, r#"{"status":"shutting_down"}"#)
                } else if line.starts_with("POST") {
                    (200, "{}")
                } else {
                    (200, health)
                };
                if line.starts_with("POST") {
                    seen.lock().unwrap().push(line);
                }
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len(),
                );
                let _ = s.write_all(response.as_bytes());
                if shutdown && status == 202 && exits {
                    return;
                }
            }
        });
        FakeEngine { base, posts }
    }

    pub(crate) const IDLE: &str =
        r#"{"active_session":null,"state":"idle","status":"ok","version":"0.3.0"}"#;
    pub(crate) const RECORDING: &str =
        r#"{"active_session":"s1","state":"recording","status":"ok","version":"0.3.0"}"#;

    /// The pid in the `_OURS` bodies. No process has this number, so tests
    /// on those can only pass by comparing it, never by finding a process.
    pub(crate) const OUR_PID: u32 = u32::MAX;
    pub(crate) const IDLE_OURS: &str = r#"{"active_session":null,"pid":4294967295,"state":"idle","status":"ok","version":"0.3.0"}"#;
    pub(crate) const RECORDING_OURS: &str = r#"{"active_session":"s1","pid":4294967295,"state":"recording","status":"ok","version":"0.3.0"}"#;

    pub(crate) fn record_pid(dir: &Path, pid: u32) {
        std::fs::write(pid_path(dir), pid.to_string()).unwrap();
    }

    /// A port nothing listens on: the listener is gone when this returns.
    pub(crate) fn closed_port() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    }

    #[tokio::test]
    async fn attach_when_auricle_health_answers_spawns_nothing() {
        // The real health shape (API.md): status + version + state.
        let addr = serve_body(IDLE);
        let client = reqwest::Client::new();
        // A nonexistent exe path proves nothing is spawned on this path.
        let exe = EngineExe::EnvOverride(PathBuf::from("Z:/does/not/exist.exe"));
        let dir = temp_dir("attach");
        let result = attach_or_spawn(&client, &format!("http://{addr}"), &exe, &dir).await;
        assert_eq!(result, Ok(Attach::Attached));
    }

    #[tokio::test]
    async fn a_port_squatter_is_not_attached_to_or_fought() {
        // 200 OK from something that is not Auricle: never attach, and no
        // spawn either, since it could only lose on the port.
        let addr = serve_body(r#"{"hello":"world"}"#);
        let client = reqwest::Client::new();
        let exe = EngineExe::EnvOverride(PathBuf::from("Z:/does/not/exist.exe"));
        let dir = temp_dir("squatter");
        let err = attach_or_spawn(&client, &format!("http://{addr}"), &exe, &dir)
            .await
            .unwrap_err();
        assert!(err.contains("not Auricle"), "{err}");
    }

    #[tokio::test]
    async fn spawn_failure_is_a_clean_error() {
        let client = reqwest::Client::new();
        let exe = EngineExe::EnvOverride(PathBuf::from("Z:/does/not/exist.exe"));
        let dir = temp_dir("spawnfail");
        // Nothing listens on this port, so it tries to spawn and fails.
        let err = attach_or_spawn(&client, "http://127.0.0.1:9", &exe, &dir)
            .await
            .unwrap_err();
        assert!(err.contains("could not start the engine"), "{err}");
        assert_eq!(read_pid(&dir), None);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_child_that_dies_at_startup_reports_why_at_once() {
        let dir = temp_dir("died");
        let script = dir.join("fake-engine.cmd");
        std::fs::write(
            &script,
            "@echo off\r\necho auricle booting 1>&2\r\necho error: address already in use 1>&2\r\nexit /b 3\r\n",
        )
        .unwrap();
        let client = reqwest::Client::new();
        let started = Instant::now();
        let exe = EngineExe::EnvOverride(script);
        let err = attach_or_spawn(&client, &closed_port(), &exe, &dir)
            .await
            .unwrap_err();
        assert!(err.contains("address already in use"), "{err}");
        assert!(err.contains("exit code: 3"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "no 20 s wait for a child that is already dead"
        );
        assert_eq!(read_pid(&dir), None, "a dead child is not recorded as ours");
    }

    #[test]
    fn each_spawn_gets_a_fresh_log_and_keeps_the_last_one() {
        let dir = temp_dir("rotate");
        std::fs::write(log_path(&dir), "the crash\n").unwrap();
        drop(open_log(&dir).unwrap());
        assert_eq!(std::fs::read_to_string(log_path(&dir)).unwrap(), "");
        let old = dir.join("engine.log.old");
        assert_eq!(std::fs::read_to_string(old).unwrap(), "the crash\n");
    }

    #[test]
    fn log_tail_is_the_last_lines_and_stays_toast_sized() {
        let dir = temp_dir("tail");
        assert_eq!(log_tail(&dir), "", "no log yet");
        std::fs::write(log_path(&dir), "a\nb\n\nc\nd\n").unwrap();
        assert_eq!(log_tail(&dir), "b | c | d");
        std::fs::write(log_path(&dir), "é".repeat(1000)).unwrap();
        assert_eq!(log_tail(&dir).chars().count(), 300);
    }

    /// A live process named auricle.exe, which is all `is_engine_process`
    /// looks at.
    #[cfg(windows)]
    fn engine_process(dir: &Path) -> std::process::Child {
        let system = std::env::var_os("SystemRoot").unwrap();
        let fake = dir.join("auricle.exe");
        std::fs::copy(PathBuf::from(system).join("System32/cmd.exe"), &fake).unwrap();
        Command::new(&fake)
            .args(["/c", "ping -n 30 127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn only_a_live_auricle_exe_counts_as_the_engine() {
        // The test binary itself is alive but is not the engine.
        assert!(!is_engine_process(std::process::id()));

        let dir = temp_dir("image");
        let mut child = engine_process(&dir);
        let pid = child.id();
        assert!(is_engine_process(pid));

        std::fs::write(pid_path(&dir), pid.to_string()).unwrap();
        assert_eq!(owned(&dir), Owned::Alive);

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!is_engine_process(pid));
        assert_eq!(owned(&dir), Owned::Dead);

        clear_pid(&dir);
        assert_eq!(owned(&dir), Owned::No);
    }

    fn answered(body: &str) -> Probe {
        Probe::Engine(serde_json::from_str(body).unwrap())
    }

    #[test]
    fn the_pid_in_health_settles_who_owns_the_engine() {
        let dir = temp_dir("owner-pid");
        assert_eq!(owner(&dir, &answered(IDLE_OURS)), Owned::No, "no record");

        record_pid(&dir, OUR_PID);
        assert_eq!(owner(&dir, &answered(IDLE_OURS)), Owned::Alive);

        // Whatever the record names, it isn't what is answering.
        record_pid(&dir, 7);
        assert_eq!(owner(&dir, &answered(IDLE_OURS)), Owned::Dead);
    }

    #[test]
    fn an_engine_that_reports_no_pid_gets_the_process_check() {
        let dir = temp_dir("owner-old");
        record_pid(&dir, OUR_PID);
        assert_eq!(owner(&dir, &answered(IDLE)), Owned::Dead);
        let odd = r#"{"pid":"not a number","state":"idle","status":"ok","version":"0.3.0"}"#;
        assert_eq!(owner(&dir, &answered(odd)), Owned::Dead);
        assert_eq!(owner(&dir, &Probe::Down), Owned::Dead);
    }

    #[cfg(windows)]
    #[test]
    fn the_process_check_still_finds_our_engine_when_it_reports_no_pid() {
        let dir = temp_dir("owner-live");
        let mut child = engine_process(&dir);
        record_pid(&dir, child.id());
        let found = (owner(&dir, &answered(IDLE)), owner(&dir, &Probe::Down));
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(found, (Owned::Alive, Owned::Alive));
    }

    #[test]
    fn quit_never_stops_or_questions_a_recording_that_is_not_ours() {
        let mut guard = QuitGuard::default();
        let now = Instant::now();
        for owned in [Owned::No, Owned::Dead] {
            assert_eq!(guard.press(owned, true, now), QuitStep::Exit);
            assert_eq!(guard.press(owned, true, now), QuitStep::Exit);
        }
    }

    #[test]
    fn quit_needs_no_confirm_when_ours_is_not_recording() {
        let mut guard = QuitGuard::default();
        let step = guard.press(Owned::Alive, false, Instant::now());
        assert_eq!(step, QuitStep::Exit);
    }

    #[test]
    fn a_second_quit_inside_the_window_stops_our_recording() {
        let mut guard = QuitGuard::default();
        let now = Instant::now();
        assert_eq!(guard.press(Owned::Alive, true, now), QuitStep::Warn);
        let second = now + QUIT_CONFIRM_WINDOW;
        assert_eq!(
            guard.press(Owned::Alive, true, second),
            QuitStep::StopThenExit
        );
    }

    #[test]
    fn a_second_quit_after_the_window_only_warns_again() {
        let mut guard = QuitGuard::default();
        let now = Instant::now();
        assert_eq!(guard.press(Owned::Alive, true, now), QuitStep::Warn);
        let late = now + QUIT_CONFIRM_WINDOW + Duration::from_secs(1);
        assert_eq!(guard.press(Owned::Alive, true, late), QuitStep::Warn);
        let again = late + Duration::from_secs(1);
        assert_eq!(
            guard.press(Owned::Alive, true, again),
            QuitStep::StopThenExit
        );
    }

    #[test]
    fn a_failed_stop_needs_a_fresh_warning() {
        let mut guard = QuitGuard::default();
        let now = Instant::now();
        guard.press(Owned::Alive, true, now);
        guard.press(Owned::Alive, true, now);
        assert_eq!(guard.press(Owned::Alive, true, now), QuitStep::Warn);
    }

    #[test]
    fn a_warning_does_not_carry_over_to_a_later_recording() {
        let mut guard = QuitGuard::default();
        let now = Instant::now();
        assert_eq!(guard.press(Owned::Alive, true, now), QuitStep::Warn);
        // It ended, the user stayed, and another began inside the window.
        assert_eq!(guard.press(Owned::Alive, false, now), QuitStep::Exit);
        assert_eq!(guard.press(Owned::Alive, true, now), QuitStep::Warn);
    }

    #[tokio::test]
    async fn probe_tells_the_engine_from_a_squatter_from_nothing() {
        let client = reqwest::Client::new();
        let engine = serve_body(IDLE);
        assert!(matches!(
            probe(&client, &format!("http://{engine}")).await,
            Probe::Engine(_)
        ));
        let squatter = serve_body("<html>not json</html>");
        assert_eq!(
            probe(&client, &format!("http://{squatter}")).await,
            Probe::Squatter
        );
        assert_eq!(probe(&client, &closed_port()).await, Probe::Down);
    }

    #[test]
    fn the_tray_tooltip_says_when_a_recording_runs() {
        let health = |state: &str| Probe::Engine(serde_json::json!({ "state": state }));
        assert_eq!(tray_tooltip(&health("idle")), "Auricle Copilot");
        assert_eq!(
            tray_tooltip(&health("recording")),
            "Auricle Copilot: recording"
        );
        assert_eq!(
            tray_tooltip(&health("stopping")),
            "Auricle Copilot: finishing a recording"
        );
        assert_eq!(
            tray_tooltip(&Probe::Down),
            "Auricle Copilot: engine offline"
        );
        assert_eq!(
            tray_tooltip(&Probe::Squatter),
            "Auricle Copilot: engine offline"
        );
    }

    #[tokio::test]
    async fn an_update_is_refused_while_recording_and_the_engine_is_left_alone() {
        let engine = fake_engine(RECORDING, 202, true);
        let client = reqwest::Client::new();
        let dir = temp_dir("upd-rec");
        let err = stop_for_update(&client, &engine.base, &dir, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(err, RECORDING_BLOCKS_UPDATE);
        assert!(engine.posts.lock().unwrap().is_empty(), "no shutdown sent");
    }

    #[tokio::test]
    async fn an_update_stops_our_idle_engine_and_waits_for_it_to_exit() {
        let engine = fake_engine(IDLE_OURS, 202, true);
        let client = reqwest::Client::new();
        let dir = temp_dir("upd-idle");
        record_pid(&dir, OUR_PID);
        let stopped = stop_for_update(&client, &engine.base, &dir, Duration::from_secs(5)).await;
        assert_eq!(stopped, Ok(true), "a failed install has one to bring back");
        let posts = engine.posts.lock().unwrap().clone();
        assert!(posts[0].starts_with("POST /api/v1/shutdown"), "{posts:?}");
        assert_eq!(
            read_pid(&dir),
            None,
            "a clean stop is not a crash to recover"
        );
        assert_eq!(probe(&client, &engine.base).await, Probe::Down);
    }

    #[tokio::test]
    async fn an_update_never_stops_an_engine_it_does_not_own() {
        let engine = fake_engine(IDLE, 202, true);
        let client = reqwest::Client::new();
        let dir = temp_dir("upd-foreign");
        let err = stop_for_update(&client, &engine.base, &dir, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.contains("Task Manager"), "{err}");
        assert!(engine.posts.lock().unwrap().is_empty(), "no shutdown sent");
    }

    #[tokio::test]
    async fn an_update_is_not_fooled_by_a_record_of_some_other_engine() {
        let engine = fake_engine(IDLE_OURS, 202, true);
        let client = reqwest::Client::new();
        let dir = temp_dir("upd-stale");
        record_pid(&dir, 7);
        let stopped = stop_for_update(&client, &engine.base, &dir, Duration::from_secs(1)).await;
        assert_eq!(stopped, Err(FOREIGN_ENGINE_BLOCKS_UPDATE.to_string()));
        assert!(engine.posts.lock().unwrap().is_empty(), "no shutdown sent");
    }

    #[test]
    fn only_a_clear_port_or_our_own_idle_engine_lets_an_update_through() {
        use UpdatePlan::{Install, Refuse, StopOurs};
        let idle = || answered(IDLE);
        let recording = || answered(RECORDING);
        let cases = [
            (idle(), Owned::Alive, StopOurs),
            (idle(), Owned::No, Refuse(FOREIGN_ENGINE_BLOCKS_UPDATE)),
            (idle(), Owned::Dead, Refuse(FOREIGN_ENGINE_BLOCKS_UPDATE)),
            (recording(), Owned::Alive, Refuse(RECORDING_BLOCKS_UPDATE)),
            (recording(), Owned::No, Refuse(RECORDING_BLOCKS_UPDATE)),
            (
                Probe::Down,
                Owned::Alive,
                Refuse(SILENT_ENGINE_BLOCKS_UPDATE),
            ),
            (
                Probe::Squatter,
                Owned::Alive,
                Refuse(SILENT_ENGINE_BLOCKS_UPDATE),
            ),
            (Probe::Down, Owned::No, Install),
            (Probe::Down, Owned::Dead, Install),
            (Probe::Squatter, Owned::No, Install),
        ];
        for (probe, owned, plan) in cases {
            assert_eq!(plan_update(&probe, owned), plan, "{probe:?} {owned:?}");
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn an_update_is_refused_while_our_engine_is_alive_but_silent() {
        let dir = temp_dir("upd-silent");
        let mut child = engine_process(&dir);
        record_pid(&dir, child.id());
        let client = reqwest::Client::new();

        let stopped = stop_for_update(&client, &closed_port(), &dir, Duration::from_secs(1)).await;

        child.kill().unwrap();
        child.wait().unwrap();
        let err = stopped.unwrap_err();
        assert!(err.contains("not answering"), "{err}");
    }

    #[tokio::test]
    async fn an_update_needs_nothing_stopped_when_no_engine_runs() {
        let client = reqwest::Client::new();
        let dir = temp_dir("upd-down");
        let stopped = stop_for_update(&client, &closed_port(), &dir, Duration::from_secs(1)).await;
        assert_eq!(stopped, Ok(false), "nothing to bring back after a failure");
    }

    #[tokio::test]
    async fn an_engine_without_the_shutdown_route_is_a_clear_failure() {
        let engine = fake_engine(IDLE, 404, false);
        let client = reqwest::Client::new();
        let dir = temp_dir("upd-404");
        let err = stop_engine(&client, &engine.base, &dir, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.contains("Task Manager"), "{err}");
        assert!(err.contains("auricle.exe"), "{err}");
    }

    #[tokio::test]
    async fn an_engine_that_will_not_exit_times_out() {
        let engine = fake_engine(IDLE, 202, false);
        let client = reqwest::Client::new();
        let dir = temp_dir("upd-stuck");
        let err = stop_engine(&client, &engine.base, &dir, Duration::from_millis(600))
            .await
            .unwrap_err();
        assert!(err.contains("did not exit"), "{err}");
    }

    #[test]
    fn matching_versions_need_nothing() {
        assert_eq!(
            check_version("0.4.1", "0.4.1", true, true),
            VersionCheck::Match
        );
    }

    #[test]
    fn a_stale_engine_we_left_running_is_restarted_when_idle() {
        assert_eq!(
            check_version("0.4.0", "0.4.1", true, false),
            VersionCheck::Restart
        );
    }

    #[test]
    fn a_recording_stale_engine_is_left_running_and_explained() {
        let VersionCheck::Warn(message) = check_version("0.4.0", "0.4.1", true, true) else {
            panic!("a recording is never interrupted");
        };
        assert!(
            message.contains("0.4.0") && message.contains("0.4.1"),
            "{message}"
        );
        assert!(message.contains("recording"), "{message}");
    }

    #[test]
    fn someone_elses_stale_engine_is_only_explained() {
        let VersionCheck::Warn(message) = check_version("0.4.0", "0.4.1", false, false) else {
            panic!("a foreign engine is not ours to restart");
        };
        assert!(
            message.contains("0.4.0") && message.contains("0.4.1"),
            "{message}"
        );
        // An engine from before pid records has no other way out.
        assert!(message.contains("Task Manager"), "{message}");
        assert!(message.contains("auricle.exe"), "{message}");
        assert!(message.contains("launch Auricle again"), "{message}");
    }

    /// One unhealthy probe a second, `ticks` times, from an engine that had
    /// been running.
    fn ticks(watch: &mut Watch, owned: Owned, start: Instant, ticks: u64) -> Vec<Step> {
        (0..ticks)
            .map(|t| watch.unhealthy(false, owned, start + Duration::from_secs(t)))
            .collect()
    }

    fn running() -> Watch {
        let mut watch = Watch::default();
        watch.healthy(Instant::now());
        watch
    }

    #[test]
    fn the_first_launch_spawns_right_away() {
        let mut watch = Watch::default();
        let now = Instant::now();
        assert_eq!(watch.unhealthy(false, Owned::No, now), Step::Spawn);
        assert!(watch.first_attempt());
    }

    #[test]
    fn a_dead_engine_of_ours_is_respawned_with_growing_delays() {
        let mut watch = running();
        let start = Instant::now();
        // Dead on the first tick; respawning clears the record after that.
        let spawns: Vec<u64> = (0..120)
            .filter(|t| {
                let owned = if *t == 0 { Owned::Dead } else { Owned::No };
                watch.unhealthy(false, owned, start + Duration::from_secs(*t)) == Step::Spawn
            })
            .collect();
        // Waits of 2, 4, 8, then 16 s.
        assert_eq!(spawns, vec![0, 2, 6, 14, 30]);
    }

    #[test]
    fn retries_end_in_one_give_up_until_a_relaunch() {
        let mut watch = running();
        let start = Instant::now();
        let mut steps = ticks(&mut watch, Owned::Dead, start, 200);
        steps.retain(|s| *s != Step::Wait);
        assert_eq!(steps.iter().filter(|s| **s == Step::Spawn).count(), 5);
        assert_eq!(steps.last(), Some(&Step::GaveUp));
        assert_eq!(steps.iter().filter(|s| **s == Step::GaveUp).count(), 1);

        watch.kick();
        let later = start + Duration::from_secs(201);
        assert_eq!(watch.unhealthy(false, Owned::No, later), Step::Spawn);
    }

    #[test]
    fn an_engine_that_is_alive_but_silent_is_never_doubled() {
        let mut watch = Watch::default();
        let all = ticks(&mut watch, Owned::Alive, Instant::now(), 300);
        assert!(!all.contains(&Step::Spawn));
    }

    #[test]
    fn a_hung_engine_is_reported_once_after_a_long_silence() {
        let mut watch = running();
        let all = ticks(&mut watch, Owned::Alive, Instant::now(), 1000);
        let hung: Vec<usize> = (0..all.len()).filter(|t| all[*t] == Step::Hung).collect();
        assert_eq!(hung, vec![HUNG_AFTER.as_secs() as usize]);
    }

    #[test]
    fn a_busy_spell_that_ends_is_not_a_hang() {
        let mut watch = running();
        let mut now = Instant::now();
        for _ in 0..10 {
            let all = ticks(&mut watch, Owned::Alive, now, HUNG_AFTER.as_secs() - 1);
            assert!(all.iter().all(|s| *s == Step::Wait));
            now += HUNG_AFTER;
            watch.healthy(now);
        }
    }

    #[test]
    fn hangs_that_keep_clearing_cannot_keep_taking_focus() {
        let mut watch = running();
        let mut now = Instant::now();
        let mut reports = 0;
        for _ in 0..4 {
            let spell = ticks(&mut watch, Owned::Alive, now, HUNG_AFTER.as_secs() + 1);
            reports += spell.iter().filter(|s| **s == Step::Hung).count();
            now += HUNG_AFTER + Duration::from_secs(1);
            watch.healthy(now);
        }
        assert_eq!(reports, 1);
    }

    #[test]
    fn a_hang_held_back_by_a_recent_notice_is_still_reported() {
        let mut watch = running();
        let now = Instant::now();
        assert!(watch.notice_due(now));
        let all = ticks(&mut watch, Owned::Alive, now, NOTICE_GAP.as_secs() + 1);
        let hung: Vec<usize> = (0..all.len()).filter(|t| all[*t] == Step::Hung).collect();
        assert_eq!(hung, vec![NOTICE_GAP.as_secs() as usize]);
    }

    #[test]
    fn an_engine_that_dies_after_a_hang_is_recovered_and_can_still_give_up() {
        let mut watch = running();
        let start = Instant::now();
        ticks(&mut watch, Owned::Alive, start, HUNG_AFTER.as_secs() + 1);
        let later = start + Duration::from_secs(1000);
        let mut steps = ticks(&mut watch, Owned::Dead, later, 200);
        steps.retain(|s| *s != Step::Wait);
        assert_eq!(steps.iter().filter(|s| **s == Step::Spawn).count(), 5);
        assert_eq!(steps.last(), Some(&Step::GaveUp));
    }

    #[test]
    fn a_respawned_engine_gets_the_full_grace_before_it_counts_as_hung() {
        let mut watch = running();
        let start = Instant::now();
        ticks(&mut watch, Owned::Alive, start, 100);
        let died = start + Duration::from_secs(100);
        assert_eq!(watch.unhealthy(false, Owned::Dead, died), Step::Spawn);
        let booting = ticks(&mut watch, Owned::Alive, died, HUNG_AFTER.as_secs() - 1);
        assert!(booting.iter().all(|s| *s == Step::Wait));
    }

    #[test]
    fn crash_notices_stay_apart_however_fast_the_engine_dies() {
        let mut watch = running();
        let now = Instant::now();
        assert!(watch.notice_due(now));
        assert!(!watch.notice_due(now + Duration::from_secs(1)));
        assert!(!watch.notice_due(now + NOTICE_GAP - Duration::from_secs(1)));
        assert!(watch.notice_due(now + NOTICE_GAP));
    }

    #[test]
    fn a_slow_crash_loop_interrupts_the_user_twice_at_most() {
        // One notice for the first crash, one for giving up: `report` holds
        // the give-up to one, and the rest wait on `notice_due`.
        let mut watch = running();
        let mut now = Instant::now();
        let mut notices = 0;
        loop {
            match watch.unhealthy(false, Owned::Dead, now) {
                Step::Spawn => notices += usize::from(watch.notice_due(now)),
                Step::GaveUp => {
                    notices += 1;
                    break;
                }
                other => panic!("{other:?}"),
            }
            now += Duration::from_secs(5);
            watch.healthy(now);
            now += STABLE_AFTER + Duration::from_secs(5);
            watch.healthy(now);
        }
        assert_eq!(notices, 2);
    }

    #[test]
    fn crashes_spread_far_apart_are_not_a_loop() {
        let mut watch = running();
        let mut now = Instant::now();
        for _ in 0..20 {
            assert_eq!(watch.unhealthy(false, Owned::Dead, now), Step::Spawn);
            now += CRASH_WINDOW / 4;
            watch.healthy(now);
            now += STABLE_AFTER;
            watch.healthy(now);
        }
    }

    #[test]
    fn a_relaunch_forgives_a_crash_loop_that_gave_up() {
        let mut watch = running();
        let mut now = Instant::now();
        while watch.unhealthy(false, Owned::Dead, now) == Step::Spawn {
            now += STABLE_AFTER * 2;
            watch.healthy(now);
            now += STABLE_AFTER * 2;
            watch.healthy(now);
        }
        watch.kick();
        assert_eq!(watch.unhealthy(false, Owned::No, now), Step::Spawn);
    }

    #[test]
    fn a_give_up_holds_even_once_the_crashes_are_old() {
        let mut watch = running();
        let mut now = Instant::now();
        while watch.unhealthy(false, Owned::Dead, now) == Step::Spawn {
            now += STABLE_AFTER * 2;
            watch.healthy(now);
            now += STABLE_AFTER * 2;
            watch.healthy(now);
        }
        now += CRASH_WINDOW * 2;
        assert_eq!(watch.unhealthy(false, Owned::No, now), Step::Wait);
    }

    #[test]
    fn a_stopped_foreign_engine_is_not_replaced() {
        let mut watch = running();
        let all = ticks(&mut watch, Owned::No, Instant::now(), 300);
        assert!(all.iter().all(|s| *s == Step::Wait));
    }

    #[test]
    fn a_relaunch_replaces_it_after_all() {
        let mut watch = running();
        watch.kick();
        let now = Instant::now();
        assert_eq!(watch.unhealthy(false, Owned::No, now), Step::Spawn);
    }

    #[test]
    fn a_squatter_is_reported_once_and_never_spawned_against() {
        let mut watch = Watch::default();
        let now = Instant::now();
        assert_eq!(watch.unhealthy(true, Owned::No, now), Step::Squatted);
        for t in 1..100 {
            let at = now + Duration::from_secs(t);
            assert_eq!(watch.unhealthy(true, Owned::No, at), Step::Wait);
        }
        // Once it leaves, the engine that was wanted can finally start.
        let gone = now + Duration::from_secs(100);
        assert_eq!(watch.unhealthy(false, Owned::No, gone), Step::Spawn);
    }

    #[test]
    fn a_crash_loop_keeps_counting_through_brief_recoveries() {
        let mut watch = running();
        let mut now = Instant::now();
        for _ in 0..5 {
            assert_eq!(watch.unhealthy(false, Owned::Dead, now), Step::Spawn);
            now += Duration::from_secs(40);
            watch.healthy(now);
            now += Duration::from_secs(10);
        }
        assert_eq!(watch.unhealthy(false, Owned::Dead, now), Step::GaveUp);
    }

    #[test]
    fn a_crash_loop_of_long_lives_still_gives_up() {
        // Every life outlasts STABLE_AFTER, so the retry count clears each time.
        let mut watch = running();
        let mut now = Instant::now();
        let mut spawns = 0;
        let last = loop {
            let step = watch.unhealthy(false, Owned::Dead, now);
            if step != Step::Spawn || spawns == 50 {
                break step;
            }
            spawns += 1;
            now += Duration::from_secs(5);
            watch.healthy(now);
            now += STABLE_AFTER + Duration::from_secs(5);
            watch.healthy(now);
        };
        assert_eq!(last, Step::GaveUp);
        assert_eq!(spawns, 5);
        for _ in 0..100 {
            now += Duration::from_secs(60);
            assert_eq!(watch.unhealthy(false, Owned::Dead, now), Step::Wait);
        }
    }

    #[test]
    fn staying_up_clears_the_crash_count() {
        let mut watch = running();
        let now = Instant::now();
        assert_eq!(watch.unhealthy(false, Owned::Dead, now), Step::Spawn);
        let up = now + Duration::from_secs(5);
        watch.healthy(up);
        watch.healthy(up + STABLE_AFTER);
        let crash = up + STABLE_AFTER + Duration::from_secs(1);
        assert_eq!(watch.unhealthy(false, Owned::Dead, crash), Step::Spawn);
        assert!(watch.first_attempt());
    }
}
