//! Session lifecycle state machine: idle → recording → stopping → idle.
//! One active session max; a concurrent start is a conflict (HTTP 409).

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
}

#[derive(Debug)]
pub struct Lifecycle {
    state: State,
}

impl Lifecycle {
    pub fn new() -> Self {
        Lifecycle { state: State::Idle }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn active_session(&self) -> Option<&str> {
        match &self.state {
            State::Idle => None,
            State::Recording(id) | State::Stopping(id) => Some(id),
        }
    }

    /// idle → recording; anything else is a conflict.
    pub fn start(&mut self, id: &str) -> Result<(), LifecycleError> {
        match &self.state {
            State::Idle => {
                self.state = State::Recording(id.to_string());
                Ok(())
            }
            State::Recording(active) | State::Stopping(active) => {
                Err(LifecycleError::Busy(active.clone()))
            }
        }
    }

    /// recording → stopping (for the named session).
    pub fn begin_stop(&mut self, id: &str) -> Result<(), LifecycleError> {
        match &self.state {
            State::Recording(active) if active == id => {
                self.state = State::Stopping(id.to_string());
                Ok(())
            }
            State::Stopping(active) if active == id => Ok(()), // already stopping
            _ => Err(LifecycleError::NotActive),
        }
    }

    /// stopping → idle (the session is now fully stopped/persisted).
    pub fn finish_stop(&mut self, id: &str) {
        if matches!(&self.state, State::Recording(a) | State::Stopping(a) if a == id) {
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
}
