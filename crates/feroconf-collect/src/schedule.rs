//! When to back up which device, for `feroconf watch`.
//!
//! A trigger makes a device due `debounce` from now; more triggers before then only push that
//! moment back, so a burst of config changes becomes one backup. A device is never backed up
//! twice at the same time: a trigger that arrives during a backup makes it due again afterwards.

use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
struct State {
    due: Option<(Instant, String)>,
    running: bool,
}

#[derive(Debug)]
pub struct Scheduler {
    debounce: Duration,
    devices: HashMap<String, State>,
}

impl Scheduler {
    pub fn new(debounce: Duration) -> Self {
        Self {
            debounce,
            devices: HashMap::new(),
        }
    }

    pub fn debounce(&self) -> Duration {
        self.debounce
    }

    /// Back up `device` after the debounce delay. Returns when it is due.
    pub fn trigger(&mut self, device: &str, reason: &str, now: Instant) -> Instant {
        let at = now + self.debounce;
        self.devices.entry(device.to_string()).or_default().due = Some((at, reason.to_string()));
        at
    }

    /// Back up `device` as soon as it is not running, unless it is already due sooner.
    pub fn trigger_now(&mut self, device: &str, reason: &str, now: Instant) {
        let st = self.devices.entry(device.to_string()).or_default();
        if st.due.as_ref().is_none_or(|(at, _)| *at > now) {
            st.due = Some((now, reason.to_string()));
        }
    }

    /// Devices whose backup should start now, with the reason. They count as running until
    /// [`Scheduler::finished`].
    pub fn take_due(&mut self, now: Instant) -> Vec<(String, String)> {
        let mut out: Vec<_> = self
            .devices
            .iter_mut()
            .filter(|(_, st)| !st.running && st.due.as_ref().is_some_and(|(at, _)| *at <= now))
            .map(|(name, st)| {
                st.running = true;
                let (_, reason) = st.due.take().expect("due");
                (name.clone(), reason)
            })
            .collect();
        out.sort();
        out
    }

    pub fn finished(&mut self, device: &str) {
        if let Some(st) = self.devices.get_mut(device) {
            st.running = false;
        }
    }

    pub fn is_running(&self, device: &str) -> bool {
        self.devices.get(device).is_some_and(|s| s.running)
    }

    /// The earliest moment a device becomes due, ignoring devices that are running.
    pub fn next_due(&self) -> Option<Instant> {
        self.devices
            .values()
            .filter(|s| !s.running)
            .filter_map(|s| s.due.as_ref().map(|(at, _)| *at))
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: Duration = Duration::from_secs(15);

    #[test]
    fn debounces_a_burst() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(D);
        s.trigger("r1", "trap 1", t0);
        s.trigger("r1", "trap 2", t0 + Duration::from_secs(10));
        assert!(s.take_due(t0 + D).is_empty(), "second trap pushed it back");
        assert_eq!(
            s.take_due(t0 + Duration::from_secs(25)),
            [("r1".to_string(), "trap 2".to_string())]
        );
        assert!(
            s.take_due(t0 + Duration::from_secs(60)).is_empty(),
            "only once"
        );
    }

    #[test]
    fn trigger_during_backup_runs_again_afterwards() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(D);
        s.trigger_now("r1", "start", t0);
        assert_eq!(s.take_due(t0).len(), 1);
        assert!(s.is_running("r1"));

        s.trigger("r1", "changed meanwhile", t0 + Duration::from_secs(1));
        assert!(
            s.take_due(t0 + Duration::from_secs(30)).is_empty(),
            "still running"
        );
        assert_eq!(s.next_due(), None);

        s.finished("r1");
        assert!(s.next_due().is_some());
        assert_eq!(s.take_due(t0 + Duration::from_secs(30)).len(), 1);
    }

    #[test]
    fn trigger_now_does_not_delay_an_earlier_due() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(D);
        s.trigger_now("r1", "interval", t0);
        s.trigger_now("r1", "later", t0 + Duration::from_secs(5));
        assert_eq!(s.take_due(t0)[0].1, "interval");
    }

    #[test]
    fn devices_are_independent() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(D);
        s.trigger("r1", "a", t0);
        s.trigger("r2", "b", t0 + Duration::from_secs(5));
        assert_eq!(s.next_due(), Some(t0 + D));
        assert_eq!(s.take_due(t0 + D).len(), 1);
        assert_eq!(s.take_due(t0 + D + Duration::from_secs(5)).len(), 1);
    }
}
