use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use log::{error, info, warn};
use time::OffsetDateTime;
use tokio::sync::{Mutex, Notify};
use tokio::time::{Instant, MissedTickBehavior};

use crate::controller::Controller;

pub const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
pub const DEFAULT_RENEWAL_DEADLINE: Duration = Duration::from_secs(5 * 60);

pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenewalCandidate {
    pub certificate_id: String,
    pub domains: Vec<String>,
    pub provider_id: String,
}

/// Storage and ACME protocol operations used by the scheduler. One call to
/// `renew` represents the complete DNS-check through atomic-activation flow.
pub trait RenewalBackend: Send + Sync + 'static {
    fn due_certificates(
        &self,
        now: OffsetDateTime,
    ) -> BackendFuture<'_, Vec<RenewalCandidate>>;

    fn renew(&self, candidate: RenewalCandidate) -> BackendFuture<'_, ()>;

    fn record_timeout(
        &self,
        candidate: &RenewalCandidate,
        deadline: Duration,
    ) -> BackendFuture<'_, ()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanTrigger {
    Startup,
    Scheduled,
    Manual,
}

impl ScanTrigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Scheduled => "scheduled",
            Self::Manual => "manual",
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScanSummary {
    pub due: usize,
    pub renewed: usize,
    pub failed: usize,
    pub timed_out: usize,
}

pub struct RenewalScheduler<B> {
    backend: Arc<B>,
    scan_interval: Duration,
    renewal_deadline: Duration,
    /// Protects against overlap even if a future caller invokes `run_once`
    /// outside the background loop.
    exclusive: Arc<Mutex<()>>,
    wake: Arc<Notify>,
}

impl<B> Clone for RenewalScheduler<B> {
    fn clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
            scan_interval: self.scan_interval,
            renewal_deadline: self.renewal_deadline,
            exclusive: Arc::clone(&self.exclusive),
            wake: Arc::clone(&self.wake),
        }
    }
}

impl<B: RenewalBackend> RenewalScheduler<B> {
    pub fn new(backend: Arc<B>) -> Self {
        Self::with_timing(backend, DEFAULT_SCAN_INTERVAL, DEFAULT_RENEWAL_DEADLINE)
    }

    pub fn with_timing(
        backend: Arc<B>,
        scan_interval: Duration,
        renewal_deadline: Duration,
    ) -> Self {
        assert!(!scan_interval.is_zero(), "scan interval must be non-zero");
        assert!(
            !renewal_deadline.is_zero(),
            "renewal deadline must be non-zero"
        );
        Self {
            backend,
            scan_interval,
            renewal_deadline,
            exclusive: Arc::new(Mutex::new(())),
            wake: Arc::new(Notify::new()),
        }
    }

    /// Coalesces repeated requests while the scheduler is busy. The pending
    /// permit causes an immediate scan after the current one completes.
    pub fn request_scan(&self) {
        self.wake.notify_one();
    }

    pub fn spawn(&self, controller: &mut Controller) {
        let scheduler = self.clone();
        drop(controller.spawn(async move {
            scheduler.run_loop().await;
        }));
    }

