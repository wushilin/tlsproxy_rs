use std::time::{Instant, Duration};

/// Idle tracker tracks the connection should still be active or not
pub struct IdleTracker {
    /// Marker for last activity happened on this tracker
    last_active: Instant,

    /// Max duration for idle. If last_active is more than max_idle ago, the tracker should be considered as expired
    max_idle: Duration,
}

/// Default impl for `IdleTracker`.
/// Default config sets last_active to current time and max_idle to 10 minutes
impl Default for IdleTracker {
    fn default() -> Self {
        IdleTracker {
            last_active: Instant::now(),
            max_idle: Duration::from_secs(600)
        }
    }
}


impl IdleTracker {
    /// Create a new IdleTracker with idle duration of `max_idle`. The `last_active` is set to `Now`
    pub fn new(max_idle:Duration) -> IdleTracker {
        let last_active = Instant::now();
        return IdleTracker { 
            last_active,
            max_idle
         };
    }

    /// Mark this `IdleTracker` as just used. The `last_active` is reset to just `Now`
    pub fn mark(&mut self) -> Instant {
        let result = self.last_active;
        self.last_active = Instant::now();
        return result;
    }

    /// Get the internal max_idle for this tracker
    pub fn max_idle(&self) -> Duration {
        self.max_idle
    }

    /// Return if the tracker is expired (e.g. most recent activity was more than max_idle ago)
    pub fn is_expired(&self) -> bool {
        return self.last_active.elapsed() > self.max_idle;
    }

    /// Get the idled duration of the tracker
    pub fn idled_for(&self) -> Duration {
        self.last_active.elapsed()
    }
}