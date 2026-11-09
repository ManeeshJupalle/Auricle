//! Session lifecycle state machine: idle → recording → stopping → idle.
//! One active session max; a concurrent start is a conflict (HTTP 409).
//!
//! This is the one record of whether a session exists and how to stop it: the
//! slot and its stop controls are reserved, signalled, and released together.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use auricle_capture::CaptureHandle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording(String),
    Stopping(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum LifecycleError {
    /// Another session is active (or still stopping) — maps to 409.
    Busy(String),
    /// The named session is not the active one — maps to 404/409.
    NotActive,
    /// The daemon is exiting and takes no more sessions — maps to 503.
    ShuttingDown,
}

/// How to stop the session in the slot. It exists from the moment the slot is
/// reserved, not from when capture opens, so a stop that lands while a start
/// is still loading its model has something to act on.
#[derive(Clone, Default)]
pub struct SessionControl {
    pub stop: Arc<AtomicBool>,
    pub handles: Arc<Mutex<Vec<CaptureHandle>>>,
}

pub struct Lifecycle {
    state: State,
    control: SessionControl,
    /// Past the last point a stop can prevent capture. Kept here because that
    /// is decided under this lock, so a stop and a start can't each miss the
    /// other: exactly one of them sees it.
    capturing: bool,
    closed: bool,
}

impl Lifecycle {
    pub fn new() -> Self {
        Lifecycle {
            state: State::Idle,
            control: SessionControl::default(),
            capturing: false,
            closed: false,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    /// The current session's stop controls.
    pub fn control(&self) -> SessionControl {
        self.control.clone()
    }

    pub fn active_session(&self) -> Option<&str> {
        match &self.state {
            State::Idle => None,
            State::Recording(id) | State::Stopping(id) => Some(id),
        }
    }

    /// Refuse every start from now on. A session already in the slot carries on.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// idle → recording; anything else is a conflict.
    pub fn start(&mut self, id: &str) -> Result<(), LifecycleError> {
        if self.closed {
            return Err(LifecycleError::ShuttingDown);
        }
        match &self.state {
            State::Idle => {
                self.state = State::Recording(id.to_string());
                self.control = SessionControl::default();
                self.capturing = false;
                Ok(())
            }
            State::Recording(active) | State::Stopping(active) => {
                Err(LifecycleError::Busy(active.clone()))
            }
        }
    }

    /// Claim the go-ahead to open capture: false if a stop got here first.
    pub fn begin_capture(&mut self, id: &str) -> bool {
        self.capturing = matches!(&self.state, State::Recording(active) if active == id);
        self.capturing
    }

    /// Whether `id` holds the slot and has gone ahead with capture.
    pub fn capture_began(&self, id: &str) -> bool {
        self.capturing && self.active_session() == Some(id)
    }

    /// recording → stopping (for the named session).
    pub fn begin_stop(&mut self, id: &str) -> Result<(), LifecycleError> {
        match &self.state {
            State::Recording(active) if active == id => {
                self.state = State::Stopping(id.to_string());
                self.control.stop.store(true, Ordering::Relaxed);
                Ok(())
            }
            State::Stopping(active) if active == id => Ok(()), // already stopping
            _ => Err(LifecycleError::NotActive),
        }
    }

    /// → idle: the session is fully stopped and persisted, or its start
    /// failed. Either way nothing of it may keep running.
    pub fn finish_stop(&mut self, id: &str) {
        if matches!(&self.state, State::Recording(a) | State::Stopping(a) if a == id) {
            self.control.stop.store(true, Ordering::Relaxed);
            self.state = State::Idle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_happy_path() {
        let mut lc = Lifecycle::new();
        assert_eq!(lc.active_session(), None);
        lc.start("a").unwrap();
        assert_eq!(lc.state(), &State::Recording("a".into()));
        lc.begin_stop("a").unwrap();
        assert_eq!(lc.state(), &State::Stopping("a".into()));
        lc.finish_stop("a");
        assert_eq!(lc.state(), &State::Idle);
    }

    #[test]
    fn concurrent_start_is_busy() {
        let mut lc = Lifecycle::new();
        lc.start("a").unwrap();
        assert_eq!(lc.start("b"), Err(LifecycleError::Busy("a".into())));
        // Still busy while stopping.
        lc.begin_stop("a").unwrap();
        assert_eq!(lc.start("b"), Err(LifecycleError::Busy("a".into())));
        // Free after the stop completes.
        lc.finish_stop("a");
        lc.start("b").unwrap();
    }

    #[test]
    fn stop_of_inactive_session_is_rejected() {
        let mut lc = Lifecycle::new();
        assert_eq!(lc.begin_stop("a"), Err(LifecycleError::NotActive));
        lc.start("a").unwrap();
        assert_eq!(lc.begin_stop("b"), Err(LifecycleError::NotActive));
        // Idempotent stop of the right session is fine.
        lc.begin_stop("a").unwrap();
        lc.begin_stop("a").unwrap();
    }

    #[test]
    fn finish_stop_of_wrong_session_is_ignored() {
        let mut lc = Lifecycle::new();
        lc.start("a").unwrap();
        lc.finish_stop("b");
        assert_eq!(lc.state(), &State::Recording("a".into()));
    }

    #[test]
    fn a_closed_lifecycle_takes_no_start_and_keeps_what_it_has() {
        let mut lc = Lifecycle::new();
        lc.start("a").unwrap();
        lc.close();
        assert_eq!(lc.start("b"), Err(LifecycleError::ShuttingDown));
        assert_eq!(lc.active_session(), Some("a"));
        lc.begin_stop("a").unwrap();
        lc.finish_stop("a");
        assert_eq!(lc.start("c"), Err(LifecycleError::ShuttingDown));
        assert_eq!(lc.state(), &State::Idle);
    }

    #[test]
    fn a_stop_and_a_start_opening_capture_see_each_other() {
        // The stop first: capture must not open, and nothing is left to await.
        let mut lc = Lifecycle::new();
        lc.start("a").unwrap();
        lc.begin_stop("a").unwrap();
        assert!(!lc.begin_capture("a"));
        assert!(!lc.capture_began("a"));

        // Capture first: the stop has a session to wait for.
        let mut lc = Lifecycle::new();
        lc.start("a").unwrap();
        assert!(lc.begin_capture("a"));
        lc.begin_stop("a").unwrap();
        assert!(lc.capture_began("a"));

        // Over and done with, and no carry-over to the next session.
        lc.finish_stop("a");
        assert!(!lc.capture_began("a"));
        lc.start("b").unwrap();
        assert!(!lc.capture_began("b"));
    }

    #[test]
    fn stop_reaches_controls_taken_at_start() {
        let mut lc = Lifecycle::new();
        lc.start("a").unwrap();
        let control = lc.control();
        assert!(!control.stop.load(Ordering::Relaxed));
        lc.begin_stop("a").unwrap();
        assert!(control.stop.load(Ordering::Relaxed));
    }

    #[test]
    fn a_failed_stop_signals_nothing() {
        let mut lc = Lifecycle::new();
        lc.start("a").unwrap();
        let control = lc.control();
        lc.begin_stop("b").unwrap_err();
        lc.finish_stop("b");
        assert!(!control.stop.load(Ordering::Relaxed));
    }

    #[test]
    fn releasing_the_slot_stops_its_session_and_the_next_starts_clean() {
        let mut lc = Lifecycle::new();
        lc.start("a").unwrap();
        let first = lc.control();
        // A failed start goes straight from recording to idle.
        lc.finish_stop("a");
        assert!(first.stop.load(Ordering::Relaxed));
        lc.start("b").unwrap();
        assert!(!lc.control().stop.load(Ordering::Relaxed));
    }
}
