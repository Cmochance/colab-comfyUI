use std::time::{Instant, SystemTime};

use serde::{Deserialize, Serialize};

pub const DEFAULT_WATCHDOG_IDLE_MINUTES: u32 = 30;
pub const WATCHDOG_WARNING_SECS: u64 = 180;
pub const WATCHDOG_SLEEP_JUMP_SECS: i64 = 60;

/// Dual-clock abstraction providing both monotonic and wall-clock timestamps
/// so sleep/wake jumps can be detected deterministically in tests and production.
pub trait Clock: Send + Sync {
    fn now_monotonic(&self) -> Instant;
    fn now_wall(&self) -> SystemTime;
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_monotonic(&self) -> Instant {
        Instant::now()
    }

    fn now_wall(&self) -> SystemTime {
        SystemTime::now()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatchdogPhase {
    Disabled,
    Counting,
    Warning,
    Stopping,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WatchdogSnapshot {
    pub enabled: bool,
    pub threshold_minutes: Option<u32>,
    pub phase: WatchdogPhase,
    pub idle_secs: u64,
    pub warning_remaining_secs: Option<u64>,
    pub sleep_wake_notice: bool,
}

impl Default for WatchdogSnapshot {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_minutes: Some(DEFAULT_WATCHDOG_IDLE_MINUTES),
            phase: WatchdogPhase::Counting,
            idle_secs: 0,
            warning_remaining_secs: None,
            sleep_wake_notice: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WatchdogTickOutcome {
    pub entered_warning: bool,
    pub trigger_stop: bool,
    pub sleep_wake_detected: bool,
    pub needs_wake_verify: bool,
}

#[derive(Debug, Clone)]
pub struct WatchdogState {
    threshold_minutes: Option<u32>,
    phase: WatchdogPhase,
    idle_secs: u64,
    override_threshold_secs: Option<u64>,
    last_mono: Instant,
    last_wall: SystemTime,
    warning_started_mono: Option<Instant>,
    sleep_wake_notice: bool,
    pending_wake_verify: bool,
    has_fresh_idle_since_warning: bool,
}

impl WatchdogState {
    pub fn new(threshold_minutes: Option<u32>, now_mono: Instant, now_wall: SystemTime) -> Self {
        let normalized = normalize_threshold(threshold_minutes);
        Self {
            threshold_minutes: normalized,
            phase: if normalized.is_some() {
                WatchdogPhase::Counting
            } else {
                WatchdogPhase::Disabled
            },
            idle_secs: 0,
            override_threshold_secs: None,
            last_mono: now_mono,
            last_wall: now_wall,
            warning_started_mono: None,
            sleep_wake_notice: false,
            pending_wake_verify: false,
            has_fresh_idle_since_warning: false,
        }
    }

    pub fn set_threshold(
        &mut self,
        threshold_minutes: Option<u32>,
        now_mono: Instant,
        now_wall: SystemTime,
    ) {
        self.threshold_minutes = normalize_threshold(threshold_minutes);
        self.last_mono = now_mono;
        self.last_wall = now_wall;
        self.idle_secs = 0;
        self.override_threshold_secs = None;
        self.warning_started_mono = None;
        self.sleep_wake_notice = false;
        self.pending_wake_verify = false;
        self.has_fresh_idle_since_warning = false;
        self.phase = if self.threshold_minutes.is_some() {
            WatchdogPhase::Counting
        } else {
            WatchdogPhase::Disabled
        };
    }

    pub fn reset_for_new_session(&mut self, now_mono: Instant, now_wall: SystemTime) {
        self.last_mono = now_mono;
        self.last_wall = now_wall;
        self.idle_secs = 0;
        self.override_threshold_secs = None;
        self.warning_started_mono = None;
        self.sleep_wake_notice = false;
        self.pending_wake_verify = false;
        self.has_fresh_idle_since_warning = false;
        self.phase = if self.threshold_minutes.is_some() {
            WatchdogPhase::Counting
        } else {
            WatchdogPhase::Disabled
        };
    }

    pub fn reset_after_failed_stop(&mut self, now_mono: Instant, now_wall: SystemTime) {
        self.last_mono = now_mono;
        self.last_wall = now_wall;
        self.idle_secs = 0;
        self.override_threshold_secs = None;
        self.warning_started_mono = None;
        self.pending_wake_verify = false;
        self.has_fresh_idle_since_warning = false;
        self.phase = if self.threshold_minutes.is_some() {
            WatchdogPhase::Counting
        } else {
            WatchdogPhase::Disabled
        };
    }

    pub fn extend(&mut self, extra_minutes: u32, now_mono: Instant, now_wall: SystemTime) {
        if self.threshold_minutes.is_none() {
            return;
        }
        self.override_threshold_secs = Some(u64::from(extra_minutes.clamp(1, 240)) * 60);
        self.idle_secs = 0;
        self.warning_started_mono = None;
        self.sleep_wake_notice = false;
        self.pending_wake_verify = false;
        self.has_fresh_idle_since_warning = false;
        self.last_mono = now_mono;
        self.last_wall = now_wall;
        self.phase = WatchdogPhase::Counting;
    }

    pub fn clear_wake_verify(&mut self) {
        self.pending_wake_verify = false;
    }

    pub fn dismiss_sleep_notice(&mut self) {
        self.sleep_wake_notice = false;
    }

    pub fn snapshot(&self, now_mono: Instant) -> WatchdogSnapshot {
        let warning_remaining_secs = match (self.phase, self.warning_started_mono) {
            (WatchdogPhase::Warning, Some(started)) => {
                let elapsed = now_mono.saturating_duration_since(started).as_secs();
                Some(WATCHDOG_WARNING_SECS.saturating_sub(elapsed))
            }
            _ => None,
        };
        WatchdogSnapshot {
            enabled: self.threshold_minutes.is_some(),
            threshold_minutes: self.threshold_minutes,
            phase: self.phase,
            idle_secs: self.idle_secs,
            warning_remaining_secs,
            sleep_wake_notice: self.sleep_wake_notice,
        }
    }

    fn effective_threshold_secs(&self, mins: u32) -> u64 {
        self.override_threshold_secs
            .unwrap_or_else(|| u64::from(mins) * 60)
    }

    /// Advances the watchdog using `queue_idle`:
    /// - `Some(true)`: fresh `/queue` probe confirmed 0 running & 0 pending
    /// - `Some(false)`: fresh `/queue` probe showed work running/pending
    /// - `None`: `/queue` probe timed out or failed (pauses idle counting & blocks auto-stop)
    pub fn tick(
        &mut self,
        queue_idle: Option<bool>,
        now_mono: Instant,
        now_wall: SystemTime,
    ) -> WatchdogTickOutcome {
        let mut outcome = WatchdogTickOutcome::default();
        let Some(mins) = self.threshold_minutes else {
            self.phase = WatchdogPhase::Disabled;
            self.last_mono = now_mono;
            self.last_wall = now_wall;
            return outcome;
        };
        if self.phase == WatchdogPhase::Stopping {
            return outcome;
        }

        let delta_mono_secs = now_mono.saturating_duration_since(self.last_mono).as_secs() as i64;
        let delta_wall_secs = match now_wall.duration_since(self.last_wall) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        };
        self.last_mono = now_mono;
        self.last_wall = now_wall;

        let threshold_secs = self.effective_threshold_secs(mins);
        let jump = (delta_wall_secs - delta_mono_secs).abs();
        if jump > WATCHDOG_SLEEP_JUMP_SECS {
            outcome.sleep_wake_detected = true;
            outcome.needs_wake_verify = true;
            self.sleep_wake_notice = true;
            self.pending_wake_verify = true;
            self.has_fresh_idle_since_warning = false;
            if self.phase == WatchdogPhase::Warning || self.idle_secs >= threshold_secs {
                // Defer warning restart until triple-gate + fresh /queue verification passes.
                self.phase = WatchdogPhase::Counting;
                self.warning_started_mono = None;
                self.idle_secs = threshold_secs;
            } else {
                // M4: reset partial idle counter on sleep/wake so UI notice is accurate.
                self.phase = WatchdogPhase::Counting;
                self.warning_started_mono = None;
                self.idle_secs = 0;
                self.override_threshold_secs = None;
            }
            return outcome;
        }

        match queue_idle {
            Some(false) => {
                self.idle_secs = 0;
                self.override_threshold_secs = None;
                self.warning_started_mono = None;
                self.pending_wake_verify = false;
                self.has_fresh_idle_since_warning = false;
                self.phase = WatchdogPhase::Counting;
                return outcome;
            }
            None => {
                // Unknown /queue state (timeout or tunnel stall): pause idle accumulation
                // and never trigger stop until a fresh /queue == 0/0 succeeds.
                if self.pending_wake_verify {
                    outcome.needs_wake_verify = true;
                }
                return outcome;
            }
            Some(true) => {}
        }

        if self.pending_wake_verify {
            outcome.needs_wake_verify = true;
            return outcome;
        }

        match self.phase {
            WatchdogPhase::Disabled | WatchdogPhase::Stopping => {}
            WatchdogPhase::Counting => {
                self.idle_secs = self.idle_secs.saturating_add(delta_mono_secs.max(0) as u64);
                if self.idle_secs >= threshold_secs {
                    self.phase = WatchdogPhase::Warning;
                    self.warning_started_mono = Some(now_mono);
                    self.has_fresh_idle_since_warning = true;
                    outcome.entered_warning = true;
                }
            }
            WatchdogPhase::Warning => {
                self.has_fresh_idle_since_warning = true;
                if let Some(started) = self.warning_started_mono {
                    let elapsed = now_mono.saturating_duration_since(started).as_secs();
                    if elapsed >= WATCHDOG_WARNING_SECS && self.has_fresh_idle_since_warning {
                        self.phase = WatchdogPhase::Stopping;
                        outcome.trigger_stop = true;
                    }
                } else {
                    self.warning_started_mono = Some(now_mono);
                }
            }
        }
        outcome
    }
}

fn normalize_threshold(minutes: Option<u32>) -> Option<u32> {
    minutes.map(|m| m.clamp(1, 240))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn watchdog_counts_idle_warns_for_180s_and_triggers_stop_and_pauses_on_none() {
        let m0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut wd = WatchdogState::new(Some(30), m0, w0);

        let o1 = wd.tick(
            Some(true),
            m0 + Duration::from_secs(600),
            w0 + Duration::from_secs(600),
        );
        assert!(!o1.entered_warning);
        assert_eq!(wd.snapshot(m0 + Duration::from_secs(600)).idle_secs, 600);

        // H1: None pauses idle accumulation without resetting or advancing.
        let o_none = wd.tick(
            None,
            m0 + Duration::from_secs(1200),
            w0 + Duration::from_secs(1200),
        );
        assert!(!o_none.entered_warning);
        assert_eq!(wd.snapshot(m0 + Duration::from_secs(1200)).idle_secs, 600);

        // Busy queue resets idle counter.
        let _ = wd.tick(
            Some(false),
            m0 + Duration::from_secs(1210),
            w0 + Duration::from_secs(1210),
        );
        assert_eq!(wd.snapshot(m0 + Duration::from_secs(1210)).idle_secs, 0);

        // Reach 30m idle -> enters Warning (180s).
        let o2 = wd.tick(
            Some(true),
            m0 + Duration::from_secs(1210 + 1800),
            w0 + Duration::from_secs(1210 + 1800),
        );
        assert!(o2.entered_warning);
        assert!(!o2.trigger_stop);

        // H1: None during Warning blocks trigger_stop even past 180s until fresh Some(true).
        let o_warn_none = wd.tick(
            None,
            m0 + Duration::from_secs(1210 + 1800 + 200),
            w0 + Duration::from_secs(1210 + 1800 + 200),
        );
        assert!(!o_warn_none.trigger_stop);

        let o3 = wd.tick(
            Some(true),
            m0 + Duration::from_secs(1210 + 1800 + 203),
            w0 + Duration::from_secs(1210 + 1800 + 203),
        );
        assert!(o3.trigger_stop);
    }

    #[test]
    fn sleep_wake_jump_requires_verify_then_restarts_full_warning_and_resets_partial_idle() {
        let m0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        // M4: partial idle (< threshold) resets to 0 on wake.
        let mut wd_partial = WatchdogState::new(Some(30), m0, w0);
        let _ = wd_partial.tick(
            Some(true),
            m0 + Duration::from_secs(1000),
            w0 + Duration::from_secs(1000),
        );
        let wake_partial = wd_partial.tick(
            Some(true),
            m0 + Duration::from_secs(1005),
            w0 + Duration::from_secs(1000 + 7200),
        );
        assert!(wake_partial.sleep_wake_detected);
        assert_eq!(
            wd_partial
                .snapshot(m0 + Duration::from_secs(1005))
                .idle_secs,
            0
        );

        // H1: Warning state + wake -> waits for clear_wake_verify() before restarting 180s warning.
        let mut wd = WatchdogState::new(Some(15), m0, w0);
        let enter = wd.tick(
            Some(true),
            m0 + Duration::from_secs(900),
            w0 + Duration::from_secs(900),
        );
        assert!(enter.entered_warning);

        let wake = wd.tick(
            None,
            m0 + Duration::from_secs(905),
            w0 + Duration::from_secs(900 + 7200),
        );
        assert!(wake.sleep_wake_detected);
        assert!(wake.needs_wake_verify);
        assert!(!wake.trigger_stop);
        assert!(!wake.entered_warning);

        // Still blocked until clear_wake_verify()
        let before_verify = wd.tick(
            Some(true),
            m0 + Duration::from_secs(908),
            w0 + Duration::from_secs(900 + 7203),
        );
        assert!(before_verify.needs_wake_verify);
        assert!(!before_verify.entered_warning);

        wd.clear_wake_verify();
        let after_verify = wd.tick(
            Some(true),
            m0 + Duration::from_secs(911),
            w0 + Duration::from_secs(900 + 7206),
        );
        assert!(after_verify.entered_warning);
        assert_eq!(
            wd.snapshot(m0 + Duration::from_secs(911))
                .warning_remaining_secs,
            Some(180)
        );
    }

    #[test]
    fn extend_30_grants_exact_30_minutes_across_15_30_and_60_min_tiers() {
        for tier in [15u32, 30, 60] {
            let m0 = Instant::now();
            let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
            let mut wd = WatchdogState::new(Some(tier), m0, w0);
            let t_tier = u64::from(tier) * 60;
            let _ = wd.tick(
                Some(true),
                m0 + Duration::from_secs(t_tier),
                w0 + Duration::from_secs(t_tier),
            );
            wd.extend(
                30,
                m0 + Duration::from_secs(t_tier + 10),
                w0 + Duration::from_secs(t_tier + 10),
            );
            assert_eq!(
                wd.snapshot(m0 + Duration::from_secs(t_tier + 10)).phase,
                WatchdogPhase::Counting
            );
            // At 29m59s after extend(30): still Counting
            let before = wd.tick(
                Some(true),
                m0 + Duration::from_secs(t_tier + 10 + 1799),
                w0 + Duration::from_secs(t_tier + 10 + 1799),
            );
            assert!(!before.entered_warning, "tier {tier} entered early");
            // At 30m00s after extend(30): enters Warning
            let exact = wd.tick(
                Some(true),
                m0 + Duration::from_secs(t_tier + 10 + 1800),
                w0 + Duration::from_secs(t_tier + 10 + 1800),
            );
            assert!(
                exact.entered_warning,
                "tier {tier} did not enter at exact 30m"
            );
        }
    }
}
