use crate::callback::check_blocking_thread;
use crate::errors::{midi_error, native_call, transport_error, StateError};
use crate::lifecycle::{self, Managed, Resource};
use crate::packets::{extract_packet, Backend, Encoding};
use crate::ports::Port;
use crate::state::Client;
use middy::transport;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use pyo3::{PyTraverseError, PyVisit};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Binding to the same confirmed-output tracker used by Rust's Output.
#[pyclass(module = "middy._native")]
pub(crate) struct OutputState {
    value: middy::midi::OutputState,
}
#[pymethods]
impl OutputState {
    #[new]
    fn new() -> Self {
        Self {
            value: Default::default(),
        }
    }
    fn feed(&mut self, packet: &[u8]) -> PyResult<()> {
        self.value.feed(packet).map_err(crate::files::os_error)
    }
    fn cleanup(&mut self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        std::mem::take(&mut self.value)
            .cleanup()
            .map(|m| crate::messages::to_python(py, m))
            .collect()
    }
}

#[pyclass(frozen, module = "middy._native")]
pub struct Output {
    subprocess: Option<transport::subprocess::SubprocessOptions>,
    state: Arc<Client<transport::Output>>,
    identity: String,
    diagnostics: transport::WorkerDiagnostics,
    snapshot: AtomicU64,
    encoding: Encoding,
}
#[pymethods]
impl Output {
    #[new]
    #[pyo3(signature = (*, identity, encoding=Encoding::Bytes, backend=None, worker_program=None, worker_args=None, worker_timeout=5.0))]
    fn new(
        py: Python<'_>,
        identity: String,
        encoding: Encoding,
        backend: Option<Backend>,
        worker_program: Option<std::path::PathBuf>,
        worker_args: Option<Vec<std::ffi::OsString>>,
        worker_timeout: f64,
    ) -> PyResult<Self> {
        check_blocking_thread()?;
        lifecycle::initialize(py)?;
        let subprocess = worker_program
            .map(|program| -> PyResult<_> {
                Ok(transport::subprocess::SubprocessOptions {
                    program,
                    args: worker_args.unwrap_or_default(),
                    timeout: std::time::Duration::try_from_secs_f64(worker_timeout).map_err(
                        |_| PyValueError::new_err("worker timeout must be finite and nonnegative"),
                    )?,
                })
            })
            .transpose()?;
        let native = py
            .detach(|| {
                native_call("create output", || {
                    let options = transport::OutputOptions {
                        encoding: encoding.native(),
                        backend: backend.map(Backend::native),
                    };
                    if let Some(subprocess) = &subprocess {
                        transport::Output::new_subprocess(options, subprocess.clone())
                    } else {
                        transport::Output::new(options)
                    }
                })
            })?
            .map_err(|e| transport_error(e, "init"))?;
        let diagnostics = native.worker_diagnostics();
        Ok(Self {
            subprocess,
            diagnostics,
            state: Arc::new(Client::new(native)),
            identity,
            snapshot: AtomicU64::new(0),
            encoding,
        })
    }
    #[getter]
    fn worker_failure(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.diagnostics.failure().map(|e| {
            transport_error(e, &self.diagnostics.operation())
                .into_value(py)
                .into_any()
        })
    }
    fn ports(&self, py: Python<'_>) -> PyResult<Vec<Port>> {
        py.detach(|| {
            self.state.with_available(|native| {
                let ports = native.ports().map_err(|e| transport_error(e, "ports"))?;
                let snapshot = self.snapshot.fetch_add(1, Ordering::Relaxed) + 1;
                Ok(ports
                    .into_iter()
                    .enumerate()
                    .map(|(index, native)| {
                        Port::new(native, &self.identity, snapshot, index, false)
                    })
                    .collect())
            })
        })
    }
    fn connect(slf: Py<Self>, py: Python<'_>, port: &Port) -> PyResult<Py<OutputConnection>> {
        let port = port.native.clone();
        OutputConnection::open(py, slf, move |native| match port {
            Some(port) => native
                .connect(&port)
                .map(SenderPort::Single)
                .map_err(middy::midi::Error::Connect),
            None => Err(middy::midi::Error::Connect(transport::Error::Invalid(
                "port is not a native snapshot from this client".into(),
            ))),
        })
    }
    fn connect_matching(
        slf: Py<Self>,
        py: Python<'_>,
        patterns: Vec<String>,
    ) -> PyResult<Py<OutputConnection>> {
        let subprocess = slf.get().subprocess.clone();
        OutputConnection::open(py, slf, move |native| {
            middy::midi::OutputGroup::new(native, patterns, subprocess)
                .map(|group| SenderPort::Group(Box::new(group)))
        })
    }
    fn create_virtual(
        slf: Py<Self>,
        py: Python<'_>,
        name: String,
    ) -> PyResult<Py<OutputConnection>> {
        OutputConnection::open(py, slf, move |native| {
            native
                .create_virtual(&name)
                .map(SenderPort::Single)
                .map_err(middy::midi::Error::Connect)
        })
    }
}
#[pyclass(frozen, weakref, module = "middy._native")]
pub struct OutputConnection {
    resource: Arc<Managed<transport::Output, Sender>>,
    client: Py<Output>,
    encoding: Encoding,
    failure: std::sync::Mutex<Option<Arc<middy::midi::Error>>>,
}
struct Sender {
    port: SenderPort,
    mpe: Option<middy::mpe::PlaybackState>,
}

