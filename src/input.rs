use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use middy::transport;
use pyo3::prelude::*;
use pyo3::{PyTraverseError, PyVisit};

use crate::callback::{check_blocking_thread, Callback};
use crate::errors::{native_call, transport_error, StateError};
use crate::lifecycle::{self, Managed, Resource};
use crate::packets::{Backend, Clock, Encoding, ReceivedPacket, TimestampMode};
use crate::ports::Port;
use crate::state::Client;

#[pyclass(frozen, module = "middy._native")]
pub struct Input {
    subprocess: Option<transport::subprocess::SubprocessOptions>,
    state: Arc<Client<transport::Input>>,
    identity: String,
    diagnostics: transport::WorkerDiagnostics,
    snapshot: AtomicU64,
    mode: TimestampMode,
}

#[pymethods]
impl Input {
    #[new]
    // Match the Python keyword interface without introducing a second options type.
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (*, identity, encoding=Encoding::Bytes, backend=None, timestamps=TimestampMode::Absolute, receive_byte_limit=8_388_608, worker_program=None, worker_args=None, worker_timeout=5.0))]
    fn new(
        py: Python<'_>,
        identity: String,
        encoding: Encoding,
        backend: Option<Backend>,
        timestamps: TimestampMode,
        receive_byte_limit: usize,
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
                    args: worker_args.clone().unwrap_or_default(),
                    timeout: std::time::Duration::try_from_secs_f64(worker_timeout).map_err(
                        |_| {
                            pyo3::exceptions::PyValueError::new_err(
                                "worker timeout must be finite and nonnegative",
                            )
                        },
                    )?,
                })
            })
            .transpose()?;
        let native = py
            .detach(|| {
                native_call("create input", || {
                    let options = transport::InputOptions {
                        encoding: encoding.native(),
                        backend: backend.map(Backend::native),
                        timestamps: timestamps.native(),
                        receive_byte_limit,
                    };
                    if let Some(subprocess) = &subprocess {
                        transport::Input::new_subprocess(options, subprocess.clone())
                    } else {
                        transport::Input::new(options)
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
            mode: timestamps,
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
                    .map(|(index, native)| Port::new(native, &self.identity, snapshot, index, true))
                    .collect())
            })
        })
    }
    fn connect(
        slf: Py<Self>,
        py: Python<'_>,
        port: &Port,
        callback: Py<PyAny>,
        clock_identity: String,
    ) -> PyResult<Py<InputConnection>> {
        let native_port = port.native.clone();
        InputConnection::open(
            py,
            slf,
            callback,
            clock_identity,
            move |native, callback| match native_port {
                Some(port) => native.connect(&port, callback),
                None => Err(transport::Error::Invalid(
                    "port is not a native snapshot from this client".into(),
                )),
            },
        )
    }
    #[pyo3(signature=(callback,stream,sysex_limit,raw,patterns))]
    fn listen_all(
        slf: Py<Self>,
        py: Python<'_>,
        callback: Py<PyAny>,
        stream: String,
        sysex_limit: usize,
        raw: bool,
        patterns: Option<Vec<String>>,
    ) -> PyResult<Py<AllInputConnection>> {
        AllInputConnection::open(py, slf, callback, stream, sysex_limit, raw, patterns)
    }
    fn create_virtual(
        slf: Py<Self>,
        py: Python<'_>,
        name: String,
        callback: Py<PyAny>,
        clock_identity: String,
    ) -> PyResult<Py<InputConnection>> {
        InputConnection::open(
            py,
            slf,
            callback,
            clock_identity,
            move |native, callback| native.create_virtual(&name, callback),
        )
    }
}

#[pyclass(frozen, weakref, module = "middy._native")]
pub struct InputConnection {
    resource: Arc<Managed<transport::Input, transport::InputConnection>>,
    client: Py<Input>,
    clock: OnceLock<Clock>,
    diagnostics: OnceLock<transport::InputDiagnostics>,
}

