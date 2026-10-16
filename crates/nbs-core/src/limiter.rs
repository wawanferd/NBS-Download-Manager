use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Async token-bucket rate limiter. Callers that overdraw the bucket go into
/// debt and sleep until it is repaid, so concurrent callers share the rate.
pub struct RateLimiter {
    state: Mutex<State>,
}

struct State {
    rate: Option<u64>,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    pub fn new(rate: Option<u64>) -> Self {
        Self {
            state: Mutex::new(State {
                rate: rate.filter(|r| *r > 0),
                tokens: 0.0,
                last: Instant::now(),
            }),
        }
    }

    pub fn rate(&self) -> Option<u64> {
        self.state.lock().rate
    }

    pub fn set_rate(&self, rate: Option<u64>) {
        let mut st = self.state.lock();
        st.rate = rate.filter(|r| *r > 0);
        st.tokens = 0.0;
        st.last = Instant::now();
    }

    /// How long the caller must wait before consuming `n` bytes.
    fn reserve(&self, n: usize) -> Option<Duration> {
        let mut st = self.state.lock();
        let rate = st.rate? as f64;
        let now = Instant::now();
        let elapsed = now.duration_since(st.last).as_secs_f64();
        st.last = now;
        // Allow a burst of a quarter second worth of data.
        let capacity = (rate / 4.0).max(16.0 * 1024.0);
        st.tokens = (st.tokens + elapsed * rate).min(capacity);
        st.tokens -= n as f64;
        (st.tokens < 0.0).then(|| Duration::from_secs_f64(-st.tokens / rate))
    }

    pub async fn acquire(&self, n: usize) {
        if let Some(wait) = self.reserve(n) {
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unlimited_never_waits() {
        let l = RateLimiter::new(None);
        assert!(l.reserve(10_000_000).is_none());
    }

    #[tokio::test]
    async fn limited_accumulates_debt() {
        let l = RateLimiter::new(Some(100_000));
        let first = l.reserve(50_000).unwrap();
        let second = l.reserve(50_000).unwrap();
        assert!(second > first);
        assert!((second.as_secs_f64() - 1.0).abs() < 0.05);
    }
}
