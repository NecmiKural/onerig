//! The two ways work is asked to stop: the owner quitting (a signal; never
//! undone) and a stop of the current action, which is cleared when the next
//! action starts so that it does not end that one too.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Cheap to clone; clones share both flags.
#[derive(Debug, Clone, Default)]
pub struct Halt {
    quit: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Halt {
    pub fn new() -> Halt {
        Halt::default()
    }

    /// The flag signal handlers set. Nothing clears it.
    pub fn quit_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.quit)
    }

    /// Asks the current action to stop.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Called when an action starts, so an earlier stop does not end it.
    pub fn clear_stop(&self) {
        self.stop.store(false, Ordering::Relaxed);
    }

    pub fn is_set(&self) -> bool {
        self.quit.load(Ordering::Relaxed) || self.stop.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn a_stop_ends_the_current_action_until_it_is_cleared() {
        let halt = Halt::new();

        halt.stop();
        let stopped = halt.is_set();
        halt.clear_stop();

        assert!(stopped);
        assert!(!halt.is_set());
    }

    #[test]
    fn a_quit_stays_set_when_the_stop_is_cleared() {
        let halt = Halt::new();

        halt.quit_flag().store(true, Ordering::Relaxed);
        halt.clear_stop();

        assert!(halt.is_set());
    }

    #[test]
    fn clones_share_both_flags() {
        let halt = Halt::new();
        let other = halt.clone();

        other.stop();

        assert!(halt.is_set());
        assert!(!Halt::new().is_set());
    }
}
