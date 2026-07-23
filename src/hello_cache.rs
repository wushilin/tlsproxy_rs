use lazy_static::lazy_static;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// Tracks identifiers this proxy has recently sent upstream: the ClientHello
// randoms of forwarded/originated TLS connections, and the request tokens
// injected into proxied plaintext HTTP requests (`X-Tlsproxy-Rid`). An
// inbound connection carrying one of these identifiers is our own forwarded
// bytes arriving back at a listener — a self-connection loop (direct, through
// NAT/port-forwarding, or via a multi-hop proxy chain) — and must be closed.
//
// Entries only need to outlive the time a forwarded identifier could take to
// loop back, which is far shorter than the proxied connection itself. Expired
// entries are evicted lazily on insert; no background task is needed.
//
// The map provides O(1) lookup; the deque tracks insertion order. Because the
// TTL is constant, insertion order equals expiry order, so eviction only ever
// pops from the front. Entries consumed by a hit leave a stale deque node
// behind that is skipped when it reaches the front.

pub const DEFAULT_TTL: Duration = Duration::from_secs(10);

struct Cache<K: Eq + Hash + Clone> {
    by_key: HashMap<K, Instant>,
    by_insertion: VecDeque<(K, Instant)>,
}

impl<K: Eq + Hash + Clone> Cache<K> {
    fn new() -> Cache<K> {
        Cache {
            by_key: HashMap::new(),
            by_insertion: VecDeque::new(),
        }
    }

    fn insert(&mut self, key: K, ttl: Duration) {
        let now = Instant::now();
        self.evict_expired(now);
        let expires_at = now + ttl;
        self.by_key.insert(key.clone(), expires_at);
        self.by_insertion.push_back((key, expires_at));
    }

    fn is_looped(&mut self, key: &K) -> bool {
        match self.by_key.remove(key) {
            Some(expires_at) => expires_at > Instant::now(),
            None => false,
        }
    }

    fn evict_expired(&mut self, now: Instant) {
        while let Some((key, expires_at)) = self.by_insertion.front() {
            if *expires_at > now {
                break;
            }
            // only drop the map entry if it still belongs to this deque node
            if self.by_key.get(key) == Some(expires_at) {
                self.by_key.remove(key);
            }
            self.by_insertion.pop_front();
        }
    }
}

lazy_static! {
    static ref SEEN: Mutex<Cache<[u8; 32]>> = Mutex::new(Cache::new());
    static ref SEEN_TOKENS: Mutex<Cache<String>> = Mutex::new(Cache::new());
}

/// Records a ClientHello random that is about to be forwarded upstream.
pub fn insert(random: [u8; 32]) {
    SEEN.lock().unwrap().insert(random, DEFAULT_TTL);
}

/// Returns true when this proxy recently forwarded a ClientHello with this
/// random, meaning the inbound connection is a self-connection loop.
/// A hit consumes the entry.
pub fn is_looped(random: &[u8; 32]) -> bool {
    SEEN.lock().unwrap().is_looped(random)
}

/// Records a request token injected into a proxied plaintext HTTP request.
pub fn insert_request_token(token: String) {
    SEEN_TOKENS.lock().unwrap().insert(token, DEFAULT_TTL);
}

/// Returns true when this proxy recently injected this token into an
/// upstream HTTP request, meaning the inbound request is a self-connection
/// loop. A hit consumes the entry.
pub fn request_token_is_looped(token: &str) -> bool {
    SEEN_TOKENS.lock().unwrap().is_looped(&token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    #[test]
    fn forwarded_random_is_detected_once() {
        let mut cache = Cache::new();
        cache.insert(random(1), DEFAULT_TTL);
        assert!(cache.is_looped(&random(1)));
        assert!(!cache.is_looped(&random(1)), "hit should consume entry");
    }

    #[test]
    fn unknown_random_is_not_a_loop() {
        let mut cache = Cache::new();
        assert!(!cache.is_looped(&random(2)));
    }

    #[test]
    fn expired_random_is_not_a_loop() {
        let mut cache = Cache::new();
        cache.insert(random(3), Duration::ZERO);
        assert!(!cache.is_looped(&random(3)));
    }

    #[test]
    fn insert_evicts_expired_entries_from_both_structures() {
        let mut cache = Cache::new();
        cache.insert(random(4), Duration::ZERO);
        cache.insert(random(5), DEFAULT_TTL);
        assert!(!cache.by_key.contains_key(&random(4)));
        assert!(cache.by_key.contains_key(&random(5)));
        assert!(!cache
            .by_insertion
            .iter()
            .any(|(random_key, _)| *random_key == random(4)));
    }

    #[test]
    fn stale_node_of_consumed_entry_does_not_evict_fresh_reinsert() {
        let mut cache = Cache::new();
        cache.insert(random(6), Duration::ZERO);
        assert!(!cache.is_looped(&random(6)));
        // re-inserting after consumption must not be evicted by the stale node
        cache.insert(random(6), DEFAULT_TTL);
        cache.insert(random(7), DEFAULT_TTL);
        assert!(cache.by_key.contains_key(&random(6)));
    }
}
