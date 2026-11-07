//! Background sampling for the Usage view's local machine section.
//! No credentials, network access, or persistent state are involved.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use sysinfo::System;

pub const REFRESH: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub cpu_percent: Option<f32>,
    /// Used and total physical memory, in bytes.
    pub memory: Option<(u64, u64)>,
    /// Root filesystem's total minus available, and total, in bytes.
    pub disk: Option<(u64, u64)>,
}

struct Sampler {
    system: System,
    has_cpu_baseline: bool,
}

impl Sampler {
    fn new() -> Self {
        Self {
            system: System::new(),
            has_cpu_baseline: false,
        }
    }

    fn sample(&mut self) -> Snapshot {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        let cpu_percent = cpu_value(
            self.has_cpu_baseline && !self.system.cpus().is_empty(),
            self.system.global_cpu_usage(),
        );
        self.has_cpu_baseline = !self.system.cpus().is_empty();
        let total = self.system.total_memory();
        let memory = (total > 0).then(|| (self.system.used_memory().min(total), total));
        // Query exactly /, not every mounted disk (remote mounts can hang,
        // and macOS disk enumeration can hide the read-only system volume).
        let disk = root_usage();
        Snapshot {
            cpu_percent,
            memory,
            disk,
        }
    }
}

#[cfg(unix)]
fn root_usage() -> Option<(u64, u64)> {
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: the constant path is NUL-terminated and stat points to writable
    // storage. Only read the structure after statvfs reports success.
    let stat = unsafe {
        if libc::statvfs(c"/".as_ptr(), stat.as_mut_ptr()) != 0 {
            return None;
        }
        stat.assume_init()
    };
    let total = u64::try_from(stat.f_blocks as u128 * stat.f_frsize as u128).ok()?;
    let available = u64::try_from(stat.f_bavail as u128 * stat.f_frsize as u128).ok()?;
    (total > 0).then(|| (total.saturating_sub(available), total))
}

#[cfg(not(unix))]
fn root_usage() -> Option<(u64, u64)> {
    None
}

fn cpu_value(has_baseline: bool, value: f32) -> Option<f32> {
    (has_baseline && value.is_finite()).then(|| value.clamp(0.0, 100.0))
}

fn due(last_started: Option<Instant>, now: Instant) -> bool {
    last_started.is_none_or(|last| now.saturating_duration_since(last) >= REFRESH)
}

/// Owns at most one sampling worker while the Usage view is open. In-flight
/// work is detached on close and its result discarded, never blocking the UI.
#[derive(Default)]
pub struct Monitor {
    sampler: Option<Sampler>,
    receiver: Option<Receiver<(Sampler, Snapshot)>>,
    last_started: Option<Instant>,
    pub snapshot: Option<Snapshot>,
}

impl Monitor {
    pub fn tick(&mut self, active: bool, now: Instant) {
        self.tick_with(active, now, |mut sampler, sender| {
            std::thread::spawn(move || {
                let snapshot = sampler.sample();
                let _ = sender.send((sampler, snapshot));
            });
        });
    }

