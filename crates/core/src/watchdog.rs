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
}

#[derive(Debug, Clone)]
pub struct WatchdogState {
    threshold_minutes: Option<u32>,
    phase: WatchdogPhase,
    idle_secs: u64,
    bonus_grace_secs: u64,
    last_mono: Instant,
    last_wall: SystemTime,
    warning_started_mono: Option<Instant>,
    sleep_wake_notice: bool,
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
            bonus_grace_secs: 0,
            last_mono: now_mono,
            last_wall: now_wall,
            warning_started_mono: None,
            sleep_wake_notice: false,
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
        self.bonus_grace_secs = 0;
        self.warning_started_mono = None;
        self.sleep_wake_notice = false;
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
        self.bonus_grace_secs = 0;
        self.warning_started_mono = None;
        self.sleep_wake_notice = false;
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
        let base_secs = u64::from(self.threshold_minutes.unwrap_or(0)) * 60;
        let extra_secs = u64::from(extra_minutes) * 60;
        self.bonus_grace_secs = extra_secs.saturating_sub(base_secs);
        self.idle_secs = 0;
        self.warning_started_mono = None;
        self.sleep_wake_notice = false;
        self.last_mono = now_mono;
        self.last_wall = now_wall;
        self.phase = WatchdogPhase::Counting;
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

    pub fn tick(
        &mut self,
        queue_idle: bool,
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

        let jump = (delta_wall_secs - delta_mono_secs).abs();
        if jump > WATCHDOG_SLEEP_JUMP_SECS {
            outcome.sleep_wake_detected = true;
            self.sleep_wake_notice = true;
            if !queue_idle {
                self.idle_secs = 0;
                self.bonus_grace_secs = 0;
                self.warning_started_mono = None;
                self.phase = WatchdogPhase::Counting;
                return outcome;
            }
            // Never skip the 3-minute warning countdown immediately after sleep wake:
            // if already warning or past threshold, restart a full 180s warning window.
            let threshold_secs = u64::from(mins) * 60 + self.bonus_grace_secs;
            if self.phase == WatchdogPhase::Warning || self.idle_secs >= threshold_secs {
                self.phase = WatchdogPhase::Warning;
                self.warning_started_mono = Some(now_mono);
                outcome.entered_warning = true;
                return outcome;
            }
            return outcome;
        }

        if !queue_idle {
            self.idle_secs = 0;
            self.bonus_grace_secs = 0;
            self.warning_started_mono = None;
            self.phase = WatchdogPhase::Counting;
            return outcome;
        }

        let threshold_secs = u64::from(mins) * 60 + self.bonus_grace_secs;
        match self.phase {
            WatchdogPhase::Disabled | WatchdogPhase::Stopping => {}
            WatchdogPhase::Counting => {
                self.idle_secs = self.idle_secs.saturating_add(delta_mono_secs.max(0) as u64);
                if self.idle_secs >= threshold_secs {
                    self.phase = WatchdogPhase::Warning;
                    self.warning_started_mono = Some(now_mono);
                    outcome.entered_warning = true;
                }
            }
            WatchdogPhase::Warning => {
                if let Some(started) = self.warning_started_mono {
                    let elapsed = now_mono.saturating_duration_since(started).as_secs();
                    if elapsed >= WATCHDOG_WARNING_SECS {
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
    fn watchdog_counts_idle_warns_for_180s_and_triggers_stop() {
        let m0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut wd = WatchdogState::new(Some(30), m0, w0);

        // Busy queue resets idle counter.
        let o1 = wd.tick(
            true,
            m0 + Duration::from_secs(600),
            w0 + Duration::from_secs(600),
        );
        assert!(!o1.entered_warning);
        assert_eq!(wd.snapshot(m0 + Duration::from_secs(600)).idle_secs, 600);

        let _ = wd.tick(
            false,
            m0 + Duration::from_secs(610),
            w0 + Duration::from_secs(610),
        );
        assert_eq!(wd.snapshot(m0 + Duration::from_secs(610)).idle_secs, 0);

        // Reach 30m idle -> enters Warning (180s).
        let o2 = wd.tick(
            true,
            m0 + Duration::from_secs(610 + 1800),
            w0 + Duration::from_secs(610 + 1800),
        );
        assert!(o2.entered_warning);
        assert!(!o2.trigger_stop);
        assert_eq!(
            wd.snapshot(m0 + Duration::from_secs(610 + 1800))
                .warning_remaining_secs,
            Some(180)
        );

        // After 180s in Warning -> triggers stop.
        let o3 = wd.tick(
            true,
            m0 + Duration::from_secs(610 + 1800 + 180),
            w0 + Duration::from_secs(610 + 1800 + 180),
        );
        assert!(o3.trigger_stop);
    }

    #[test]
    fn sleep_wake_jump_restarts_full_warning_instead_of_immediate_stop() {
        let m0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut wd = WatchdogState::new(Some(15), m0, w0);

        // Reach 15m idle -> Warning.
        let enter = wd.tick(
            true,
            m0 + Duration::from_secs(900),
            w0 + Duration::from_secs(900),
        );
        assert!(enter.entered_warning);

        // Simulate Mac sleeping 2 hours (wall jumped +7200s, monotonic only +5s).
        let wake = wd.tick(
            true,
            m0 + Duration::from_secs(905),
            w0 + Duration::from_secs(900 + 7200),
        );
        assert!(wake.sleep_wake_detected);
        assert!(
            !wake.trigger_stop,
            "must never immediately stop on sleep wake without a fresh 3m warning"
        );
        assert!(wake.entered_warning);
        assert_eq!(
            wd.snapshot(m0 + Duration::from_secs(905))
                .warning_remaining_secs,
            Some(180)
        );
    }

    #[test]
    fn extend_clears_warning_and_grants_30_minutes() {
        let m0 = Instant::now();
        let w0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut wd = WatchdogState::new(Some(15), m0, w0);
        let _ = wd.tick(
            true,
            m0 + Duration::from_secs(900),
            w0 + Duration::from_secs(900),
        );
        wd.extend(
            30,
            m0 + Duration::from_secs(930),
            w0 + Duration::from_secs(930),
        );
        assert_eq!(
            wd.snapshot(m0 + Duration::from_secs(930)).phase,
            WatchdogPhase::Counting
        );
        // 20 minutes later still counting because extend(30) granted 30 minutes
        let o = wd.tick(
            true,
            m0 + Duration::from_secs(930 + 1200),
            w0 + Duration::from_secs(930 + 1200),
        );
        assert!(!o.entered_warning);
    }
}