    pub async fn run_loop(&self) {
        // Startup is deliberately immediate: missing and already-due
        // certificates must not wait for the first twelve-hour boundary.
        let _ = self.run_once(ScanTrigger::Startup).await;

        let first_tick = Instant::now() + self.scan_interval;
        let mut interval = tokio::time::interval_at(first_tick, self.scan_interval);
        // Keep the fixed cadence, but collapse any number of missed ticks into
        // one immediate tick instead of launching a catch-up burst.
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            let trigger = tokio::select! {
                _ = interval.tick() => ScanTrigger::Scheduled,
                _ = self.wake.notified() => ScanTrigger::Manual,
            };
            let _ = self.run_once(trigger).await;
        }
    }

    pub async fn run_once(&self, trigger: ScanTrigger) -> Result<ScanSummary> {
        let _guard = self.exclusive.lock().await;
        let started = Instant::now();
        let candidates = match self
            .backend
            .due_certificates(OffsetDateTime::now_utc())
            .await
        {
            Ok(candidates) => candidates,
            Err(cause) => {
                error!(
                    "ACME scan failed: trigger={}, error={cause:#}",
                    trigger.as_str()
                );
                return Err(cause);
            }
        };
        let mut summary = ScanSummary {
            due: candidates.len(),
            ..Default::default()
        };
        info!(
            "ACME scan started: trigger={}, due={}",
            trigger.as_str(),
            summary.due
        );

        for candidate in candidates {
            let certificate_id = candidate.certificate_id.clone();
            let domains = candidate.domains.clone();
            let provider_id = candidate.provider_id.clone();
            let renewal_started = Instant::now();
            info!(
                "ACME renewal started: cert={certificate_id}, domains={domains:?}, provider={provider_id}, deadline_seconds={}",
                self.renewal_deadline.as_secs()
            );
            match tokio::time::timeout(
                self.renewal_deadline,
                self.backend.renew(candidate.clone()),
            )
            .await
            {
                Ok(Ok(())) => {
                    summary.renewed += 1;
                    info!(
                        "ACME renewal succeeded: cert={certificate_id}, domains={domains:?}, elapsed_ms={}",
                        renewal_started.elapsed().as_millis()
                    );
                }
                Ok(Err(cause)) => {
                    summary.failed += 1;
                    warn!(
                        "ACME renewal failed: cert={certificate_id}, domains={domains:?}, elapsed_ms={}, error={cause:#}",
                        renewal_started.elapsed().as_millis()
                    );
                }
                Err(_) => {
                    summary.failed += 1;
                    summary.timed_out += 1;
                    warn!(
                        "ACME renewal timed out: cert={certificate_id}, domains={domains:?}, deadline_seconds={}",
                        self.renewal_deadline.as_secs()
                    );
                    if let Err(cause) = self
                        .backend
                        .record_timeout(&candidate, self.renewal_deadline)
                        .await
                    {
                        error!(
                            "ACME timeout state update failed: cert={certificate_id}, error={cause:#}"
                        );
                    }
                }
            }
        }

        info!(
            "ACME scan finished: trigger={}, due={}, renewed={}, failed={}, timed_out={}, elapsed_ms={}",
            trigger.as_str(),
            summary.due,
            summary.renewed,
            summary.failed,
            summary.timed_out,
            started.elapsed().as_millis()
        );
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::anyhow;

    use super::*;

    struct FakeBackend {
        candidates: Vec<RenewalCandidate>,
        active: AtomicUsize,
        maximum_active: AtomicUsize,
        calls: AtomicUsize,
        timeouts: AtomicUsize,
        delay: Duration,
        fail_id: Option<String>,
    }

    impl RenewalBackend for FakeBackend {
        fn due_certificates(
            &self,
            _now: OffsetDateTime,
        ) -> BackendFuture<'_, Vec<RenewalCandidate>> {
            Box::pin(async { Ok(self.candidates.clone()) })
        }

        fn renew(&self, candidate: RenewalCandidate) -> BackendFuture<'_, ()> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.maximum_active.fetch_max(active, Ordering::SeqCst);
                tokio::time::sleep(self.delay).await;
                self.active.fetch_sub(1, Ordering::SeqCst);
                if self.fail_id.as_deref() == Some(&candidate.certificate_id) {
                    Err(anyhow!("expected failure"))
                } else {
                    Ok(())
                }
            })
        }

        fn record_timeout(
            &self,
            _candidate: &RenewalCandidate,
            _deadline: Duration,
        ) -> BackendFuture<'_, ()> {
            Box::pin(async {
                self.active.store(0, Ordering::SeqCst);
                self.timeouts.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    fn candidate(id: &str) -> RenewalCandidate {
        RenewalCandidate {
            certificate_id: id.into(),
            domains: vec![format!("{id}.example")],
            provider_id: "test".into(),
        }
    }

    #[tokio::test]
    async fn concurrent_scans_never_overlap_renewals() {
        let backend = Arc::new(FakeBackend {
            candidates: vec![candidate("one")],
            active: AtomicUsize::new(0),
            maximum_active: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            timeouts: AtomicUsize::new(0),
            delay: Duration::from_millis(20),
            fail_id: None,
        });
        let scheduler = RenewalScheduler::with_timing(
            Arc::clone(&backend),
            Duration::from_secs(60),
            Duration::from_secs(1),
        );
        let (first, second) = tokio::join!(
            scheduler.run_once(ScanTrigger::Manual),
            scheduler.run_once(ScanTrigger::Scheduled)
        );
        first.unwrap();
        second.unwrap();
        assert_eq!(backend.maximum_active.load(Ordering::SeqCst), 1);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn timeout_is_recorded_and_next_certificate_continues() {
        let backend = Arc::new(FakeBackend {
            candidates: vec![candidate("one"), candidate("two")],
            active: AtomicUsize::new(0),
            maximum_active: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            timeouts: AtomicUsize::new(0),
            delay: Duration::from_millis(30),
            fail_id: None,
        });
        let scheduler = RenewalScheduler::with_timing(
            Arc::clone(&backend),
            Duration::from_secs(60),
            Duration::from_millis(5),
        );
        let summary = scheduler.run_once(ScanTrigger::Manual).await.unwrap();
        assert_eq!(summary.due, 2);
        assert_eq!(summary.timed_out, 2);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(backend.timeouts.load(Ordering::SeqCst), 2);
    }
}
