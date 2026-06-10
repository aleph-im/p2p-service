use std::collections::HashMap;
use std::time::{Duration, Instant};

use libp2p::PeerId;

/// Exponential backoff with jitter for dial attempts.
/// Delay after n failures: base * 2^(n-1), capped, multiplied by a jitter
/// factor in [0.5, 1.5].
#[derive(Debug)]
pub struct DialBackoff {
    base: Duration,
    cap: Duration,
    state: HashMap<PeerId, BackoffState>,
}

#[derive(Debug)]
struct BackoffState {
    failures: u32,
    next_attempt: Instant,
}

impl DialBackoff {
    pub fn new(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap,
            state: HashMap::new(),
        }
    }

    pub fn ready(&self, peer: &PeerId, now: Instant) -> bool {
        match self.state.get(peer) {
            None => true,
            Some(state) => now >= state.next_attempt,
        }
    }

    pub fn record_failure(&mut self, peer: PeerId, now: Instant) {
        let failures = self.state.get(&peer).map(|s| s.failures).unwrap_or(0) + 1;
        let exponential = self
            .base
            .saturating_mul(2u32.saturating_pow(failures.saturating_sub(1)));
        let capped = exponential.min(self.cap);
        let jitter = 0.5 + rand::random::<f64>();
        let delay = capped.mul_f64(jitter);
        self.state.insert(
            peer,
            BackoffState {
                failures,
                next_attempt: now + delay,
            },
        );
    }

    pub fn record_success(&mut self, peer: &PeerId) {
        self.state.remove(peer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test 1: a peer with no recorded failures is ready immediately.
    #[test]
    fn unknown_peer_is_ready() {
        let backoff = DialBackoff::new(Duration::from_secs(1), Duration::from_secs(60));
        let peer = PeerId::random();
        assert!(backoff.ready(&peer, Instant::now()));
    }

    /// Test 2: a failure delays the next attempt. With base 10s, the delay is
    /// in [5s, 15s]: not ready at `now`, ready at `now + 16s`.
    #[test]
    fn failure_delays_next_attempt() {
        let mut backoff = DialBackoff::new(Duration::from_secs(10), Duration::from_secs(3600));
        let peer = PeerId::random();
        let now = Instant::now();
        backoff.record_failure(peer, now);
        assert!(!backoff.ready(&peer, now));
        assert!(backoff.ready(&peer, now + Duration::from_secs(16)));
    }

    /// Test 3: repeated failures grow the delay up to the cap. With base 1s
    /// and cap 60s, after 20 failures the delay is in [30s, 90s]:
    /// not ready at `now + 29s`, ready at `now + 91s`.
    #[test]
    fn repeated_failures_grow_to_cap() {
        let mut backoff = DialBackoff::new(Duration::from_secs(1), Duration::from_secs(60));
        let peer = PeerId::random();
        let now = Instant::now();
        for _ in 0..20 {
            backoff.record_failure(peer, now);
        }
        assert!(!backoff.ready(&peer, now + Duration::from_secs(29)));
        assert!(backoff.ready(&peer, now + Duration::from_secs(91)));
    }

    /// Test 4: a success resets the backoff; the peer is ready immediately.
    #[test]
    fn success_resets_backoff() {
        let mut backoff = DialBackoff::new(Duration::from_secs(10), Duration::from_secs(3600));
        let peer = PeerId::random();
        let now = Instant::now();
        backoff.record_failure(peer, now);
        assert!(!backoff.ready(&peer, now));
        backoff.record_success(&peer);
        assert!(backoff.ready(&peer, now));
    }
}
