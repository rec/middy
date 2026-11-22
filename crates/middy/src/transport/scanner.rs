//! One independent polling worker per active backend/direction/worker configuration.
use super::{subprocess::SubprocessOptions, *};
use std::sync::{atomic::AtomicBool, Condvar, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_INTERVAL: Duration = Duration::from_millis(100);

/// Set the process-wide device polling interval (default: 100 ms).
/// Applies to existing and future followers; schedules the next scan one interval
/// after this call. The interval must be positive and fit in u64 nanoseconds.
pub fn set_scan_interval(interval: Duration) -> Result<()> {
    ensure_control_thread()?;
    if interval.is_zero() {
        return Err(Error::Invalid("scan interval must be positive".into()));
    }
    if interval.as_nanos() > u128::from(u64::MAX) {
        return Err(Error::Invalid("scan interval is out of range".into()));
    }
    let next = Instant::now()
        .checked_add(interval)
        .ok_or_else(|| Error::Invalid("scan interval is out of range".into()))?;
    let mut registry = registry().lock().unwrap_or_else(|p| p.into_inner());
    registry.interval = interval;
    registry.entries.retain(|(_, weak)| {
        let Some(catalog) = weak.upgrade() else {
            return false;
        };
        let mut schedule = catalog
            .shared
            .schedule
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        schedule.interval = interval;
        schedule.next = next;
        catalog.shared.timer.notify_one();
        true
    });
    Ok(())
}

struct Registry {
    interval: Duration,
    entries: Vec<(Key, Weak<Catalog>)>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        Mutex::new(Registry {
            interval: DEFAULT_INTERVAL,
            entries: Vec::new(),
        })
    })
}

#[derive(Clone, PartialEq, Eq)]
struct Key {
    backend: Backend,
    input: bool,
    subprocess: Option<SubprocessOptions>,
}

#[derive(Clone)]
pub(crate) struct Snapshot {
    pub revision: u64,
    pub ports: Arc<Vec<PortKey>>,
}
impl Snapshot {
    pub fn selected(&self, patterns: Option<&[String]>) -> Result<Vec<PortKey>> {
        let mut seen = std::collections::HashSet::new();
        let mut selected = Vec::new();
        for port in self.ports.iter().filter(|p| {
            patterns.is_none_or(|patterns| {
                patterns
                    .iter()
                    .any(|pattern| crate::output::port_matches(pattern, &p.name))
            })
        }) {
            if !seen.insert(port) {
                return Err(port.ambiguous());
            }
            selected.push(port.clone());
        }
        Ok(selected)
    }
}

struct Schedule {
    interval: Duration,
    next: Instant,
    stopped: bool,
}

struct Shared {
    state: Mutex<Option<Result<Snapshot>>>,
    changed: Condvar,
    schedule: Mutex<Schedule>,
    timer: Condvar,
}
struct Catalog {
    shared: Arc<Shared>,
}
impl Drop for Catalog {
    fn drop(&mut self) {
        self.shared
            .schedule
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stopped = true;
        self.shared.timer.notify_one();
    }
}

#[derive(Clone)]
pub(crate) struct Subscription(Arc<Catalog>);
impl Subscription {
    pub fn new(
        backend: Backend,
        input: bool,
        subprocess: Option<SubprocessOptions>,
    ) -> Result<Self> {
        ensure_control_thread()?;
        let key = Key {
            backend,
            input,
            subprocess,
        };
        let mut registry = registry().lock().unwrap_or_else(|p| p.into_inner());
        registry
            .entries
            .retain(|(_, catalog)| catalog.strong_count() != 0);
        let existing = registry.entries.iter().find_map(|(k, weak)| {
            if k != &key {
                return None;
            }
            let catalog = weak.upgrade()?;
            let failed = matches!(
                *catalog
                    .shared
                    .state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()),
                Some(Err(_))
            );
            (!failed).then_some(catalog)
        });
        let catalog = if let Some(existing) = existing {
            existing
        } else {
            let shared = Arc::new(Shared {
                state: Mutex::new(None),
                changed: Condvar::new(),
                schedule: Mutex::new(Schedule {
                    interval: registry.interval,
                    next: Instant::now(),
                    stopped: false,
                }),
                timer: Condvar::new(),
            });
            let catalog = Arc::new(Catalog {
                shared: shared.clone(),
            });
            let worker_key = key.clone();
            thread::Builder::new()
                .name("middy-device-scanner".into())
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        poll(&worker_key, &shared)
                    }))
                    .unwrap_or_else(|_| Err(Error::Worker("device scanner panicked".into())));
                    if let Err(error) = result {
                        *shared.state.lock().unwrap_or_else(|p| p.into_inner()) = Some(Err(error));
                        shared.changed.notify_all();
                    }
                })
                .map_err(|e| Error::Worker(e.to_string()))?;
            registry.entries.push((key, Arc::downgrade(&catalog)));
            catalog
        };
        drop(registry);
        let subscription = Self(catalog);
        subscription.snapshot()?;
        Ok(subscription)
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        let shared = &self.0.shared;
        let state = shared.state.lock().unwrap_or_else(|p| p.into_inner());
        shared
            .changed
            .wait_while(state, |s| s.is_none())
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .unwrap()
            .clone()
    }
    pub fn wait(&self, ports: &Arc<Vec<PortKey>>, stopped: &AtomicBool) {
        let shared = &self.0.shared;
        let state = shared.state.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(&*state, Some(Ok(s)) if Arc::ptr_eq(&s.ports, ports))
            && !stopped.load(Ordering::Acquire)
        {
            drop(
                shared
                    .changed
                    .wait_timeout(state, Duration::from_millis(100))
                    .unwrap_or_else(|p| p.into_inner()),
            );
        }
    }
    pub fn wake(&self) {
        let _state = self
            .0
            .shared
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        self.0.shared.changed.notify_all();
    }
}