    fn tick_with(
        &mut self,
        active: bool,
        now: Instant,
        spawn: impl FnOnce(Sampler, mpsc::Sender<(Sampler, Snapshot)>),
    ) {
        if !active {
            *self = Self::default();
            return;
        }
        if let Some(receiver) = &self.receiver {
            match receiver.try_recv() {
                Ok((sampler, snapshot)) => {
                    self.sampler = Some(sampler);
                    self.snapshot = Some(snapshot);
                    self.receiver = None;
                }
                Err(TryRecvError::Disconnected) => self.receiver = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.receiver.is_none() && due(self.last_started, now) {
            let sampler = self.sampler.take().unwrap_or_else(Sampler::new);
            let (sender, receiver) = mpsc::channel();
            self.last_started = Some(now);
            self.receiver = Some(receiver);
            spawn(sampler, sender);
        }
    }
}

pub fn percent(used: u64, total: u64) -> Option<u64> {
    (total > 0).then(|| ((used.min(total) as f64 / total as f64) * 100.0).round() as u64)
}

pub fn capacity(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Percent is first so it survives truncation in narrow sidebars.
pub fn lines(snapshot: Option<Snapshot>) -> [String; 3] {
    let snapshot = snapshot.unwrap_or_default();
    let cpu = snapshot
        .cpu_percent
        .map_or_else(|| "—".to_owned(), |v| format!("{v:.0}%"));
    let usage = |label: &str, value: Option<(u64, u64)>| match value {
        Some((used, total)) if total > 0 => format!(
            " {label}  {}%  {} / {}",
            percent(used, total).unwrap_or(0),
            capacity(used),
            capacity(total)
        ),
        _ => format!(" {label}  —"),
    };
    [
        format!(" CPU  {cpu}"),
        usage("RAM", snapshot.memory),
        usage("Disk /", snapshot.disk),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentages_and_capacity_are_safe() {
        assert_eq!(percent(1, 0), None);
        assert_eq!(percent(150, 100), Some(100));
        assert_eq!(percent(1, 4), Some(25));
        assert_eq!(capacity(1024 * 1024 * 1024), "1.0 GiB");
        assert_eq!(lines(None), [" CPU  —", " RAM  —", " Disk /  —"]);
    }

    #[test]
    fn cpu_needs_a_baseline_and_invalid_values_stay_unknown() {
        assert_eq!(cpu_value(false, 80.0), None);
        assert_eq!(cpu_value(true, 37.5), Some(37.5));
        assert_eq!(cpu_value(true, f32::NAN), None);
        assert_eq!(cpu_value(true, 120.0), Some(100.0));
    }

    #[test]
    fn fake_sampler_is_throttled_and_stops_when_hidden() {
        let now = Instant::now();
        let mut monitor = Monitor::default();
        let first = Snapshot {
            memory: Some((1, 4)),
            ..Snapshot::default()
        };
        monitor.tick_with(true, now, |sampler, sender| {
            assert!(sender.send((sampler, first)).is_ok());
        });
        monitor.tick_with(true, now + Duration::from_secs(1), |_, _| {
            panic!("too soon")
        });
        assert_eq!(monitor.snapshot, Some(first));
        let second = Snapshot {
            cpu_percent: Some(25.0),
            ..first
        };
        monitor.tick_with(true, now + REFRESH, |sampler, sender| {
            assert!(sender.send((sampler, second)).is_ok());
        });
        monitor.tick_with(true, now + REFRESH, |_, _| panic!("duplicate sample"));
        assert_eq!(monitor.snapshot, Some(second));
        monitor.tick_with(false, now + REFRESH, |_, _| panic!("hidden"));
        assert!(monitor.receiver.is_none());
        assert!(monitor.snapshot.is_none());
        monitor.tick_with(false, now + REFRESH * 2, |_, _| panic!("hidden"));
    }

    #[test]
    fn a_missing_metric_does_not_hide_others() {
        let text = lines(Some(Snapshot {
            cpu_percent: None,
            memory: Some((1, 4)),
            disk: Some((3, 4)),
        }));
        assert_eq!(text[0], " CPU  —");
        assert!(text[1].contains("25%"));
        assert!(text[2].contains("75%"));
    }

    #[test]
    fn native_sample_starts_with_unknown_cpu_and_valid_capacities() {
        let snapshot = Sampler::new().sample();
        assert_eq!(snapshot.cpu_percent, None);
        for (used, total) in [snapshot.memory, snapshot.disk].into_iter().flatten() {
            assert!(total > 0);
            assert!(used <= total);
        }
        println!("native system sample: {snapshot:?}");
    }

    #[test]
    fn refresh_is_throttled() {
        let now = Instant::now();
        assert!(due(None, now));
        assert!(!due(Some(now), now + Duration::from_secs(9)));
        assert!(due(Some(now), now + REFRESH));
    }
}
