use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// ConnStats records the connection statistics. It can track:
/// ID or connection
/// start time of connection
/// uploaded bytes
/// downloaded bytes
/// reference to global stats
#[derive(Debug)]
pub struct ConnStats {
    id: usize,
    start: Instant,
    uploaded_bytes: Arc<AtomicUsize>,
    downloaded_bytes: Arc<AtomicUsize>,
    global_stats: Arc<GlobalStats>,
}

impl ConnStats {
    /// Create new connection stats with global stats. The reason we need global stats is that we
    /// need to update global stats finally to let master know globally how many bytes are transferred.
    pub fn new(gstat: Arc<GlobalStats>) -> ConnStats {
        gstat.increase_conn_count();
        gstat.increase_active_conn_count();
        let result = ConnStats {
            id: gstat.gen_conn_id(),
            start: Instant::now(),
            uploaded_bytes: new_au(0),
            downloaded_bytes: new_au(0),
            global_stats: Arc::clone(&gstat),
        };
        return result;
    }

    /// Get connection elapsed time
    pub fn elapsed(&self) -> Duration {
        return self.start.elapsed();
    }

    /// Get reference of global status
    pub fn get_global_stats(&self) -> Arc<GlobalStats> {
        return Arc::clone(&self.global_stats);
    }

    /// Return connection id as string
    pub fn id_str(&self) -> String {
        let id = self.id;
        return format!("conn {id}");
    }

    /// Return conneciton id as usize
    pub fn id(&self) -> usize {
        return self.id;
    }

    /// Add downloaded bytes to this object
    pub fn add_downloaded_bytes(&self, new: usize) -> usize {
        let result = self.downloaded_bytes.fetch_add(new, Ordering::SeqCst) + new;
        self.global_stats.add_downloaded_bytes(new);
        return result;
    }

    /// Add uploaded bytes to this object
    pub fn add_uploaded_bytes(&self, new: usize) -> usize {
        let result = self.uploaded_bytes.fetch_add(new, Ordering::SeqCst) + new;
        self.global_stats.add_uploaded_bytes(new);
        return result;
    }

    /// Get downloaded bytes
    pub fn downloaded_bytes(&self) -> usize {
        return self.downloaded_bytes.load(Ordering::SeqCst);
    }

    /// Get uploaded bytes
    pub fn uploaded_bytes(&self) -> usize {
        return self.uploaded_bytes.load(Ordering::SeqCst);
    }
}

/// When connect stats is dropped, global will need to decrease active connection count
impl Drop for ConnStats {
    /// Upon drop, decrease active connection count
    fn drop(&mut self) {
        self.global_stats.decrease_active_conn_count();
    }
}

/// Connection stats can be cloned since most of fields are just Arc<AtomicUsize>
impl Clone for ConnStats {
    fn clone(&self) -> ConnStats {
        return ConnStats {
            id: self.id,
            start: self.start,
            uploaded_bytes: Arc::clone(&self.uploaded_bytes),
            downloaded_bytes: Arc::clone(&self.downloaded_bytes),
            global_stats: Arc::clone(&self.global_stats),
        };
    }
}


/// Global stats. It tracks connection id generator, connection total count, active count, uploaded bytes and downloaded bytes
#[derive(Debug)]
pub struct GlobalStats {
    id_gen: Arc<AtomicUsize>,
    conn_count: Arc<AtomicUsize>,
    active_conn_count: Arc<AtomicUsize>,
    total_uploaded_bytes: Arc<AtomicUsize>,
    total_downloaded_bytes: Arc<AtomicUsize>,
}

/// Global stats is totally cloneable since all fields are just Arc<AtomicUsize>
impl Clone for GlobalStats {
    fn clone(&self) -> GlobalStats {
        return GlobalStats {
            id_gen: Arc::clone(&self.id_gen),
            conn_count: Arc::clone(&self.conn_count),
            active_conn_count: Arc::clone(&self.active_conn_count),
            total_downloaded_bytes: Arc::clone(&self.total_downloaded_bytes),
            total_uploaded_bytes: Arc::clone(&self.total_uploaded_bytes),
        };
    }
}

fn new_au(start: usize) -> Arc<AtomicUsize> {
    let au = AtomicUsize::new(start);
    return Arc::new(au);
}


impl GlobalStats {
    /// Create new Global Stats where everything is set to zero.
    pub fn new() -> GlobalStats {
        return GlobalStats {
            id_gen: new_au(0),
            conn_count: new_au(0),
            active_conn_count: new_au(0),
            total_downloaded_bytes: new_au(0),
            total_uploaded_bytes: new_au(0),
        };
    }

    fn gen_conn_id(&self) -> usize {
        return self.id_gen.fetch_add(1, Ordering::SeqCst) + 1;
    }

    fn increase_conn_count(&self) -> usize {
        return self.conn_count.fetch_add(1, Ordering::SeqCst) + 1;
    }

    /// Get total connection count
    pub fn conn_count(&self) -> usize {
        return self.conn_count.load(Ordering::SeqCst);
    }

    fn increase_active_conn_count(&self) -> usize {
        return self.active_conn_count.fetch_add(1, Ordering::SeqCst) + 1;
    }

    fn decrease_active_conn_count(&self) -> usize {
        return self.active_conn_count.fetch_sub(1, Ordering::SeqCst) - 1;
    }

    /// Get active connection count
    pub fn active_conn_count(&self) -> usize {
        return self.active_conn_count.load(Ordering::SeqCst);
    }

    fn add_downloaded_bytes(&self, new: usize) -> usize {
        return self.total_downloaded_bytes.fetch_add(new, Ordering::SeqCst) + new;
    }

    /// Get total downloaded bytes
    pub fn total_downloaded_bytes(&self) -> usize {
        return self.total_downloaded_bytes.load(Ordering::SeqCst);
    }

    fn add_uploaded_bytes(&self, new: usize) -> usize {
        return self.total_uploaded_bytes.fetch_add(new, Ordering::SeqCst) + new;
    }

    /// Get total uploaded bytes
    pub fn total_uploaded_bytes(&self) -> usize {
        return self.total_uploaded_bytes.load(Ordering::SeqCst);
    }
}
