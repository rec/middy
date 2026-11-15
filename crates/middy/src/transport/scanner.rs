//! One process-wide polling loop, with one catalog per active backend/direction/worker.
use super::{subprocess::SubprocessOptions, *};
use std::sync::{atomic::AtomicBool, mpsc, Condvar, OnceLock};
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
    let (reply, response) = mpsc::sync_channel(1);
    sender()?
        .send(Request::Interval { interval, reply })
        .map_err(|_| Error::Worker("device scanner stopped".into()))?;
    response
        .recv()
        .map_err(|_| Error::Worker("device scanner stopped".into()))?
}

fn sender() -> Result<&'static mpsc::Sender<Request>> {
    static SCANNER: OnceLock<Result<mpsc::Sender<Request>>> = OnceLock::new();
    SCANNER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel();
            thread::Builder::new()
                .name("middy-device-scanner".into())
                .spawn(move || run(receiver))
                .map_err(|e| Error::Worker(e.to_string()))?;
            Ok(sender)
        })
        .as_ref()
        .map_err(Clone::clone)
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
struct Catalog {
    state: Mutex<Result<Snapshot>>,
    changed: Condvar,
    client: Mutex<Option<Client>>,
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
        let (reply, response) = mpsc::sync_channel(1);
        sender()?
            .send(Request::Subscribe {
                key: Key {
                    backend,
                    input,
                    subprocess,
                },
                reply,
            })
            .map_err(|_| Error::Worker("device scanner stopped".into()))?;
        response
            .recv()
            .map_err(|_| Error::Worker("device scanner stopped".into()))?
    }
    pub fn snapshot(&self) -> Result<Snapshot> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn wait(&self, revision: u64, stopped: &AtomicBool) {
        let state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(&*state, Ok(s) if s.revision == revision) && !stopped.load(Ordering::Acquire) {
            drop(
                self.0
                    .changed
                    // Keep health checks responsive even with slow device polling.
                    .wait_timeout(state, Duration::from_millis(100))
                    .unwrap_or_else(|p| p.into_inner()),
            );
        }
    }
    pub fn wake(&self) {
        // Pair notification with the waiter's lock so close cannot lose a wakeup.
        let _state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        self.0.changed.notify_all();
    }
}
enum Request {
    Subscribe {
        key: Key,
        reply: mpsc::SyncSender<Result<Subscription>>,
    },
    Interval {
        interval: Duration,
        reply: mpsc::SyncSender<Result<()>>,
    },
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
struct Entry {
    key: Key,
    catalog: Weak<Catalog>,
}
impl Entry {
    fn new(key: Key) -> Result<(Self, Subscription)> {
        let client = Client::new(&key)?;
        let catalog = Arc::new(Catalog {
            state: Mutex::new(Ok(Snapshot {
                revision: 0,
                ports: Arc::new(client.ports()?),
            })),
            changed: Condvar::new(),
            client: Mutex::new(Some(client)),
        });
        Ok((
            Self {
                key,
                catalog: Arc::downgrade(&catalog),
            },
            Subscription(catalog),
        ))
    }
    fn scan(&mut self) {
        let Some(catalog) = self.catalog.upgrade() else {
            return;
        };
        let mut client = catalog.client.lock().unwrap_or_else(|p| p.into_inner());
        let Some(discovery) = &*client else {
            return;
        };
        let result = discovery.ports();
        let mut state = catalog.state.lock().unwrap_or_else(|p| p.into_inner());
        match result {
            Ok(ports) => {
                if let Ok(snapshot) = &mut *state {
                    snapshot.revision += 1;
                    if *snapshot.ports != ports {
                        snapshot.ports = Arc::new(ports);
                    }
                    catalog.changed.notify_all();
                }
            }
            Err(error) => {
                *state = Err(error);
                catalog.changed.notify_all();
                *client = None;
            }
        }
    }
}
impl Drop for Entry {
    fn drop(&mut self) {
        if let Some(catalog) = self.catalog.upgrade() {
            let mut state = catalog.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.is_ok() {
                *state = Err(Error::Worker("device scanner stopped".into()));
            }
            catalog.changed.notify_all();
        }
    }
}
fn run(receiver: mpsc::Receiver<Request>) {
    let mut entries: Vec<Entry> = Vec::new();
    let mut interval = DEFAULT_INTERVAL;
    let mut next = Instant::now() + interval;
    loop {
        entries.retain(|e| e.catalog.strong_count() != 0);
        if Instant::now() >= next {
            for entry in &mut entries {
                entry.scan();
            }
            next = Instant::now() + interval;
        }
        let request = if entries.is_empty() {
            match receiver.recv() {
                Ok(request) => request,
                Err(_) => return,
            }
        } else {
            match receiver.recv_timeout(next.saturating_duration_since(Instant::now())) {
                Ok(request) => request,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        };
        let (key, reply) = match request {
            Request::Interval {
                interval: value,
                reply,
            } => {
                let result = Instant::now()
                    .checked_add(value)
                    .ok_or_else(|| Error::Invalid("scan interval is out of range".into()))
                    .map(|deadline| {
                        interval = value;
                        next = deadline;
                    });
                let _ = reply.send(result);
                continue;
            }
            Request::Subscribe { key, reply } => (key, reply),
        };
        let existing = entries
            .iter()
            .find(|e| e.key == key)
            .and_then(|e| e.catalog.upgrade());
        let first = entries.is_empty();
        let result = if let Some(catalog) = existing {
            Ok(Subscription(catalog))
        } else {
            Entry::new(key).map(|(entry, subscription)| {
                entries.push(entry);
                subscription
            })
        };
        let _ = reply.send(result);
        if first {
            next = Instant::now() + interval;
        }
    }
}
