//! Per-client token-bucket rate limiting (in memory).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Run,
    Compile,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

pub struct RateLimiter {
    /// (tokens refilled per second, burst) per kind
    rates: HashMap<Kind, (f64, f64)>,
    buckets: Mutex<HashMap<(IpAddr, Kind), Bucket>>,
}

impl RateLimiter {
    pub fn new(run_per_min: u32, compile_per_min: u32) -> RateLimiter {
        let mut rates = HashMap::new();
        rates.insert(Kind::Run, (run_per_min as f64 / 60.0, (run_per_min as f64).clamp(1.0, 5.0)));
        rates.insert(Kind::Compile, (compile_per_min as f64 / 60.0, (compile_per_min as f64).clamp(1.0, 10.0)));
        RateLimiter { rates, buckets: Mutex::new(HashMap::new()) }
    }

    /// Take one token for `ip`; `Err(retry_after_secs)` when the bucket is empty.
    pub fn check(&self, ip: IpAddr, kind: Kind) -> Result<(), u64> {
        self.check_at(ip, kind, Instant::now())
    }

    fn check_at(&self, ip: IpAddr, kind: Kind, now: Instant) -> Result<(), u64> {
        let (rate, burst) = self.rates[&kind];
        let mut map = self.buckets.lock().unwrap();
        if map.len() > 10_000 {
            // forget clients that have been idle long enough to have a full bucket again
            let idle = Duration::from_secs_f64((burst / rate.max(1e-9)).min(3600.0));
            map.retain(|_, b| now.duration_since(b.last) < idle);
        }
        let b = map.entry((ip, kind)).or_insert(Bucket { tokens: burst, last: now });
        let elapsed = now.duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * rate).min(burst);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - b.tokens) / rate.max(1e-9)).ceil() as u64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    #[test]
    fn burst_then_refill() {
        let rl = RateLimiter::new(60, 600); // run: 1 token/s, burst 5
        let t0 = Instant::now();
        for _ in 0..5 {
            assert!(rl.check_at(ip(1), Kind::Run, t0).is_ok());
        }
        let retry = rl.check_at(ip(1), Kind::Run, t0).unwrap_err();
        assert_eq!(retry, 1);
        assert!(rl.check_at(ip(1), Kind::Run, t0 + Duration::from_millis(1100)).is_ok());
        assert!(rl.check_at(ip(1), Kind::Run, t0 + Duration::from_millis(1200)).is_err());
    }

    #[test]
    fn clients_and_kinds_are_independent() {
        let rl = RateLimiter::new(6, 600);
        let t0 = Instant::now();
        for _ in 0..5 {
            rl.check_at(ip(1), Kind::Run, t0).unwrap();
        }
        assert!(rl.check_at(ip(1), Kind::Run, t0).is_err());
        assert!(rl.check_at(ip(2), Kind::Run, t0).is_ok(), "another client");
        assert!(rl.check_at(ip(1), Kind::Compile, t0).is_ok(), "another endpoint");
    }

    #[test]
    fn tokens_never_exceed_the_burst() {
        let rl = RateLimiter::new(60, 60);
        let t0 = Instant::now();
        rl.check_at(ip(3), Kind::Run, t0).unwrap();
        // a long idle period refills to the burst (5), not beyond
        let later = t0 + Duration::from_secs(3600);
        for _ in 0..5 {
            assert!(rl.check_at(ip(3), Kind::Run, later).is_ok());
        }
        assert!(rl.check_at(ip(3), Kind::Run, later).is_err());
    }
}