fn poll(key: &Key, shared: &Shared) -> Result<()> {
    let client = Client::new(key)?;
    poll_ports(shared, || client.ports())
}

fn poll_ports(shared: &Shared, mut ports: impl FnMut() -> Result<Vec<PortKey>>) -> Result<()> {
    let mut deadline = shared
        .schedule
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .next;
    let mut initialized = false;
    let mut scan_failed = false;
    loop {
        if let Some(ports) = match ports() {
            Ok(ports) => {
                initialized = true;
                scan_failed = false;
                Some(ports)
            }
            Err(error @ (Error::Native(_) | Error::Busy)) if initialized => {
                if !scan_failed {
                    eprintln!("middy: MIDI device scan failed; keeping the last catalog: {error}");
                }
                scan_failed = true;
                None
            }
            Err(error) => return Err(error),
        } {
            let mut state = shared.state.lock().unwrap_or_else(|p| p.into_inner());
            let revision = match state.as_ref() {
                Some(Ok(previous)) => previous.revision.wrapping_add(1),
                _ => 0,
            };
            if let Some(Ok(previous)) = state.as_mut() {
                if *previous.ports == ports {
                    // An open that raced a disappearing endpoint can retry after this scan.
                    previous.revision = revision;
                } else {
                    *previous = Snapshot {
                        revision,
                        ports: Arc::new(ports),
                    };
                    shared.changed.notify_all();
                }
            } else {
                *state = Some(Ok(Snapshot {
                    revision,
                    ports: Arc::new(ports),
                }));
                shared.changed.notify_all();
            }
        }
        let mut schedule = shared.schedule.lock().unwrap_or_else(|p| p.into_inner());
        if schedule.next == deadline {
            schedule.next = Instant::now()
                .checked_add(schedule.interval)
                .ok_or_else(|| Error::Invalid("scan interval is out of range".into()))?;
        }
        while !schedule.stopped {
            let remaining = schedule.next.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            schedule = shared
                .timer
                .wait_timeout(schedule, remaining)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        deadline = schedule.next;
        if schedule.stopped {
            return Ok(());
        }
    }
}
enum Client {
    Input(Input),
    Output(Output),
}
impl Client {
    fn new(key: &Key) -> Result<Self> {
        if key.input {
            let options = InputOptions {
                backend: Some(key.backend),
                encoding: key.backend.encoding(),
                ..InputOptions::default()
            };
            match &key.subprocess {
                Some(worker) => Input::new_subprocess(options, worker.clone()),
                None => Input::new(options),
            }
            .map(Self::Input)
        } else {
            let options = OutputOptions {
                backend: Some(key.backend),
                encoding: key.backend.encoding(),
            };
            match &key.subprocess {
                Some(worker) => Output::new_subprocess(options, worker.clone()),
                None => Output::new(options),
            }
            .map(Self::Output)
        }
    }
    fn ports(&self) -> Result<Vec<PortKey>> {
        let ports = match self {
            Self::Input(client) => client.ports(),
            Self::Output(client) => client.ports(),
        }?;
        Ok(port_keys(&ports))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_resumes_after_native_enumeration_error() {
        let shared = Shared {
            state: Mutex::new(None),
            changed: Condvar::new(),
            schedule: Mutex::new(Schedule {
                interval: Duration::ZERO,
                next: Instant::now(),
                stopped: false,
            }),
            timer: Condvar::new(),
        };
        let first = PortKey {
            id: 1,
            name: "first".into(),
        };
        let second = PortKey {
            id: 2,
            name: "second".into(),
        };
        let mut scans = 0;
        let result = poll_ports(&shared, || {
            scans += 1;
            match scans {
                1 => Ok(vec![first.clone()]),
                2 => Err(Error::Native("temporary enumeration failure".into())),
                3 => {
                    let state = shared.state.lock().unwrap();
                    assert_eq!(
                        state.as_ref().unwrap().as_ref().unwrap().ports.as_slice(),
                        std::slice::from_ref(&first)
                    );
                    Ok(vec![second.clone()])
                }
                _ => Err(Error::Closed),
            }
        });
        assert_eq!(result, Err(Error::Closed));
        let snapshot = shared.state.into_inner().unwrap().unwrap().unwrap();
        assert_eq!(*snapshot.ports, [second]);
        assert_eq!(snapshot.revision, 1);
    }
}
