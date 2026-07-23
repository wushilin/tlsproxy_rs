use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc, OnceLock,
};

use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub struct ListenerStats {
    pub name: String,
    idle_timeout_ms: AtomicU64,
    pub total: Arc<AtomicUsize>,
    pub active: Arc<AtomicUsize>,
    pub downloaded_bytes: Arc<AtomicUsize>,
    pub uploaded_bytes: Arc<AtomicUsize>,
    transfer: OnceLock<Arc<crate::events_hub::TransferTotals>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatsSerde {
    pub name: String,
    pub total: usize,
    pub active: usize,
    pub downloaded_bytes: usize,
    pub uploaded_bytes: usize,
}

impl StatsSerde {
    pub fn from(input: &ListenerStats) -> Self {
        Self {
            name: input.name.clone(),
            total: input.total_count(),
            active: input.active_count(),
            downloaded_bytes: input.downloaded_bytes_count(),
            uploaded_bytes: input.uploaded_bytes_count(),
        }
    }
}

impl ListenerStats {
    fn newau() -> Arc<AtomicUsize> {
        return Arc::new(AtomicUsize::new(0));
    }
    pub fn new(name: &str, idletimeout: u64) -> Self {
        Self {
            name: name.into(),
            idle_timeout_ms: AtomicU64::new(idletimeout),
            total: Self::newau(),
            active: Self::newau(),
            downloaded_bytes: Self::newau(),
            uploaded_bytes: Self::newau(),
            transfer: OnceLock::new(),
        }
    }

    fn transfer(&self) -> &crate::events_hub::TransferTotals {
        self.transfer.get_or_init(|| crate::events_hub::transfer_totals(&self.name))
    }
    pub fn idle_timeout_ms(&self) -> u64 { self.idle_timeout_ms.load(Ordering::Relaxed) }
    pub fn set_idle_timeout_ms(&self, value: u64) { self.idle_timeout_ms.store(value, Ordering::Relaxed); }
    pub fn increase_conn_count(&self) -> usize {
        self.total.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn decrease_conn_count(&self) -> usize {
        self.active.fetch_sub(1, Ordering::SeqCst) - 1
    }

    pub fn total_count(&self) -> usize {
        self.total.load(Ordering::SeqCst)
    }

    pub fn active_count(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    pub fn increase_uploaded_bytes(&self, count: usize) -> usize {
        self.transfer().add_uploaded(count as u64);
        self.uploaded_bytes.fetch_add(count, Ordering::SeqCst) + count
    }

    pub fn increase_downloaded_bytes(&self, count: usize) -> usize {
        self.transfer().add_downloaded(count as u64);
        self.downloaded_bytes.fetch_add(count, Ordering::SeqCst) + count
    }

    pub fn uploaded_bytes_count(&self) -> usize {
        self.uploaded_bytes.load(Ordering::SeqCst)
    }

    pub fn downloaded_bytes_count(&self) -> usize {
        self.downloaded_bytes.load(Ordering::SeqCst)
    }
}