#[pymethods]
impl InputConnection {
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        check_blocking_thread()?;
        self.resource.retire();
        py.detach(|| self.resource.close())
    }
    #[getter]
    fn closed(&self) -> bool {
        self.resource.native.closed() || self.client.get().diagnostics.failure().is_some()
    }
    #[getter]
    fn clock(&self) -> PyResult<Clock> {
        self.clock
            .get()
            .cloned()
            .ok_or_else(|| StateError::new_err("input is not open"))
    }
    #[getter]
    fn dropped_packets(&self) -> u64 {
        self.diagnostics
            .get()
            .map_or(0, transport::InputDiagnostics::dropped_packets)
            .saturating_add(self.resource.callback.as_ref().map_or(0, |c| c.discarded()))
    }
    #[getter]
    fn worker_failure(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.diagnostics
            .get()
            .and_then(|d| d.worker_failure())
            .map(|e| {
                transport_error(e, &self.client.get().diagnostics.operation())
                    .into_value(py)
                    .into_any()
            })
            .or_else(|| self.resource.callback.as_ref().and_then(|c| c.failure(py)))
    }
    #[getter]
    fn last_message_time(&self) -> Option<f64> {
        self.resource
            .callback
            .as_ref()
            .and_then(|c| c.last_message_time())
    }
    fn __enter__(slf: Py<Self>, py: Python<'_>) -> PyResult<Py<Self>> {
        slf.get().resource.native.check_failed()?;
        if slf.get().closed() {
            return Err(StateError::new_err("MIDI connection is closed"));
        }
        let _ = py;
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
        visit.call(&self.client)?;
        self.resource.callback.as_ref().unwrap().traverse(visit)
    }
    fn __clear__(&self) {
        self.resource.callback.as_ref().unwrap().clear();
        lifecycle::defer(self.resource.clone());
    }
}

impl InputConnection {
    fn open(
        py: Python<'_>,
        client: Py<Input>,
        callable: Py<PyAny>,
        identity: String,
        operation: impl FnOnce(
                transport::Input,
                Box<dyn FnMut(transport::ReceivedPacket) + Send>,
            ) -> transport::Result<transport::InputConnection>
            + Send,
    ) -> PyResult<Py<Self>> {
        check_blocking_thread()?;
        let callback = Arc::new(Callback::new(py, callable)?);
        let resource = Arc::new(Managed::new(
            Arc::clone(&client.get().state),
            Some(Arc::clone(&callback)),
            |native: transport::InputConnection| {
                native.close().map_err(|e| transport_error(e, "close"))
            },
        ));
        let mode = client.get().mode;
        let connection = Py::new(
            py,
            Self {
                resource: Arc::clone(&resource),
                client,
                clock: OnceLock::new(),
                diagnostics: OnceLock::new(),
            },
        )?;
        lifecycle::register(resource.clone())?;
        let callback_identity = identity.clone();
        let result = py.detach(|| {
            resource.connect(|native| {
                let input = operation(
                    native,
                    Box::new(move |packet| {
                        let clock = Clock::native(
                            callback_identity.clone(),
                            Backend::from_native(packet.clock.backend),
                            mode,
                        );
                        callback.deliver(move |py| {
                            Ok(Py::new(
                                py,
                                ReceivedPacket {
                                    sequence: Some(packet.sequence),
                                    timestamp_ns: packet.timestamp_ns,
                                    clock,
                                    packet: packet.packet,
                                },
                            )?
                            .into_any())
                        });
                    }),
                )
                .map_err(|e| transport_error(e, "open"))?;
                connection
                    .get()
                    .clock
                    .set(Clock::native(
                        identity,
                        Backend::from_native(input.clock().backend),
                        mode,
                    ))
                    .ok();
                connection.get().diagnostics.set(input.diagnostics()).ok();
                Ok(input)
            })
        });
        if result.is_err() {
            resource.retire();
        }
        result?;
        Ok(connection)
    }
}
impl Drop for InputConnection {
    fn drop(&mut self) {
        lifecycle::defer(self.resource.clone());
    }
}