enum SenderPort {
    Single(transport::OutputConnection),
    Group(Box<middy::midi::OutputGroup>),
}
enum PreparedWire {
    Packet(transport::Packet),
    Batch(Vec<Vec<u8>>),
}
impl SenderPort {
    fn send_failure_is_terminal(&self) -> bool {
        match self {
            Self::Single(_) => true,
            Self::Group(group) => group.failure().is_some(),
        }
    }

    fn send(&mut self, packet: &transport::Packet) -> middy::midi::Result<()> {
        match self {
            Self::Single(c) => c.send(packet).map_err(middy::midi::Error::Send),
            Self::Group(c) => c.send(packet),
        }
    }
    fn send_byte_batch(&mut self, packets: &[Vec<u8>]) -> middy::midi::Result<()> {
        match self {
            Self::Single(c) => c.send_byte_batch(packets).map_err(middy::midi::Error::Send),
            Self::Group(c) => c.send_byte_batch(packets),
        }
    }
    fn close(self) -> middy::midi::Result<transport::Output> {
        match self {
            Self::Single(c) => c.close().map_err(middy::midi::Error::Transport),
            Self::Group(c) => c.close(),
        }
    }
}
impl Sender {
    fn send(&mut self, packet: &transport::Packet) -> middy::midi::Result<()> {
        self.port.send(packet)
    }

    fn send_mpe(
        &mut self,
        message: &middy::events::Payload,
        wire: &PreparedWire,
        start: Option<u8>,
        end: bool,
        encoding: transport::Encoding,
    ) -> middy::midi::Result<()> {
        if let Some(members) = start {
            if let SenderPort::Group(group) = &mut self.port {
                group.pin()?;
            }
            self.mpe = Some(middy::mpe::PlaybackState::new(members, encoding));
        }
        if let Some(state) = &mut self.mpe {
            state.attempt(message);
        }
        match wire {
            PreparedWire::Batch(packets) => self.port.send_byte_batch(packets)?,
            PreparedWire::Packet(packet) => self.send(packet)?,
        }
        if end {
            self.mpe = None;
        } else if let Some(state) = &mut self.mpe {
            state.confirm(message);
        }
        Ok(())
    }

    fn finish_mpe(&mut self) -> middy::midi::Result<()> {
        let result = self.mpe.take().map_or(Ok(()), |state| {
            let packets = state.cleanup()?;
            match &mut self.port {
                SenderPort::Group(group) => group.cleanup_pinned(&packets),
                SenderPort::Single(connection) => {
                    let mut failure = None;
                    for packet in &packets {
                        if let Err(error) = connection.send(packet) {
                            failure.get_or_insert(middy::midi::Error::Send(error));
                        }
                    }
                    failure.map_or(Ok(()), Err)
                }
            }
        });
        if let SenderPort::Group(group) = &mut self.port {
            group.unpin();
        }
        result
    }

    fn close(mut self) -> middy::midi::Result<transport::Output> {
        let result = self.finish_mpe();
        let closed = self.port.close();
        result.and(closed)
    }
}
#[pymethods]
impl OutputConnection {
    #[getter]
    fn worker_failure(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        let failure = self
            .failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(error) = failure {
            return Some(midi_error(&error).into_value(py).into_any());
        }
        self.client.get().diagnostics.failure().map(|e| {
            transport_error(e, &self.client.get().diagnostics.operation())
                .into_value(py)
                .into_any()
        })
    }

    fn send_byte_batch(&self, py: Python<'_>, packets: Vec<Vec<u8>>) -> PyResult<()> {
        check_blocking_thread()?;
        self.send_value(py, &packets, |native, packets| {
            native.port.send_byte_batch(packets)
        })
    }

    fn send(&self, py: Python<'_>, packet: &Bound<'_, PyAny>) -> PyResult<()> {
        check_blocking_thread()?;
        let packet = extract_packet(packet)?;
        transport::validate_packet(self.encoding.native(), &packet)
            .map_err(|e| transport_error(e, "send"))?;
        self.send_value(py, &packet, Sender::send)
    }
    fn send_mpe(
        &self,
        py: Python<'_>,
        message: &Bound<'_, PyAny>,
        prepared: &Bound<'_, PyAny>,
        start: Option<u8>,
        end: bool,
    ) -> PyResult<()> {
        check_blocking_thread()?;
        let payload = crate::event_values::payload(message)?;
        let wire = if prepared.is_instance_of::<PyTuple>() {
            PreparedWire::Batch(prepared.extract()?)
        } else {
            PreparedWire::Packet(extract_packet(prepared)?)
        };
        let encoding = self.encoding.native();
        self.send_value(py, &wire, move |native, wire| {
            native.send_mpe(&payload, wire, start, end, encoding)
        })
    }

