use crate::callback::check_blocking_thread;
use crate::errors::{midi_error, native_call, transport_error, StateError};
use crate::lifecycle::{self, Managed, Resource};
use crate::packets::{extract_packet, Backend, Encoding};
use crate::ports::Port;
use crate::state::Client;
use middy::transport;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
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
                .map(Sender::Single)
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
            middy::midi::OutputGroup::new(native, patterns, subprocess).map(Sender::Group)
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
                .map(Sender::Single)
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
enum Sender {
    Single(transport::OutputConnection),
    Group(middy::midi::OutputGroup),
}
impl Sender {
    fn send(&mut self, packet: &transport::Packet) -> middy::midi::Result<()> {
        match self {
            Self::Single(c) => c.send(packet).map_err(middy::midi::Error::Send),
            Self::Group(c) => c.send(packet),
        }
    }
    fn send_command(&mut self, command: &middy::messages::AnyCommand) -> middy::midi::Result<()> {
        match self {
            Self::Single(c) => c.send_command(command).map_err(middy::midi::Error::Send),
            Self::Group(c) => c.send_command(command),
        }
    }
    fn close(self) -> middy::midi::Result<transport::Output> {
        match self {
            Self::Single(c) => c.close().map_err(middy::midi::Error::Transport),
            Self::Group(c) => c.close(),
        }
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

    fn send_command(&self, py: Python<'_>, command: &Bound<'_, PyAny>) -> PyResult<()> {
        check_blocking_thread()?;
        let command = crate::messages::extract_command(command)?;
        // Validate before entering Managed::send: preparation errors must not
        // retire a healthy connection as though a backend send had failed.
        if self.encoding == Encoding::Ump {
            let words = middy::ump::encode(&middy::events::Payload::Command(command.clone()))
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
            transport::validate_packet(self.encoding.native(), &transport::Packet::Ump(words))
                .map_err(|e| transport_error(e, "send"))?;
        } else {
            use middy::messages::PrimitiveMessage;
            for message in
                middy::codecs::expand(&command).map_err(|e| PyValueError::new_err(e.to_string()))?
            {
                message
                    .encode()
                    .map_err(|e| PyValueError::new_err(e.to_string()))?;
            }
        }
        let mut failure = None;
        let result =
            self.resource
                .native
                .send(&self.resource.client, py, &command, |native, command| {
                    native.send_command(command).map_err(|error| {
                        failure = Some(error);
                        crate::errors::SendError::new_err("compound MIDI send failed")
                    })
                });
        // Construct the Python progress exception after releasing the connection
        // lock. Importing/calling a Python exception class can execute user code.
        match failure {
            Some(error) => {
                let error = Arc::new(error);
                *self.failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&error));
                Err(midi_error(&error))
            }
            None => result,
        }
    }

    fn send(&self, py: Python<'_>, packet: &Bound<'_, PyAny>) -> PyResult<()> {
        check_blocking_thread()?;
        let packet = extract_packet(packet)?;
        transport::validate_packet(self.encoding.native(), &packet)
            .map_err(|e| transport_error(e, "send"))?;
        let mut failure = None;
        let result =
            self.resource
                .native
                .send(&self.resource.client, py, &packet, |native, packet| {
                    native.send(packet).map_err(|error| {
                        failure = Some(error);
                        crate::errors::SendError::new_err("MIDI send failed")
                    })
                });
        match failure {
            Some(error) => {
                let error = Arc::new(error);
                *self.failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&error));
                Err(midi_error(&error))
            }
            None => result,
        }
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
    fn open(
        py: Python<'_>,
        client: Py<Output>,
        operation: impl FnOnce(transport::Output) -> middy::midi::Result<Sender> + Send,
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
                operation(native).map_err(|error| {
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
