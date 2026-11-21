//! Shared hot-plug subscription used by the Rust input and Python facade.
use super::*;
use crate::{events, live};
use std::sync::{atomic::AtomicBool, mpsc};
use std::thread::{self, JoinHandle};

pub enum AllItem {
    Decoded(live::Item),
    Raw(live::Item<ReceivedPacket>, events::Clock),
}
type Delivery = Arc<Mutex<Box<dyn FnMut(AllItem) + Send>>>;

struct Topology<C> {
    sources: Vec<(PortKey, C)>,
}
impl<C> Topology<C> {
    fn update(
        &mut self,
        visible: Vec<PortKey>,
        mut open: impl FnMut(&PortKey) -> Result<Option<C>>,
    ) -> Result<Vec<C>> {
        for key in &visible {
            if !self.sources.iter().any(|(k, _)| k == key) {
                if let Some(source) = open(key)? {
                    self.sources.push((key.clone(), source));
                }
            }
        }
        let mut removed = Vec::new();
        let mut i = 0;
        while i < self.sources.len() {
            if visible.contains(&self.sources[i].0) {
                i += 1;
            } else {
                removed.push(self.sources.remove(i).1);
            }
        }
        Ok(removed)
    }
}
struct Source {
    pending: Arc<std::sync::atomic::AtomicUsize>,
    connection: InputConnection,
    decoder: Arc<Mutex<SourceDecoder>>,
    stream: String,
    clock: events::Clock,
    raw: bool,
    delivery: Delivery,
}
struct SourceDecoder {
    decoder: live::Decoder,
    expected: u64,
    reported_loss: u64,
}
impl Source {
    fn close(self, disconnected: bool) -> Result<()> {
        let diagnostics = self.connection.diagnostics();
        let result = self.connection.close().map(drop);
        let mut decoder = self.decoder.lock().unwrap_or_else(|p| p.into_inner());
        if decoder.decoder.pending() {
            self.pending.fetch_sub(1, Ordering::AcqRel);
        }
        let remaining = diagnostics
            .dropped_packets()
            .saturating_sub(decoder.reported_loss);
        if remaining > 0 {
            let value = decoder
                .decoder
                .loss(remaining, "native receive dropped packets".into());
            let mut callback = self.delivery.lock().unwrap_or_else(|p| p.into_inner());
            if self.raw {
                if let live::Item::Diagnostic(value) = value {
                    callback(AllItem::Raw(
                        live::Item::Diagnostic(value),
                        self.clock.clone(),
                    ));
                }
            } else {
                callback(AllItem::Decoded(value));
            }
        }
        if !self.raw {
            for item in decoder.decoder.finish() {
                (self.delivery.lock().unwrap_or_else(|p| p.into_inner()))(AllItem::Decoded(item));
            }
        }
        if disconnected {
            let diagnostic = live::Diagnostic {
                stream: self.stream,
                packet: 0,
                kind: live::DiagnosticKind::ClockChange,
                lost: 0,
                reason: "MIDI input port disconnected".into(),
            };
            let mut callback = self.delivery.lock().unwrap_or_else(|p| p.into_inner());
            if self.raw {
                callback(AllItem::Raw(live::Item::Diagnostic(diagnostic), self.clock));
            } else {
                callback(AllItem::Decoded(live::Item::Diagnostic(diagnostic)));
            }
        }
        result
    }
}
struct DiscoveryGuard(Arc<FailureState>);
impl Drop for DiscoveryGuard {
    fn drop(&mut self) {
        if thread::panicking() {
            self.0
                .error
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get_or_insert_with(|| Error::Worker("input discovery thread panicked".into()));
        }
    }
}

