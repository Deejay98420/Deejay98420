//! Non-blocking token bucket + remote cooldown (HTTP 429 / Retry-After).

#[derive(Debug, Clone)]
pub struct TokenBucket {
    rate_per_s: f64,
    burst: f64,
    tokens: f64,
    last_us: i64,
    cooldown_until_us: i64,
}

impl TokenBucket {
    pub fn new(rate_per_s: f64, burst: u32, now_us: i64) -> Self {
        Self { rate_per_s, burst: burst as f64, tokens: burst as f64, last_us: now_us, cooldown_until_us: 0 }
    }
    pub fn try_acquire(&mut self, now_us: i64) -> Result<(), RateLimitReason> {
        if now_us < self.cooldown_until_us {
            return Err(RateLimitReason::RemoteCooldown { until_us: self.cooldown_until_us });
        }
        let dt = (now_us - self.last_us).max(0) as f64 / 1e6;
        self.tokens = (self.tokens + dt * self.rate_per_s).min(self.burst);
        self.last_us = now_us;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            Err(RateLimitReason::LocalBudget)
        }
    }
    /// Microseconds until `try_acquire` can next succeed (0 = now), including any remote cooldown.
    pub fn wait_us(&self, now_us: i64) -> i64 {
        if now_us < self.cooldown_until_us {
            return self.cooldown_until_us - now_us;
        }
        let dt = (now_us - self.last_us).max(0) as f64 / 1e6;
        let tokens = (self.tokens + dt * self.rate_per_s).min(self.burst);
        if tokens >= 1.0 || self.rate_per_s <= 0.0 {
            0
        } else {
            ((1.0 - tokens) / self.rate_per_s * 1e6).ceil() as i64
        }
    }
    pub fn cooldown_until_us(&self) -> i64 {
        self.cooldown_until_us
    }
    pub fn cooldown(&mut self, now_us: i64, ms: u64) {
        self.cooldown_until_us = self.cooldown_until_us.max(now_us + ms as i64 * 1000);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RateLimitReason {
    LocalBudget,
    RemoteCooldown { until_us: i64 },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bucket_refills_and_cools_down() {
        let mut b = TokenBucket::new(2.0, 2, 0);
        assert!(b.try_acquire(0).is_ok());
        assert!(b.try_acquire(0).is_ok());
        assert_eq!(b.try_acquire(0), Err(RateLimitReason::LocalBudget));
        assert!(b.try_acquire(500_000).is_ok()); // +1 token after 0.5s
        b.cooldown(500_000, 1000);
        assert!(matches!(b.try_acquire(1_000_000), Err(RateLimitReason::RemoteCooldown { .. })));
        assert!(b.try_acquire(2_000_000).is_ok());
    }
}