#[pyclass(frozen, weakref, module = "middy._native")]
pub struct AllInputConnection {
    time_origin: (std::time::Instant, f64),
    resource: Arc<Managed<transport::Input, transport::all::AllInputConnection>>,
    client: Py<Input>,
    clock: OnceLock<Clock>,
    diagnostics: OnceLock<transport::all::AllDiagnostics>,
}
#[pymethods]
impl AllInputConnection {
    #[getter]
    fn pending(&self) -> bool {
        self.diagnostics.get().is_some_and(|d| d.pending())
    }
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        check_blocking_thread()?;
        let result = py.detach(|| self.resource.close_native());
        self.resource.retire();
        if let Some(callback) = &self.resource.callback {
            py.detach(|| callback.drain())?;
        }
        result
    }
    #[getter]
    fn closed(&self) -> bool {
        self.resource.native.closed()
            || self
                .diagnostics
                .get()
                .is_some_and(|d| d.worker_failure().is_some())
    }
    #[getter]
    fn clock(&self) -> PyResult<Clock> {
        self.clock
            .get()
            .cloned()
            .ok_or_else(|| StateError::new_err("input is not open"))
    }
    #[getter]
    fn dropped_packets(&self) -> u64 {
        self.diagnostics
            .get()
            .map_or(0, |d| d.dropped_packets())
            .saturating_add(self.resource.callback.as_ref().map_or(0, |c| c.discarded()))
    }
    #[getter]
    fn worker_failure(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.diagnostics
            .get()
            .and_then(|d| d.worker_failure())
            .map(|e| {
                transport_error(e, "follow MIDI inputs")
                    .into_value(py)
                    .into_any()
            })
            .or_else(|| self.resource.callback.as_ref().and_then(|c| c.failure(py)))
    }
    #[getter]
    fn last_message_time(&self) -> Option<f64> {
        self.diagnostics
            .get()
            .and_then(|d| d.last_message_time())
            .map(|t| self.time_origin.1 + t.duration_since(self.time_origin.0).as_secs_f64())
    }
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.client)?;
        self.resource.callback.as_ref().unwrap().traverse(visit)
    }
    fn __clear__(&self) {
        self.resource.callback.as_ref().unwrap().clear();
        lifecycle::defer(self.resource.clone());
    }
}
impl AllInputConnection {
    fn open(
        py: Python<'_>,
        client: Py<Input>,
        callable: Py<PyAny>,
        stream: String,
        sysex_limit: usize,
        raw: bool,
        patterns: Option<Vec<String>>,
    ) -> PyResult<Py<Self>> {
        let time_origin = (
            std::time::Instant::now(),
            pyo3::types::PyModule::import(py, "time")?
                .getattr("monotonic")?
                .call0()?
                .extract::<f64>()?,
        );
        check_blocking_thread()?;
        let callback = Arc::new(Callback::new(py, callable)?);
        let resource = Arc::new(Managed::new(
            Arc::clone(&client.get().state),
            Some(Arc::clone(&callback)),
            |native: transport::all::AllInputConnection| {
                native.close().map_err(|e| transport_error(e, "close"))
            },
        ));
        let subprocess = client.get().subprocess.clone();
        let mode = client.get().mode;
        let connection = Py::new(
            py,
            Self {
                time_origin,
                resource: Arc::clone(&resource),
                client,
                clock: OnceLock::new(),
                diagnostics: OnceLock::new(),
            },
        )?;
        lifecycle::register(resource.clone())?;
        let result = py.detach(|| {
            resource.connect(|native| {
                let group_clock = format!("{stream}/clock");
                let input = native
                    .connect_all(
                        stream,
                        sysex_limit,
                        raw,
                        subprocess,
                        patterns,
                        move |item| {
                            callback.deliver(move |py| match item {
                                transport::all::AllItem::Decoded(item) => {
                                    crate::live::item(py, item)
                                }
                                transport::all::AllItem::Raw(
                                    middy::events::InputItem::Diagnostic(value),
                                    _,
                                ) => Ok(Py::new(py, crate::live::StreamDiagnostic { value })?
                                    .into_any()),
                                transport::all::AllItem::Raw(
                                    middy::events::InputItem::Message(packet),
                                    clock,
                                ) => Ok(Py::new(
                                    py,
                                    ReceivedPacket {
                                        sequence: Some(packet.sequence),
                                        timestamp_ns: packet.timestamp_ns,
                                        clock: Clock { value: clock },
                                        packet: packet.packet,
                                    },
                                )?
                                .into_any()),
                            });
                        },
                    )
                    .map_err(|e| transport_error(e, "open"))?;
                connection
                    .get()
                    .clock
                    .set(Clock::native(
                        group_clock,
                        Backend::from_native(input.clock().backend),
                        mode,
                    ))
                    .ok();
                connection.get().diagnostics.set(input.diagnostics()).ok();
                Ok(input)
            })
        });
        if result.is_err() {
            resource.retire();
        }
        result?;
        Ok(connection)
    }
}
impl Drop for AllInputConnection {
    fn drop(&mut self) {
        lifecycle::defer(self.resource.clone());
    }
}
