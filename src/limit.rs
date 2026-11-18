//! The `/feedback` rate limit: one 60 s window and one daily counter, global,
//! in memory.
//!
//! Client IPs are useless behind Caddy (everything arrives from the proxy) and
//! the volume is roughly ten mails a year, so per-IP accounting would buy
//! nothing. With one replica, in-memory state is enough; a restart resets it,
//! which at this volume nobody will ever notice.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config;

pub struct Limiter {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    window_start: Option<Instant>,
    window_count: u32,
    day: u32,
    day_count: u32,
}

impl Limiter {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
        }
    }

    /// Takes one slot from both counters, or reports how long to wait.
    ///
    /// Called once a request has been validated and is about to be queued, so
    /// a rejected payload never spends anyone's quota. A request that then
    /// fails to enqueue keeps its slot: the caller is being told to back off
    /// anyway, and refunding would need two locks for no benefit at this
    /// volume.
    pub fn acquire(&self) -> Result<(), u64> {
        let (day, until_midnight) = utc_day();
        let now = Instant::now();

        // Recover from poisoning: the state is two counters, and refusing every
        // request for the rest of the process's life would be worse.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        check(
            &mut state,
            now,
            day,
            until_midnight,
            config::LIMIT_WINDOW,
            config::LIMIT_PER_WINDOW,
            config::LIMIT_PER_DAY,
        )
    }
}

/// The whole policy, with its inputs passed in so the tests can drive a clock
/// and a calendar without sleeping or freezing time.
fn check(
    state: &mut State,
    now: Instant,
    day: u32,
    seconds_until_midnight: u64,
    window: Duration,
    per_window: u32,
    per_day: u32,
) -> Result<(), u64> {
    match state.window_start {
        Some(start) if now.duration_since(start) >= window => {
            state.window_start = Some(now);
            state.window_count = 0;
        }
        None => {
            state.window_start = Some(now);
            state.window_count = 0;
        }
        Some(_) => {}
    }

    if day != state.day {
        state.day = day;
        state.day_count = 0;
    }

    if state.window_count >= per_window {
        let elapsed = now.duration_since(state.window_start.expect("set above"));
        return Err(at_least_one(window.saturating_sub(elapsed).as_secs()));
    }

    if state.day_count >= per_day {
        // The daily counter is what a spam complaint would actually punish, so
        // waiting is measured to the reset, not to the end of the window.
        return Err(at_least_one(seconds_until_midnight));
    }

    state.window_count += 1;
    state.day_count += 1;
    Ok(())
}

/// `Retry-After: 0` invites an immediate retry that will be refused again.
fn at_least_one(seconds: u64) -> u64 {
    seconds.max(1)
}

/// Days since the epoch, and seconds until the next UTC midnight. UTC on
/// purpose: a daily cap that resets at the server's local midnight would move
/// when the host's timezone changed.
fn utc_day() -> (u32, u64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day = (now / 86_400) as u32;
    let into_day = now % 86_400;
    (day, 86_400 - into_day)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(60);
    const PER_WINDOW: u32 = 3;
    const PER_DAY: u32 = 5;

    fn run(state: &mut State, at: Duration, day: u32) -> Result<(), u64> {
        check(
            state,
            Instant::now() + at,
            day,
            3_600,
            WINDOW,
            PER_WINDOW,
            PER_DAY,
        )
    }

    #[test]
    fn window_allows_three_then_refuses_with_a_wait() {
        let mut state = State::default();
        let base = Instant::now();

        for _ in 0..PER_WINDOW {
            assert!(run(&mut state, Duration::ZERO, 20_000).is_ok());
        }

        // 10 s into the window: 50 s left, minus nothing.
        let wait = check(
            &mut state,
            base + Duration::from_secs(10),
            20_000,
            3_600,
            WINDOW,
            PER_WINDOW,
            PER_DAY,
        )
        .unwrap_err();
        assert_eq!(wait, 50);
    }

    #[test]
    fn window_rolls_over() {
        let mut state = State::default();
        for _ in 0..PER_WINDOW {
            assert!(run(&mut state, Duration::ZERO, 20_000).is_ok());
        }
        // Still inside the window.
        assert!(run(&mut state, Duration::from_secs(59), 20_000).is_err());
        // Reset.
        assert!(run(&mut state, Duration::from_secs(60), 20_000).is_ok());
    }

    #[test]
    fn daily_cap_survives_window_resets() {
        let mut state = State::default();
        // Five allowed in total across five separate windows.
        for minute in 0..PER_DAY {
            assert!(run(&mut state, Duration::from_secs(60 * minute as u64), 20_000).is_ok());
        }
        let err = run(&mut state, Duration::from_secs(600), 20_000).unwrap_err();
        // Waits for the day to turn over, not for the window.
        assert_eq!(err, 3_600);
    }

    #[test]
    fn day_rollover_clears_the_daily_counter() {
        let mut state = State::default();
        for _ in 0..PER_WINDOW {
            assert!(run(&mut state, Duration::ZERO, 20_000).is_ok());
        }
        assert!(run(&mut state, Duration::from_secs(120), 20_001).is_ok());
    }

    #[test]
    fn a_rejection_does_not_spend_quota() {
        let mut state = State::default();
        for _ in 0..PER_WINDOW {
            assert!(run(&mut state, Duration::ZERO, 20_000).is_ok());
        }
        // Refused twice; the window still resets on schedule and the day
        // counter is still at three, not five.
        assert!(run(&mut state, Duration::from_secs(1), 20_000).is_err());
        assert!(run(&mut state, Duration::from_secs(2), 20_000).is_err());

        assert!(run(&mut state, Duration::from_secs(60), 20_000).is_ok());
        assert!(run(&mut state, Duration::from_secs(121), 20_000).is_ok());
        assert!(run(&mut state, Duration::from_secs(182), 20_000).is_err());
    }

    #[test]
    fn retry_after_is_never_zero() {
        assert_eq!(at_least_one(0), 1);
        assert_eq!(at_least_one(7), 7);
    }

    #[test]
    fn utc_day_is_plausible() {
        let (day, until_midnight) = utc_day();
        assert!(day > 20_000);
        assert!((1..=86_400).contains(&until_midnight));
    }
}