/// Follows the shared scanner and owns a connection per visible input. Explicit
/// close joins the follower and drains accepted deliveries before returning the client.
pub struct AllInputConnection {
    pending: Arc<std::sync::atomic::AtomicUsize>,
    activity: Arc<Mutex<Option<std::time::Instant>>>,
    stop: Arc<AtomicBool>,
    scanner: scanner::Subscription,
    thread: Option<JoinHandle<Result<Input>>>,
    failure: Arc<FailureState>,
    clock: Clock,
    dropped: Arc<AtomicU64>,
}
#[derive(Clone)]
pub struct AllDiagnostics {
    pending: Arc<std::sync::atomic::AtomicUsize>,
    failure: Arc<FailureState>,
    dropped: Arc<AtomicU64>,
    activity: Arc<Mutex<Option<std::time::Instant>>>,
}
impl AllDiagnostics {
    pub fn pending(&self) -> bool {
        self.pending.load(Ordering::Acquire) != 0
    }
    pub fn last_message_time(&self) -> Option<std::time::Instant> {
        *self.activity.lock().unwrap_or_else(|p| p.into_inner())
    }
    pub fn worker_failure(&self) -> Option<Error> {
        self.failure
            .error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn dropped_packets(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}
impl AllInputConnection {
    pub fn diagnostics(&self) -> AllDiagnostics {
        AllDiagnostics {
            pending: Arc::clone(&self.pending),
            failure: Arc::clone(&self.failure),
            dropped: Arc::clone(&self.dropped),
            activity: Arc::clone(&self.activity),
        }
    }
    pub fn clock(&self) -> Clock {
        self.clock
    }
    pub fn worker_failure(&self) -> Option<Error> {
        self.failure
            .error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn dropped_packets(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
    pub fn close(mut self) -> Result<Input> {
        ensure_control_thread()?;
        self.signal_stop();
        self.thread
            .take()
            .expect("active port discovery")
            .join()
            .map_err(|_| Error::Worker("input discovery thread panicked".into()))?
    }
    fn signal_stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.scanner.wake();
    }
}
impl Drop for AllInputConnection {
    fn drop(&mut self) {
        self.signal_stop();
    }
}
impl Input {
    pub fn connect_all(
        self,
        stream: String,
        sysex_limit: usize,
        raw: bool,
        subprocess: Option<subprocess::SubprocessOptions>,
        patterns: Option<Vec<String>>,
        callback: impl FnMut(AllItem) + Send + 'static,
    ) -> Result<AllInputConnection> {
        ensure_control_thread()?;
        if patterns
            .as_ref()
            .is_some_and(|p| p.iter().any(String::is_empty))
        {
            return Err(Error::Invalid("port pattern must not be empty".into()));
        }
        if stream.is_empty() || sysex_limit == 0 {
            return Err(Error::Invalid(
                "source and SysEx limit must be nonempty".into(),
            ));
        }
        let options = InputOptions {
            backend: Some(self.clock.backend),
            encoding: self.clock.backend.encoding(),
            timestamps: self.clock.mode,
            receive_byte_limit: self.limit,
        };
        let clock = self.clock;
        let scanner = scanner::Subscription::new(clock.backend, true, subprocess.clone())?;
        let discovery = scanner.clone();
        let failure = Arc::clone(&self.failure);
        let thread_failure = Arc::clone(&failure);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let dropped = Arc::new(AtomicU64::new(0));
        let loss = Arc::clone(&dropped);
        let delivery: Delivery = Arc::new(Mutex::new(Box::new(callback)));
        let activity = Arc::new(Mutex::new(None));
        let pending = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pending_sources = Arc::clone(&pending);
        let last_message = Arc::clone(&activity);
        let (ready, started) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("middy-input-follower".into())
            .spawn(move || {
                let _guard = DiscoveryGuard(Arc::clone(&thread_failure));
                let mut topology = Topology {
                    sources: Vec::<(PortKey, Source)>::new(),
                };
                let mut initialized = false;
                let mut retired_loss = 0u64;
                let result = (|| {
                    loop {
                        if stopped.load(Ordering::Acquire) {
                            break;
                        }
                        let snapshot = discovery.snapshot()?;
                        let visible = snapshot
                            .ports
                            .iter()
                            .filter(|p| {
                                patterns.as_ref().is_none_or(|patterns| {
                                    patterns
                                        .iter()
                                        .any(|s| crate::output::port_matches(s, &p.name))
                                })
                            })
                            .cloned()
                            .collect();
                        let removed = topology.update(visible, |key| {
                            let client = if let Some(subprocess) = &subprocess {
                                Input::new_subprocess(options, subprocess.clone())?
                            } else {
                                Input::new(options)?
                            };
                            let ports = client.ports()?;
                            let Some(port) = ports
                                .iter()
                                .filter(|p| p.name() == key.name && p.backend_id() == key.id)
                                .nth(key.occurrence)
                            else {
                                return Ok(None);
                            };
                            let info = events::PortInfo::new(
                                port.name().into(),
                                port.backend(),
                                port.backend_id(),
                            )
                            .map_err(|e| Error::Resource(e.to_string()))?;
                            let source_stream = format!("{stream}/{}", info.identity);
                            let enabled = options.timestamps == TimestampMode::Absolute;
                            let clock = events::Clock::new(
                                format!("{source_stream}/clock"),
                                1,
                                1_000_000_000,
                                if enabled {
                                    "backend_absolute"
                                } else {
                                    "disabled"
                                }
                                .into(),
                                None,
                            )
                            .map_err(|e| Error::Invalid(e.to_string()))?
                            .with_transport(options.backend.unwrap() as i32, enabled);
                            let decoder = Arc::new(Mutex::new(SourceDecoder {
                                decoder: live::Decoder::new(
                                    source_stream.clone(),
                                    options.encoding,
                                    sysex_limit,
                                )
                                .map_err(|e| Error::Invalid(e.to_string()))?
                                .with_port(info),
                                expected: 0,
                                reported_loss: 0,
                            }));
                            let decode = Arc::clone(&decoder);
                            let deliver = Arc::clone(&delivery);
                            let source_clock = clock.clone();
                            let last_message = Arc::clone(&last_message);
                            let pending = Arc::clone(&pending_sources);
                            let callback_stream = source_stream.clone();
                            let connection = client.connect(port, move |packet| {
                                *last_message.lock().unwrap_or_else(|p| p.into_inner()) =
                                    Some(std::time::Instant::now());
                                let mut decoder = decode.lock().unwrap_or_else(|p| p.into_inner());
                                let gap = packet.sequence.saturating_sub(decoder.expected);
                                decoder.reported_loss = decoder.reported_loss.saturating_add(gap);
                                decoder.expected = packet.sequence.saturating_add(1);
                                if raw {
                                    if gap > 0 {
                                        (deliver.lock().unwrap_or_else(|p| p.into_inner()))(
                                            AllItem::Raw(
                                                live::Item::Diagnostic(live::Diagnostic {
                                                    stream: callback_stream.clone(),
                                                    packet: packet.sequence,
                                                    kind: live::DiagnosticKind::Loss,
                                                    lost: gap,
                                                    reason: "packet sequence gap".into(),
                                                }),
                                                source_clock.clone(),
                                            ),
                                        );
                                    }
                                    (deliver.lock().unwrap_or_else(|p| p.into_inner()))(
                                        AllItem::Raw(
                                            live::Item::Message(Box::new(packet)),
                                            source_clock.clone(),
                                        ),
                                    );
                                } else {
                                    let before = decoder.decoder.pending();
                                    let values = decoder.decoder.feed(
                                        &packet.packet,
                                        &source_clock,
                                        packet.timestamp_ns,
                                        Some(packet.sequence),
                                    );
                                    let mut callback =
                                        deliver.lock().unwrap_or_else(|p| p.into_inner());
                                    match values {
                                        Ok(values) => {
                                            for value in values {
                                                callback(AllItem::Decoded(value));
                                            }
                                        }
                                        Err(error) => callback(AllItem::Decoded(
                                            live::Item::Diagnostic(live::Diagnostic {
                                                stream: callback_stream.clone(),
                                                packet: packet.sequence,
                                                kind: live::DiagnosticKind::Malformed,
                                                lost: 0,
                                                reason: error.to_string(),
                                            }),
                                        )),
                                    }
                                    let after = decoder.decoder.pending();
                                    if after && !before {
                                        pending.fetch_add(1, Ordering::AcqRel);
                                    } else if before && !after {
                                        pending.fetch_sub(1, Ordering::AcqRel);
                                    }
                                }
                            });
                            let connection = match connection {
                                Ok(connection) => connection,
                                Err(_) if !port_keys(&self.ports()?).contains(key) => {
                                    return Ok(None)
                                }
                                Err(error) => return Err(error),
                            };
                            Ok(Some(Source {
                                pending: Arc::clone(&pending_sources),
                                connection,
                                decoder,
                                stream: source_stream,
                                clock,
                                raw,
                                delivery: Arc::clone(&delivery),
                            }))
                        })?;
                        let mut retirement = Ok(());
                        for source in removed {
                            let diagnostics = source.connection.diagnostics();
                            retirement = retirement.and(source.close(true));
                            retired_loss =
                                retired_loss.saturating_add(diagnostics.dropped_packets());
                        }
                        retirement?;
                        let mut count = retired_loss;
                        for (_, source) in &topology.sources {
                            source.connection.check_health()?;
                            count = count.saturating_add(source.connection.dropped_packets());
                        }
                        loss.store(count, Ordering::Relaxed);
                        if !initialized {
                            let _ = ready.send(Ok(()));
                            initialized = true;
                        }
                        discovery.wait(snapshot.revision, &stopped);
                    }
                    Ok(())
                })();
                if !initialized {
                    let _ = ready.send(result.clone());
                }
                let mut result = result;
                for (_, source) in topology.sources {
                    let diagnostics = source.connection.diagnostics();
                    if let Err(error) = source.close(false) {
                        if result.is_ok() {
                            result = Err(error);
                        }
                    }
                    retired_loss = retired_loss.saturating_add(diagnostics.dropped_packets());
                }
                loss.store(retired_loss, Ordering::Relaxed);
                if let Err(error) = &result {
                    *thread_failure
                        .operation
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()) = "follow MIDI inputs".into();
                    thread_failure
                        .error
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .get_or_insert_with(|| error.clone());
                }
                result.map(|_| self)
            })
            .map_err(|e| Error::Worker(e.to_string()))?;
        match started
            .recv()
            .map_err(|_| Error::Worker("input discovery stopped during startup".into()))?
        {
            Ok(()) => Ok(AllInputConnection {
                pending,
                stop,
                scanner,
                thread: Some(thread),
                failure,
                clock,
                dropped,
                activity,
            }),
            Err(error) => {
                let _ = thread.join();
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_attaches_new_ports_and_retires_only_removed_connections() {
        let a = PortKey {
            name: "Keyboard".into(),
            id: 1,
            occurrence: 0,
        };
        let b = PortKey {
            name: "Keyboard".into(),
            id: 1,
            occurrence: 1,
        };
        let mut topology = Topology {
            sources: Vec::new(),
        };
        let mut serial = 0;
        let mut open = |_: &PortKey| {
            serial += 1;
            Ok(Some(serial))
        };
        assert!(topology.update(Vec::new(), &mut open).unwrap().is_empty());
        assert!(topology
            .update(vec![a.clone()], &mut open)
            .unwrap()
            .is_empty());
        assert!(topology
            .update(vec![a.clone(), b.clone()], &mut open)
            .unwrap()
            .is_empty());
        assert_eq!(
            topology
                .sources
                .iter()
                .map(|(_, id)| *id)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(topology.update(vec![b.clone()], &mut open).unwrap(), [1]);
        assert_eq!(topology.update(vec![a, b], &mut open).unwrap(), []);
        assert_eq!(
            topology
                .sources
                .iter()
                .map(|(_, id)| *id)
                .collect::<Vec<_>>(),
            [2, 3]
        );
    }
}