    fn finish_mpe(&self, py: Python<'_>) -> PyResult<()> {
        let mut outcome = Ok(());
        self.resource
            .native
            .cleanup(&self.resource.client, py, |native| {
                outcome = native.finish_mpe();
            })?;
        outcome.map_err(|error| midi_error(&error))
    }
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        check_blocking_thread()?;
        py.detach(|| self.resource.close())
    }
    #[getter]
    fn closed(&self) -> bool {
        self.resource.native.closed() || self.client.get().diagnostics.failure().is_some()
    }
    fn __enter__(slf: Py<Self>) -> PyResult<Py<Self>> {
        slf.get().resource.native.check_failed()?;
        if slf.get().closed() {
            return Err(StateError::new_err("MIDI connection is closed"));
        }
        Ok(slf)
    }
    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: &Bound<'_, PyAny>,
        _exc_value: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        self.close(py)
    }
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.client)
    }
    fn __clear__(&self) {
        lifecycle::defer(self.resource.clone());
    }
}
impl OutputConnection {
    fn send_value<P: Sync>(
        &self,
        py: Python<'_>,
        value: &P,
        send: impl FnOnce(&mut Sender, &P) -> middy::midi::Result<()> + Send,
    ) -> PyResult<()> {
        let mut failure = None;
        let result =
            self.resource
                .native
                .send(&self.resource.client, py, value, |native, value| {
                    if let Err(error) = send(native, value) {
                        let terminal = native.port.send_failure_is_terminal();
                        failure = Some((error, terminal));
                        if terminal {
                            return Err(crate::errors::SendError::new_err("MIDI send failed"));
                        }
                    }
                    Ok(())
                });
        // Build Python progress exceptions after releasing the connection lock.
        match failure {
            Some((error, terminal)) => {
                let error = Arc::new(error);
                if terminal {
                    *self.failure.lock().unwrap_or_else(|p| p.into_inner()) =
                        Some(Arc::clone(&error));
                }
                Err(midi_error(&error))
            }
            None => result,
        }
    }

    fn open(
        py: Python<'_>,
        client: Py<Output>,
        operation: impl FnOnce(transport::Output) -> middy::midi::Result<SenderPort> + Send,
    ) -> PyResult<Py<Self>> {
        check_blocking_thread()?;
        let encoding = client.get().encoding;
        let resource = Arc::new(Managed::new(
            Arc::clone(&client.get().state),
            None,
            |native: Sender| native.close().map_err(|e| midi_error(&e)),
        ));
        let connection = Py::new(
            py,
            Self {
                resource: Arc::clone(&resource),
                client,
                encoding,
                failure: std::sync::Mutex::new(None),
            },
        )?;
        lifecycle::register(resource.clone())?;
        let mut failure = None;
        let result = py.detach(|| {
            resource.connect(|native| {
                operation(native)
                    .map(|port| Sender { port, mpe: None })
                    .map_err(|error| {
                        failure = Some(error);
                        crate::errors::ConnectError::new_err("MIDI output open failed")
                    })
            })
        });
        if let Some(error) = failure {
            return Err(midi_error(&error));
        }
        result?;
        Ok(connection)
    }
}
impl Drop for OutputConnection {
    fn drop(&mut self) {
        lifecycle::defer(self.resource.clone());
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::state::Connection;
    use std::sync::Mutex;
    #[pyclass(frozen)]
    pub(crate) struct OutputDriver {
        connection: Connection<()>,
        client: Client<()>,
        pub(crate) messages: Mutex<Vec<Vec<u8>>>,
    }
    #[pymethods]
    impl OutputDriver {
        fn send(&self, py: Python<'_>, packet: &Bound<'_, PyAny>) -> PyResult<()> {
            check_blocking_thread()?;
            let packet = extract_packet(packet)?;
            transport::validate_packet(transport::Encoding::Bytes, &packet)
                .map_err(|e| transport_error(e, "send"))?;
            self.connection
                .send(&self.client, py, &packet, |(), packet| {
                    if let transport::Packet::Bytes(bytes) = packet {
                        self.messages.lock().unwrap().push(bytes.clone());
                    }
                    Ok(())
                })
        }
        fn close(&self, py: Python<'_>) -> PyResult<()> {
            check_blocking_thread()?;
            py.detach(|| self.connection.close(&self.client, |()| Ok(())))
        }
    }
    impl OutputDriver {
        pub(crate) fn new(py: Python<'_>) -> Py<Self> {
            let driver = Py::new(
                py,
                Self {
                    connection: Connection::new(),
                    client: Client::new(()),
                    messages: Mutex::new(Vec::new()),
                },
            )
            .unwrap();
            driver
                .get()
                .client
                .connect(&driver.get().connection, Ok)
                .unwrap();
            driver
        }
    }
}
