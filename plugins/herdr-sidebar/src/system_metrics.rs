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

/// Aligned percentages with separate capacity rows. Leave one cell on each
/// side so values do not run into the pane divider or terminal chrome.
pub fn rows(snapshot: Option<Snapshot>, width: usize, details: bool) -> Vec<String> {
    let snapshot = snapshot.unwrap_or_default();
    let percent_text = |value: Option<(u64, u64)>| {
        value
            .and_then(|(used, total)| percent(used, total))
            .map_or_else(|| "—".to_owned(), |value| format!("{value}%"))
    };
    let summary = |label: &str, value: String| {
        let pad = width
            .saturating_sub(label.len() + value.chars().count() + 2)
            .max(1);
        format!(" {label}{}{value} ", " ".repeat(pad))
            .chars()
            .take(width)
            .collect::<String>()
    };
    let capacity_row = |value: Option<(u64, u64)>| {
        let text = value.filter(|(_, total)| *total > 0).map_or_else(
            || "  —".to_owned(),
            |(used, total)| format!("  {} / {}", capacity(used), capacity(total)),
        );
        text.chars()
            .take(width.saturating_sub(1))
            .collect::<String>()
    };
    let mut rows = vec![summary(
        "CPU",
        snapshot
            .cpu_percent
            .map_or_else(|| "—".to_owned(), |value| format!("{value:.0}%")),
    )];
    rows.push(summary("RAM", percent_text(snapshot.memory)));
    if details {
        rows.push(capacity_row(snapshot.memory));
    }
    rows.push(summary("Disk /", percent_text(snapshot.disk)));
    if details {
        rows.push(capacity_row(snapshot.disk));
    }
    rows
}

/// Current utilization, not a history plot. Unknown data is visibly distinct
/// from a known zero-percent empty bar.
fn progress_bar(value: Option<f32>, width: usize) -> String {
    match value.filter(|v| v.is_finite()) {
        Some(value) => {
            let filled = ((value.clamp(0.0, 100.0) / 100.0) * width as f32).round() as usize;
            format!("{}{}", "━".repeat(filled), "─".repeat(width - filled))
        }
        None => "·".repeat(width),
    }
}

pub fn rows_with_bars(snapshot: Option<Snapshot>, width: usize, details: bool) -> Vec<String> {
    let mut text = rows(snapshot, width, details);
    if width < 17 {
        return text;
    }
    let snapshot = snapshot.unwrap_or_default();
    let ratio = |value: Option<(u64, u64)>| {
        value
            .and_then(|(used, total)| percent(used, total))
            .map(|v| v as f32)
    };
    for (index, label, current) in [
        (0, "CPU", snapshot.cpu_percent),
        (1, "RAM", ratio(snapshot.memory)),
        (if details { 3 } else { 2 }, "Disk /", ratio(snapshot.disk)),
    ] {
        let plot = progress_bar(current, width - 14);
        let value = current.map_or_else(|| "—".to_owned(), |v| format!("{v:.0}%"));
        text[index] = format!(" {label:<6} {plot} {value:>4} ");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacities_have_separate_rows_and_percentages_align() {
        let gib = 1024_u64.pow(3);
        let snapshot = Some(Snapshot {
            disk: Some((839 * gib, 1007 * gib)),
            ..Snapshot::default()
        });
        for width in [32, 35, 36, 80] {
            let text = rows(snapshot, width, true);
            assert_eq!(text.len(), 5);
            assert!(text[3].ends_with("83% "));
            assert_eq!(text[4], "  839.0 GiB / 1007.0 GiB");
            for index in [0, 1, 3] {
                assert_eq!(text[index].chars().count(), width);
            }
            assert!(text.iter().all(|line| line.chars().count() <= width));
        }
        let compact = rows(snapshot, 12, false);
        assert_eq!(compact.len(), 3);
        assert!(compact[2].ends_with("83% "));
    }

    #[test]
    fn progress_bars_show_current_values_without_history() {
        assert_eq!(progress_bar(Some(0.0), 4), "────");
        assert_eq!(progress_bar(Some(50.0), 4), "━━──");
        assert_eq!(progress_bar(Some(100.0), 4), "━━━━");
        assert_eq!(progress_bar(Some(120.0), 4), "━━━━");
        assert_eq!(progress_bar(None, 4), "····");
        assert_eq!(progress_bar(Some(f32::NAN), 4), "····");
        assert_eq!(progress_bar(Some(50.0), 0), "");
        let snapshot = Snapshot {
            cpu_percent: Some(50.0),
            disk: Some((3, 4)),
            ..Snapshot::default()
        };
        for width in [1, 12, 17, 35, 36, 80] {
            let text = rows_with_bars(Some(snapshot), width, true);
            assert!(text.iter().all(|line| line.chars().count() <= width));
            if width >= 17 {
                assert!(text[0].contains('━'));
            }
        }
        assert_eq!(rows_with_bars(None, 12, true), rows(None, 12, true));
    }

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
