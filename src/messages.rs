//! Frozen Python message classes backed by the shared Rust toolkit.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use middy::messages;
use middy::messages::PrimitiveMessage;
use pyo3::buffer::PyBuffer;
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyInt, PyTuple};

#[pyclass(frozen, subclass, skip_from_py_object, module = "middy.messages")]
#[derive(Clone, Default)]
pub(crate) struct Message {
    metadata: Option<std::sync::Arc<Metadata>>,
}
#[derive(Clone, Default)]
pub(crate) struct Metadata {
    pub port: Option<std::sync::Arc<middy::events::PortInfo>>,
    pub timestamp: Option<middy::events::Timestamp>,
    pub stream: Option<std::sync::Arc<str>>,
    pub span: Option<middy::events::SourceSpan>,
}
impl Message {
    pub(crate) fn metadata(&self) -> Option<&std::sync::Arc<Metadata>> {
        self.metadata.as_ref()
    }
    pub(crate) fn from_metadata(metadata: Option<std::sync::Arc<Metadata>>) -> PyResult<Self> {
        if let Some(metadata) = &metadata {
            if metadata.stream.as_ref().is_some_and(|s| s.is_empty()) {
                return Err(PyValueError::new_err("empty stream identity"));
            }
            if let Some(span) = &metadata.span {
                span.validate(metadata.timestamp.as_ref())
                    .map_err(|e| PyValueError::new_err(e.to_string()))?;
            }
        }
        Ok(Self {
            metadata: metadata.filter(|m| {
                m.port.is_some() || m.timestamp.is_some() || m.stream.is_some() || m.span.is_some()
            }),
        })
    }
}

/// A validated value that can be allocated with default or replacement metadata.
pub(crate) struct MessageInitializer<T>(pub(crate) T);

impl<T: pyo3::PyClass<BaseType = Message>> From<MessageInitializer<T>> for PyClassInitializer<T> {
    fn from(value: MessageInitializer<T>) -> Self {
        PyClassInitializer::from(Message::default()).add_subclass(value.0)
    }
}

impl<'py, T: pyo3::PyClass<BaseType = Message>> IntoPyObject<'py> for MessageInitializer<T> {
    type Target = T;
    type Output = Bound<'py, T>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
        Py::new(py, self).map(|v| v.into_bound(py))
    }
}

macro_rules! construct_message {
    ($object:expr, $metadata:expr; $($class:path => $initializer:expr),+ $(,)?) => {$({
        if $object.is_instance_of::<$class>() {
            let value = $initializer?.0;
            let base = $crate::messages::Message::from_metadata($metadata.clone())?;
            return Ok(Some(Py::new($object.py(),
                PyClassInitializer::from(base).add_subclass(value))?.into_any()));
        }
    })+};
}
pub(crate) use construct_message;

pub(crate) fn field<'py>(fields: &Bound<'py, PyDict>, name: &str) -> PyResult<Bound<'py, PyAny>> {
    fields
        .get_item(name)?
        .ok_or_else(|| PyTypeError::new_err(format!("missing field: {name}")))
}
pub(crate) fn optional_field<'py>(
    fields: &Bound<'py, PyDict>,
    name: &str,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    Ok(fields.get_item(name)?.filter(|v| !v.is_none()))
}

pub(crate) fn argument_error(py: Python<'_>, name: &str, error: PyErr) -> PyErr {
    let _ = error
        .value(py)
        .call_method1("add_note", (format!("while processing '{name}'"),));
    error
}

pub(crate) fn converted_field<T>(
    fields: &Bound<'_, PyDict>,
    name: &str,
    convert: impl FnOnce(&Bound<'_, PyAny>) -> PyResult<T>,
) -> PyResult<T> {
    convert(&field(fields, name)?).map_err(|error| argument_error(fields.py(), name, error))
}

#[pymethods]
impl Message {
    #[getter]
    fn port(&self) -> Option<crate::ports::PortInfo> {
        self.metadata()
            .and_then(|m| m.port.clone())
            .map(|value| crate::ports::PortInfo { value })
    }
    /// Return this message unchanged; note-addressed subclasses shift their note number.
    fn transpose(
        slf: PyRef<'_, Self>,
        #[pyo3(from_py_with = crate::units::semitones)] semitones: i64,
    ) -> PyResult<Py<PyAny>> {
        let _ = semitones;
        let py = slf.py();
        Ok(slf.into_pyobject(py)?.into_any().unbind())
    }
    #[getter]
    fn timestamp(&self) -> Option<crate::event_values::Timestamp> {
        self.metadata()
            .and_then(|m| m.timestamp.clone())
            .map(|value| crate::event_values::Timestamp { value })
    }
    #[getter]
    fn stream(&self) -> Option<&str> {
        self.metadata().and_then(|m| m.stream.as_deref())
    }
    #[getter]
    fn span(&self) -> Option<crate::event_values::SourceSpan> {
        self.metadata()
            .and_then(|m| m.span.clone())
            .map(|value| crate::event_values::SourceSpan { value })
    }
    #[new]
    fn new() -> PyResult<Self> {
        Err(PyTypeError::new_err("Message is abstract"))
    }
}

#[pyclass(frozen, extends = Message, module = "middy.messages")]
pub(crate) struct SysEx {
    pub(crate) value: messages::SysEx,
}

#[pymethods]
impl SysEx {
    #[new]
    #[pyo3(signature = (data, *, group=None))]
    fn new(
        data: &Bound<'_, PyAny>,
        group: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<MessageInitializer<Self>> {
        let mut value = payload(data)?;
        if let Some(g) = group {
            value = value
                .with_group(integer(g, "group")?)
                .map_err(value_error)?;
        }
        Ok(MessageInitializer(Self { value }))
    }

    #[getter]
    fn data<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, self.value.data())
    }
    #[getter]
    fn group(&self) -> Option<u8> {
        self.value.group()
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        let suffix = self
            .value
            .group()
            .map_or(String::new(), |g| format!(", group={g}"));
        Ok(format!("SysEx(data={}{})", self.data(py).repr()?, suffix))
    }

    fn __eq__(&self, other: &Bound<'_, PyAny>) -> PyResult<bool> {
        Ok(other
            .extract::<PyRef<'_, Self>>()
            .is_ok_and(|other| self.value == other.value))
    }

    fn __hash__(&self) -> u64 {
        value_hash(&self.value)
    }

    #[pyo3(signature = (**changes))]
    fn replace<'py>(
        slf: PyRef<'py, Self>,
        changes: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        crate::tuning::replacement(slf.into_pyobject(py)?.as_any(), &["data", "group"], changes)
    }

    #[classattr]
    fn __annotations__(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
        let annotations = PyDict::new(py);
        annotations.set_item("data", py.get_type::<PyBytes>())?;
        annotations.set_item(
            "group",
            py.import("typing")?
                .getattr("Optional")?
                .get_item(py.get_type::<PyInt>())?,
        )?;
        Ok(annotations)
    }

    #[classattr]
    fn __match_args__() -> (&'static str,) {
        ("data",)
    }
}

macro_rules! versioned_message {
    ($name:ident $signature:tt { $($required:ident),* } { $($optional:ident => $setter:ident),* } { $($extra:ident => $extra_setter:ident),* } $(; $($method:item)*)?) => {
        #[pyclass(frozen, extends=Message, module="middy.messages")]
        struct $name { value: messages::$name }
        impl $name {
            fn from_value(value: messages::$name) -> Self { Self { value } }
            fn from_fields(fields: &Bound<'_, PyDict>) -> PyResult<MessageInitializer<Self>> {
                Self::new($(&field(fields,stringify!($required))?,)*
                    $(optional_field(fields,stringify!($optional))?.as_ref(),)*
                    optional_field(fields,"channel")?.as_ref(), converted_field(fields,"version",strict_integer)?,
                    optional_field(fields,"group")?.as_ref(),
                    $(optional_field(fields,stringify!($extra))?.as_ref(),)*)
            }
            fn resolved(&self) -> PyResult<&messages::$name> {
                if self.value.version() != 1 { return Err(PyValueError::new_err("expected MIDI 1; convert explicitly")); }
                if self.value.channel().is_none() { return Err(PyValueError::new_err("message channel is unassigned")); }
                Ok(&self.value)
            }
        }
        #[pymethods]
        impl $name {
            #[new]
            #[pyo3(signature=$signature)]
            #[allow(clippy::too_many_arguments)]
            fn new($($required: &Bound<'_, PyAny>,)* $($optional: Option<&Bound<'_, PyAny>>,)* channel: Option<&Bound<'_, PyAny>>, #[pyo3(from_py_with=strict_integer)] version: i64, group: Option<&Bound<'_, PyAny>>, $($extra: Option<&Bound<'_, PyAny>>),*) -> PyResult<MessageInitializer<Self>> {
                let mut value = match version {
                    1 => messages::$name::new($(integer($required,stringify!($required))?),*),
                    2 => messages::$name::new_midi2($(integer($required,stringify!($required))?),*),
                    _ => return Err(PyValueError::new_err("version must be 1 or 2")),
                }.map_err(value_error)?;
                $(if let Some(v) = $optional { value = value.$setter(integer(v,stringify!($optional))?).map_err(value_error)?; })*
                if let Some(v) = channel { value = value.with_channel(integer(v,"channel")?).map_err(value_error)?; }
                if let Some(v) = group { value = value.with_group(integer(v,"group")?).map_err(value_error)?; }
                $(if let Some(v) = $extra { value = value.$extra_setter(integer(v,stringify!($extra))?).map_err(value_error)?; })*
                Ok(MessageInitializer(Self { value }))
            }
            #[getter] fn channel(&self) -> Option<u8> { self.value.channel() }
            #[getter] fn version(&self) -> u8 { self.value.version() }
            #[getter] fn group(&self) -> Option<u8> { self.value.group() }
            $(#[getter] fn $required(&self) -> i64 { i64::from(self.value.$required()) })*
            $(#[getter] fn $optional(&self) -> i64 { i64::from(self.value.$optional()) })*
            $(#[getter] fn $extra(&self) -> Option<i64> { self.value.$extra().map(i64::from) })*
            fn __eq__(&self, other:&Bound<'_,PyAny>) -> bool { other.extract::<PyRef<'_,Self>>().is_ok_and(|v| self.value==v.value) }
            fn __hash__(&self) -> u64 { value_hash(&self.value) }
            fn __repr__(slf:PyRef<'_,Self>) -> PyResult<String> {
                let py=slf.py();
                let mut fields = if slf.value.version()==1 { vec![$(stringify!($required),)* $(stringify!($optional),)* "channel"] } else { vec![$(stringify!($required),)* $(stringify!($optional),)* "channel", "version", "group", $(stringify!($extra)),*] };
                if slf.value.version()==1 && slf.value.group().is_some() { fields.push("group"); }
                crate::tuning::render(slf.into_pyobject(py)?.as_any(), stringify!($name), &fields)
            }
            #[pyo3(signature=(**changes))]
            fn replace<'py>(slf:PyRef<'py,Self>, changes:Option<&Bound<'py,PyDict>>) -> PyResult<Bound<'py,PyAny>> {
                if slf.value.version() == 2 && changes.map(|c| c.get_item("group")).transpose()?.flatten().is_some_and(|v| v.is_none()) {
                    return Err(PyValueError::new_err("MIDI 2 requires a group"));
                }
                if let Some(v) = changes.map(|c| c.get_item("version")).transpose()?.flatten() {
                    if integer(&v,"version")? != i64::from(slf.value.version()) { return Err(PyValueError::new_err("version cannot change through replace; convert explicitly")); }
                }
                let py=slf.py();
                crate::tuning::replacement(slf.into_pyobject(py)?.as_any(), &[$(stringify!($required),)* $(stringify!($optional),)* "channel", "version", "group", $(stringify!($extra)),*],changes)
            }
            #[classattr] fn __annotations__(py:Python<'_>) -> PyResult<Bound<'_,PyDict>> {
                let d=PyDict::new(py);
                for field in [$(stringify!($required),)* $(stringify!($optional),)* "version"] { d.set_item(field,py.get_type::<PyInt>())?; }
                for field in ["channel", "group", $(stringify!($extra)),*] { d.set_item(field,py.import("typing")?.getattr("Optional")?.get_item(py.get_type::<PyInt>())?)?; }
                Ok(d)
            }
            #[classattr] fn __match_args__(py:Python<'_>) -> PyResult<Bound<'_,PyTuple>> { PyTuple::new(py,[$(stringify!($required),)* $(stringify!($optional),)* "channel"]) }
            $($($method)*)?
        }
    };
}
pub(crate) fn transpose_note<'py>(
    value: &Bound<'py, PyAny>,
    semitones: i64,
) -> PyResult<Py<PyAny>> {
    let changes = PyDict::new(value.py());
    let note: u8 = value.getattr("note")?.extract()?;
    changes.set_item(
        "note",
        i64::from(note).saturating_add(semitones).clamp(0, 127),
    )?;
    Ok(value.call_method("replace", (), Some(&changes))?.unbind())
}

macro_rules! versioned_note {
    ($name:ident) => {
        versioned_message! { $name (note, velocity=None, channel=None, *, version=1, group=None, attribute_type=None, attribute_data=None) {note} {velocity => with_velocity} {attribute_type => with_attribute_type, attribute_data => with_attribute_data};
            fn transpose(slf:PyRef<'_,Self>, #[pyo3(from_py_with=crate::units::semitones)] semitones:i64) -> PyResult<Py<PyAny>> {
                let py=slf.py();
                transpose_note(slf.into_pyobject(py)?.as_any(), semitones)
            }
        }
    };
}
versioned_note!(NoteOn);
versioned_note!(NoteOff);
versioned_message! { PolyPressure (note, pressure=None, channel=None, *, version=1, group=None) {note} {pressure => with_pressure} {};
    fn transpose(slf:PyRef<'_,Self>, #[pyo3(from_py_with=crate::units::semitones)] semitones:i64) -> PyResult<Py<PyAny>> {
        let py=slf.py();
        transpose_note(slf.into_pyobject(py)?.as_any(), semitones)
    }
}
#[pyclass(frozen, extends = Message, module = "middy.messages")]
pub(crate) struct ControlChange {
    value: messages::ControlChange,
}
impl ControlChange {
    fn from_value(value: messages::ControlChange) -> Self {
        Self { value }
    }
    fn from_fields(fields: &Bound<'_, PyDict>) -> PyResult<MessageInitializer<Self>> {
        Self::new(
            &field(fields, "controller")?,
            &field(fields, "value")?,
            optional_field(fields, "channel")?.as_ref(),
            converted_field(fields, "version", strict_integer)?,
            optional_field(fields, "group")?.as_ref(),
            optional_field(fields, "resolution")?.as_ref(),
        )
    }
    fn resolved(&self) -> PyResult<&messages::ControlChange> {
        if self.value.version() != 1 {
            return Err(PyValueError::new_err("expected MIDI 1; convert explicitly"));
        }
        if self.value.channel().is_none() {
            return Err(PyValueError::new_err("message channel is unassigned"));
        }
        if self.value.resolution() == 14 {
            return Err(PyValueError::new_err(
                "14-bit ControlChange requires expansion into two messages",
            ));
        }
        Ok(&self.value)
    }
}
#[pymethods]
impl ControlChange {
    #[new]
    #[pyo3(signature=(controller, value, channel=None, *, version=1, group=None, resolution=None))]
    fn new(
        controller: &Bound<'_, PyAny>,
        value: &Bound<'_, PyAny>,
        channel: Option<&Bound<'_, PyAny>>,
        #[pyo3(from_py_with=strict_integer)] version: i64,
        group: Option<&Bound<'_, PyAny>>,
        resolution: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<MessageInitializer<Self>> {
        let controller = integer(controller, "controller")?;
        let number = integer(value, "value")?;
        let resolution = resolution.map(|v| integer(v, "resolution")).transpose()?;
        let mut value = match version {
            1 => messages::ControlChange::new_with_resolution(
                controller,
                number,
                resolution.unwrap_or(7),
            ),
            2 => messages::ControlChange::new_midi2(controller, number)
                .and_then(|v| v.with_resolution(resolution.unwrap_or(32))),
            _ => return Err(PyValueError::new_err("version must be 1 or 2")),
        }
        .map_err(value_error)?;
        if let Some(v) = channel {
            value = value
                .with_channel(integer(v, "channel")?)
                .map_err(value_error)?;
        }
        if let Some(v) = group {
            value = value
                .with_group(integer(v, "group")?)
                .map_err(value_error)?;
        }
        Ok(MessageInitializer(Self { value }))
    }
    #[getter]
    fn controller(&self) -> u8 {
        self.value.controller()
    }
    #[getter]
    fn value(&self) -> u32 {
        self.value.value()
    }
    #[getter]
    fn channel(&self) -> Option<u8> {
        self.value.channel()
    }
    #[getter]
    fn version(&self) -> u8 {
        self.value.version()
    }
    #[getter]
    fn group(&self) -> Option<u8> {
        self.value.group()
    }
    #[getter]
    fn resolution(&self) -> u8 {
        self.value.resolution()
    }
    fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        other
            .extract::<PyRef<'_, Self>>()
            .is_ok_and(|v| self.value == v.value)
    }
    fn __hash__(&self) -> u64 {
        value_hash(&self.value)
    }
    fn __repr__(slf: PyRef<'_, Self>) -> PyResult<String> {
        let py = slf.py();
        let mut fields = if slf.value.version() == 2 {
            vec![
                "controller",
                "value",
                "channel",
                "version",
                "group",
                "resolution",
            ]
        } else {
            vec!["controller", "value", "channel", "resolution"]
        };
        if slf.value.version() == 1 && slf.value.group().is_some() {
            fields.push("group");
        }
        crate::tuning::render(slf.into_pyobject(py)?.as_any(), "ControlChange", &fields)
    }
    #[pyo3(signature=(**changes))]
    fn replace<'py>(
        slf: PyRef<'py, Self>,
        changes: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if slf.value.version() == 2
            && changes
                .map(|c| c.get_item("group"))
                .transpose()?
                .flatten()
                .is_some_and(|v| v.is_none())
        {
            return Err(PyValueError::new_err("MIDI 2 requires a group"));
        }
        if let Some(v) = changes
            .map(|c| c.get_item("version"))
            .transpose()?
            .flatten()
        {
            if integer(&v, "version")? != i64::from(slf.value.version()) {
                return Err(PyValueError::new_err(
                    "version cannot change through replace; convert explicitly",
                ));
            }
        }
        let py = slf.py();
        crate::tuning::replacement(
            slf.into_pyobject(py)?.as_any(),
            &[
                "controller",
                "value",
                "channel",
                "version",
                "group",
                "resolution",
            ],
            changes,
        )
    }
    #[classattr]
    fn __annotations__(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
        let fields = PyDict::new(py);
        for name in ["controller", "value", "version", "resolution"] {
            fields.set_item(name, py.get_type::<PyInt>())?;
        }
        for name in ["channel", "group"] {
            fields.set_item(
                name,
                py.import("typing")?
                    .getattr("Optional")?
                    .get_item(py.get_type::<PyInt>())?,
            )?;
        }
        Ok(fields)
    }
    #[classattr]
    fn __match_args__() -> (&'static str, &'static str, &'static str) {
        ("controller", "value", "channel")
    }
}
#[pyclass(frozen, extends = Message, module = "middy.messages")]
struct ProgramChange {
    value: messages::ProgramChange,
}
impl ProgramChange {
    fn from_value(value: messages::ProgramChange) -> Self {
        Self { value }
    }
    fn from_fields(fields: &Bound<'_, PyDict>) -> PyResult<MessageInitializer<Self>> {
        Self::new(
            converted_field(fields, "program", strict_integer)?,
            optional_field(fields, "channel")?.as_ref(),
            converted_field(fields, "version", strict_integer)?,
            optional_field(fields, "group")?.as_ref(),
            optional_field(fields, "bank_msb")?.as_ref(),
            optional_field(fields, "bank_lsb")?.as_ref(),
        )
    }
    fn resolved(&self) -> PyResult<&messages::ProgramChange> {
        Ok(&self.value)
    }
}
#[pymethods]
impl ProgramChange {
    #[new]
    #[pyo3(signature=(program, channel=None, *, version=1, group=None, bank_msb=None, bank_lsb=None))]
    fn new(
        #[pyo3(from_py_with=strict_integer)] program: i64,
        channel: Option<&Bound<'_, PyAny>>,
        #[pyo3(from_py_with=strict_integer)] version: i64,
        group: Option<&Bound<'_, PyAny>>,
        bank_msb: Option<&Bound<'_, PyAny>>,
        bank_lsb: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<MessageInitializer<Self>> {
        let mut value = match version {
            1 => messages::ProgramChange::new(program),
            2 => messages::ProgramChange::new_midi2(program),
            _ => return Err(PyValueError::new_err("version must be 1 or 2")),
        }
        .map_err(value_error)?;
        match (bank_msb, bank_lsb) {
            (Some(msb), Some(lsb)) => {
                value = value
                    .with_bank(integer(msb, "bank_msb")?, integer(lsb, "bank_lsb")?)
                    .map_err(value_error)?
            }
            (None, None) => {}
            _ => {
                return Err(PyValueError::new_err(
                    "bank_msb and bank_lsb must be supplied together",
                ))
            }
        }
        if let Some(v) = channel {
            value = value
                .with_channel(integer(v, "channel")?)
                .map_err(value_error)?;
        }
        if let Some(v) = group {
            value = value
                .with_group(integer(v, "group")?)
                .map_err(value_error)?;
        }
        Ok(MessageInitializer(Self { value }))
    }
    #[getter]
    fn program(&self) -> u8 {
        self.value.program()
    }
    #[getter]
    fn channel(&self) -> Option<u8> {
        self.value.channel()
    }
    #[getter]
    fn group(&self) -> Option<u8> {
        self.value.group()
    }
    #[getter]
    fn version(&self) -> u8 {
        self.value.version()
    }
    #[getter]
    fn bank_msb(&self) -> Option<u8> {
        self.value.bank_msb()
    }
    #[getter]
    fn bank_lsb(&self) -> Option<u8> {
        self.value.bank_lsb()
    }
    fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        other
            .extract::<PyRef<'_, Self>>()
            .is_ok_and(|v| self.value == v.value)
    }
    fn __hash__(&self) -> u64 {
        value_hash(&self.value)
    }
    fn __repr__(slf: PyRef<'_, Self>) -> PyResult<String> {
        let py = slf.py();
        let mut fields = vec!["program", "channel"];
        if slf.value.version() == 2 {
            fields.push("version");
        }
        if slf.value.group().is_some() {
            fields.push("group");
        }
        if slf.value.bank_msb().is_some() {
            fields.extend(["bank_msb", "bank_lsb"]);
        }
        crate::tuning::render(slf.into_pyobject(py)?.as_any(), "ProgramChange", &fields)
    }
    #[pyo3(signature=(**changes))]
    fn replace<'py>(
        slf: PyRef<'py, Self>,
        changes: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if slf.value.version() == 2
            && changes
                .map(|c| c.get_item("group"))
                .transpose()?
                .flatten()
                .is_some_and(|v| v.is_none())
        {
            return Err(PyValueError::new_err("MIDI 2 requires a group"));
        }
        if let Some(v) = changes
            .map(|c| c.get_item("version"))
            .transpose()?
            .flatten()
        {
            if integer(&v, "version")? != i64::from(slf.value.version()) {
                return Err(PyValueError::new_err(
                    "version cannot change through replace; convert explicitly",
                ));
            }
        }
        let py = slf.py();
        crate::tuning::replacement(
            slf.into_pyobject(py)?.as_any(),
            &[
                "program", "channel", "version", "group", "bank_msb", "bank_lsb",
            ],
            changes,
        )
    }
    #[classattr]
    fn __annotations__(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
        let fields = PyDict::new(py);
        for name in ["program", "version"] {
            fields.set_item(name, py.get_type::<PyInt>())?;
        }
        for name in ["channel", "group", "bank_msb", "bank_lsb"] {
            fields.set_item(
                name,
                py.import("typing")?
                    .getattr("Optional")?
                    .get_item(py.get_type::<PyInt>())?,
            )?;
        }
        Ok(fields)
    }
    #[classattr]
    fn __match_args__() -> (&'static str, &'static str) {
        ("program", "channel")
    }
}
versioned_message! { ChannelPressure (pressure=None, channel=None, *, version=1, group=None) {} {pressure => with_pressure} {} }
versioned_message! { PitchBend (value=None, channel=None, *, version=1, group=None) {} {value => with_value} {} }

macro_rules! message_classes {
    (NoteOn $signature:tt { $($field:ident),* }; $($rest:tt)*) => { message_classes! { $($rest)* } };
    (NoteOff $signature:tt { $($field:ident),* }; $($rest:tt)*) => { message_classes! { $($rest)* } };
    (PolyPressure $signature:tt { $($field:ident),* }; $($rest:tt)*) => { message_classes! { $($rest)* } };
    (ControlChange $signature:tt { $($field:ident),* }; $($rest:tt)*) => { message_classes! { $($rest)* } };
    (ProgramChange $signature:tt { $($field:ident),* }; $($rest:tt)*) => { message_classes! { $($rest)* } };
    (ChannelPressure $signature:tt { $($field:ident),* }; $($rest:tt)*) => { message_classes! { $($rest)* } };
    (PitchBend $signature:tt { $($field:ident),* }; $($rest:tt)*) => { message_classes! { $($rest)* } };
    ($name:ident $signature:tt { $($field:ident),* }; $($rest:tt)*) => {
            #[pyclass(frozen, extends = Message, module = "middy.messages")]
            struct $name { value: messages::$name }
            impl $name {
                fn from_value(value: messages::$name) -> Self { Self { value } }
                fn from_fields(fields: &Bound<'_, PyDict>) -> PyResult<MessageInitializer<Self>> {
                    Self::new($(&field(fields,stringify!($field))?,)* optional_field(fields,"group")?.as_ref())
                }
                fn resolved(&self) -> PyResult<&messages::$name> { Ok(&self.value) }
            }

            #[pymethods]
            impl $name {
                #[new]
                #[pyo3(signature = ($($field,)* *, group=None))]
                fn new($($field: &Bound<'_, PyAny>,)* group: Option<&Bound<'_, PyAny>>) -> PyResult<MessageInitializer<Self>> {
                    let mut value = messages::$name::new($(integer($field, stringify!($field))?),*).map_err(value_error)?;
                    if let Some(g) = group { value = value.with_group(integer(g, "group")?).map_err(value_error)?; }
                    Ok(MessageInitializer(Self { value }))
                }
                $(#[getter]
                fn $field(&self) -> i64 { i64::from(self.value.$field()) })*
                #[getter] fn group(&self) -> Option<u8> { self.value.group() }

                fn __repr__(&self) -> String {
                    let mut fields: Vec<String> = vec![$(format!("{}={}", stringify!($field), self.value.$field())),*];
                    if let Some(g) = self.value.group() { fields.push(format!("group={g}")); }
                    format!("{}({})", stringify!($name), fields.join(", "))
                }
                fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
                    other.extract::<PyRef<'_, Self>>().is_ok_and(|other| self.value == other.value)
                }
                fn __hash__(&self) -> u64 { value_hash(&self.value) }

                #[pyo3(signature = (**changes))]
                fn replace<'py>(slf: PyRef<'py, Self>, changes: Option<&Bound<'py, PyDict>>) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        crate::tuning::replacement(slf.into_pyobject(py)?.as_any(), &[$(stringify!($field),)* "group"], changes)
    }
                #[classattr]
                fn __annotations__(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
                    let annotations = PyDict::new(py);
                    $(annotations.set_item(stringify!($field), py.get_type::<PyInt>())?;)*
                    annotations.set_item("group", py.import("typing")?.getattr("Optional")?.get_item(py.get_type::<PyInt>())?)?;
                    Ok(annotations)
                }
                #[classattr]
                fn __match_args__(py: Python<'_>) -> PyResult<Bound<'_, PyTuple>> {
                    let names: &[&str] = &[$(stringify!($field)),*];
                    PyTuple::new(py, names)
                }
            }
        message_classes! { $($rest)* }
    };
    () => {};
}

macro_rules! short_messages {
    ($($name:ident $signature:tt { $($field:ident),* };)*) => {
        message_classes! { $($name $signature { $($field),* };)* }

        fn register_short(module: &Bound<'_, PyModule>) -> PyResult<()> {
            $(module.add_class::<$name>()?;)*
            Ok(())
        }

        pub fn encode_message(message: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
            if let Ok(message) = message.extract::<PyRef<'_, SysEx>>() {
                return message.value.encode().map_err(value_error);
            }
            $(if let Ok(message) = message.extract::<PyRef<'_, $name>>() {
                return message.resolved()?.encode().map_err(value_error);
            })*
            if let Some(bytes) = crate::profiles::encode_message(message) { return Ok(bytes); }
            if let Some(bytes) = crate::ci::encode_message(message) { return Ok(bytes); }
            crate::tuning::encode_message(message)
        }

        pub(crate) fn extract_message(message: &Bound<'_, PyAny>) -> PyResult<messages::AnyMessage> {
            if let Some(v) = crate::messages_midi2::extract(message) { return Ok(v); }
            if let Ok(value) = message.extract::<PyRef<'_, SysEx>>() {return Ok(messages::AnyMessage::SysEx(value.value.clone()));}
            $(if let Ok(value) = message.extract::<PyRef<'_, $name>>() {return Ok(messages::AnyMessage::$name(value.value.clone()));})*
            Err(PyTypeError::new_err("expected a primitive MIDI 1.0 message"))
        }

        pub fn to_python(py: Python<'_>, message: messages::AnyMessage) -> PyResult<Py<PyAny>> {
            to_python_with_base(py, message, Message::default())
        }

        pub(crate) fn to_python_with_base(py: Python<'_>, message: messages::AnyMessage, base: Message) -> PyResult<Py<PyAny>> {
            match message {
                messages::AnyMessage::SysEx(value) => Ok(Py::new(py,
                    PyClassInitializer::from(base).add_subclass(SysEx { value }))?.into_any()),
                $(messages::AnyMessage::$name(value) => Ok(Py::new(py,
                    PyClassInitializer::from(base).add_subclass($name::from_value(value)))?.into_any()),)*
                v => crate::messages_midi2::to_python(py, v, base),
            }
        }

        fn construct_primitive(object: &Bound<'_, PyAny>, fields: &Bound<'_, PyDict>, metadata: &Option<std::sync::Arc<Metadata>>) -> PyResult<Option<Py<PyAny>>> {
            construct_message!(object, metadata;
                SysEx => SysEx::new(&field(fields, "data")?, optional_field(fields, "group")?.as_ref()),
                $($name => $name::from_fields(fields),)*
                ParameterSet => ParameterSet::new(converted_field(fields,"parameter",strict_integer)?,converted_field(fields,"value",strict_integer)?,converted_field(fields,"resolution",strict_integer)?, optional_field(fields,"channel")?.as_ref(), &converted_field(fields,"family",|v|v.extract::<String>())?, optional_field(fields,"group")?.as_ref()),
                ParameterStep => ParameterStep::new(converted_field(fields,"parameter",strict_integer)?,converted_field(fields,"direction",strict_integer)?, optional_field(fields,"channel")?.as_ref(), &converted_field(fields,"family",|v|v.extract::<String>())?, optional_field(fields,"group")?.as_ref())
            );
            Ok(None)
        }
        pub(crate) fn construct(object: &Bound<'_, PyAny>, fields: &Bound<'_, PyDict>, metadata: &Option<std::sync::Arc<Metadata>>) -> PyResult<Py<PyAny>> {
            for construct in [
                construct_primitive,
                crate::messages_midi2::construct, crate::ump::construct,
                crate::tuning::construct, crate::profiles::construct, crate::ci::construct,
            ] {
                if let Some(value) = construct(object, fields, metadata)? { return Ok(value); }
            }
            Err(PyTypeError::new_err("unsupported native Message class"))
        }



    };
}

short_messages! {
    NoteOn (note, velocity=90, channel=None) { channel, note, velocity };
    NoteOff (note, velocity=0, channel=None) { channel, note, velocity };
    PolyPressure (note, pressure=0, channel=None) { channel, note, pressure };
    ControlChange (controller, value, channel=None) { channel, controller, value };
    ProgramChange (program, channel=None) { channel, program };
    ChannelPressure (pressure=0, channel=None) { channel, pressure };
    PitchBend (value=0, channel=None) { channel, value };
    QuarterFrame ( message_type, value) { message_type, value };
    SongPosition ( position) { position };
    SongSelect ( song) { song };
    TuneRequest () {};
    TimingClock () {};
    Start () {};
    Continue () {};
    Stop () {};
    ActiveSensing () {};
    SystemReset () {};
}

fn register_commands(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<ParameterSet>()?;
    module.add_class::<ParameterStep>()
}
pub(crate) fn command_to_python(
    py: Python<'_>,
    value: messages::AnyCommand,
    base: Message,
) -> PyResult<Py<PyAny>> {
    macro_rules! object {
        ($class:ident, $value:expr) => {
            Ok(Py::new(
                py,
                PyClassInitializer::from(base).add_subclass($class { value: $value }),
            )?
            .into_any())
        };
    }
    match value {
        messages::AnyCommand::ControlChange(value) => object!(ControlChange, value),
        messages::AnyCommand::ParameterSet(value) => object!(ParameterSet, value),
        messages::AnyCommand::ParameterStep(value) => object!(ParameterStep, value),
        messages::AnyCommand::ProgramChange(value) => object!(ProgramChange, value),
        messages::AnyCommand::ZoneConfiguration(value) => {
            crate::profiles::zone_configuration_to_python(py, value, base)
        }
    }
}
pub fn extract_command(command: &Bound<'_, PyAny>) -> PyResult<messages::AnyCommand> {
    macro_rules! extract { ($($name:ident),*) => { $(
        if let Ok(command) = command.extract::<PyRef<'_, $name>>() {
            return Ok(messages::AnyCommand::$name(command.value.clone()));
        }
    )* }; }
    if let Ok(command) = command.extract::<PyRef<'_, ControlChange>>() {
        if command.value.version() != 1 {
            return Err(PyTypeError::new_err(
                "MIDI 2 controller values require UMP encoding",
            ));
        }
        return Ok(messages::AnyCommand::ControlChange(command.value.clone()));
    }
    extract!(ParameterSet, ParameterStep);
    if let Ok(command) = command.extract::<PyRef<'_, ProgramChange>>() {
        if command.value.version() != 1 {
            return Err(PyTypeError::new_err(
                "MIDI 2 program changes use UMP encoding",
            ));
        }
        if command.value.channel().is_none() {
            return Err(PyValueError::new_err("message channel is unassigned"));
        }
        return Ok(messages::AnyCommand::ProgramChange(command.value.clone()));
    }
    crate::profiles::extract_command(command)?
        .ok_or_else(|| PyTypeError::new_err("command must be a supported compound command"))
}
pub fn command_wire(
    py: Python<'_>,
    command: &messages::AnyCommand,
) -> PyResult<(String, Py<PyDict>)> {
    let kwargs = PyDict::new(py);
    kwargs.set_item("group", command.group())?;
    let name = match command {
        messages::AnyCommand::ProgramChange(value) => {
            kwargs.set_item("program", value.program())?;
            kwargs.set_item("channel", value.channel())?;
            kwargs.set_item("version", value.version())?;
            kwargs.set_item("bank_msb", value.bank_msb())?;
            kwargs.set_item("bank_lsb", value.bank_lsb())?;
            "ProgramChange".into()
        }
        messages::AnyCommand::ControlChange(value) => {
            kwargs.set_item("controller", value.controller())?;
            kwargs.set_item("value", value.value())?;
            kwargs.set_item("channel", value.channel())?;
            kwargs.set_item("version", value.version())?;
            kwargs.set_item("group", value.group())?;
            kwargs.set_item("resolution", value.resolution())?;
            "ControlChange".into()
        }
        messages::AnyCommand::ParameterSet(value) => {
            kwargs.set_item("parameter", value.parameter())?;
            kwargs.set_item("value", value.value())?;
            kwargs.set_item("resolution", value.resolution())?;
            kwargs.set_item("channel", value.channel())?;
            kwargs.set_item("family", family_name(value.family()))?;
            "ParameterSet".into()
        }
        messages::AnyCommand::ParameterStep(value) => {
            let value = ParameterStep {
                value: value.clone(),
            };
            kwargs.set_item("parameter", value.parameter())?;
            kwargs.set_item("direction", value.direction())?;
            kwargs.set_item("channel", value.channel())?;
            kwargs.set_item("family", value.family())?;
            "ParameterStep".into()
        }
        _ => {
            let (name, fields) = crate::profiles::command_wire(command);
            for (key, value) in fields {
                kwargs.set_item(key, value)?;
            }
            name
        }
    };
    Ok((name, kwargs.unbind()))
}
fn family_name(family: middy::controls::ParameterFamily) -> &'static str {
    match family {
        middy::controls::ParameterFamily::Rpn => "rpn",
        middy::controls::ParameterFamily::Nrpn => "nrpn",
    }
}
fn parameter_family(family: &str) -> PyResult<middy::controls::ParameterFamily> {
    match family {
        "rpn" => Ok(middy::controls::ParameterFamily::Rpn),
        "nrpn" => Ok(middy::controls::ParameterFamily::Nrpn),
        _ => Err(PyValueError::new_err("family must be rpn or nrpn")),
    }
}

#[pyclass(frozen, extends = Message, module = "middy.messages")]
struct ParameterSet {
    value: messages::ParameterSet,
}
#[pymethods]
impl ParameterSet {
    #[getter]
    fn group(&self) -> Option<u8> {
        self.value.group()
    }
    #[new]
    #[pyo3(signature=(parameter, value, resolution=14, channel=None, *, family="rpn", group=None))]
    fn new(
        #[pyo3(from_py_with=strict_integer)] parameter: i64,
        #[pyo3(from_py_with=strict_integer)] value: i64,
        #[pyo3(from_py_with=strict_integer)] resolution: i64,
        channel: Option<&Bound<'_, PyAny>>,
        family: &str,
        group: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<MessageInitializer<Self>> {
        let family = parameter_family(family)?;
        let mut value = messages::ParameterSet::new(0, value)
            .and_then(|v| v.with_family(family))
            .and_then(|v| v.with_parameter(parameter))
            .and_then(|v| v.with_resolution(resolution))
            .map_err(value_error)?;
        if let Some(g) = group {
            value = value
                .with_group(integer(g, "group")?)
                .map_err(value_error)?;
        }
        if let Some(channel) = channel {
            value = value
                .with_channel(integer(channel, "channel")?)
                .map_err(value_error)?;
        }
        Ok(MessageInitializer(Self { value }))
    }
    #[getter]
    fn parameter(&self) -> u16 {
        self.value.parameter()
    }
    #[getter]
    fn value(&self) -> u16 {
        self.value.value()
    }
    #[getter]
    fn resolution(&self) -> u8 {
        self.value.resolution()
    }
    #[getter]
    fn channel(&self) -> Option<u8> {
        self.value.channel()
    }
    #[getter]
    fn family(&self) -> &'static str {
        family_name(self.value.family())
    }
    fn __repr__(slf: PyRef<'_, Self>) -> PyResult<String> {
        let py = slf.py();
        crate::tuning::render(
            slf.into_pyobject(py)?.as_any(),
            "ParameterSet",
            &[
                "parameter",
                "value",
                "resolution",
                "channel",
                "family",
                "group",
            ],
        )
    }
    fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        other
            .extract::<PyRef<'_, Self>>()
            .is_ok_and(|v| self.value == v.value)
    }
    fn __hash__(&self) -> u64 {
        value_hash(&self.value)
    }
    #[pyo3(signature=(**changes))]
    fn replace<'py>(
        slf: PyRef<'py, Self>,
        changes: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        crate::tuning::replacement(
            slf.into_pyobject(py)?.as_any(),
            &[
                "parameter",
                "value",
                "resolution",
                "channel",
                "family",
                "group",
            ],
            changes,
        )
    }
    #[classattr]
    fn __annotations__(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
        let fields = PyDict::new(py);
        for name in ["parameter", "value", "resolution"] {
            fields.set_item(name, py.get_type::<PyInt>())?;
        }
        fields.set_item(
            "channel",
            py.import("typing")?
                .getattr("Optional")?
                .get_item(py.get_type::<PyInt>())?,
        )?;
        fields.set_item("family", py.get_type::<pyo3::types::PyString>())?;
        fields.set_item(
            "group",
            py.import("typing")?
                .getattr("Optional")?
                .get_item(py.get_type::<PyInt>())?,
        )?;
        Ok(fields)
    }
    #[classattr]
    fn __match_args__() -> (&'static str, &'static str, &'static str, &'static str) {
        ("parameter", "value", "resolution", "channel")
    }
}

#[pyclass(frozen, extends = Message, module = "middy.messages")]
struct ParameterStep {
    value: messages::ParameterStep,
}
#[pymethods]
impl ParameterStep {
    #[getter]
    fn group(&self) -> Option<u8> {
        self.value.group()
    }
    #[new]
    #[pyo3(signature=(parameter, direction=1, channel=None, *, family="rpn", group=None))]
    fn new(
        #[pyo3(from_py_with=strict_integer)] parameter: i64,
        #[pyo3(from_py_with=strict_integer)] direction: i64,
        channel: Option<&Bound<'_, PyAny>>,
        family: &str,
        group: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<MessageInitializer<Self>> {
        use middy::controls::StepDirection;
        let family = parameter_family(family)?;
        let direction = match direction {
            1 => StepDirection::Increment,
            -1 => StepDirection::Decrement,
            _ => return Err(PyValueError::new_err("direction must be 1 or -1")),
        };
        let mut value = messages::ParameterStep::new(0)
            .and_then(|v| v.with_family(family))
            .and_then(|v| v.with_parameter(parameter))
            .map_err(value_error)?
            .with_direction(direction);
        if let Some(g) = group {
            value = value
                .with_group(integer(g, "group")?)
                .map_err(value_error)?;
        }
        if let Some(channel) = channel {
            value = value
                .with_channel(integer(channel, "channel")?)
                .map_err(value_error)?;
        }
        Ok(MessageInitializer(Self { value }))
    }
    #[getter]
    fn parameter(&self) -> u16 {
        self.value.parameter()
    }
    #[getter]
    fn direction(&self) -> i8 {
        match self.value.direction() {
            middy::controls::StepDirection::Increment => 1,
            middy::controls::StepDirection::Decrement => -1,
        }
    }
    #[getter]
    fn family(&self) -> &'static str {
        match self.value.family() {
            middy::controls::ParameterFamily::Rpn => "rpn",
            middy::controls::ParameterFamily::Nrpn => "nrpn",
        }
    }
    #[getter]
    fn channel(&self) -> Option<u8> {
        self.value.channel()
    }
    fn __repr__(&self) -> String {
        format!(
            "ParameterStep(parameter={}, direction={}, channel={}, family={:?})",
            self.parameter(),
            self.direction(),
            self.channel()
                .map_or_else(|| "None".into(), |v| v.to_string()),
            self.family()
        )
    }
    fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        other
            .extract::<PyRef<'_, Self>>()
            .is_ok_and(|v| self.value == v.value)
    }
    fn __hash__(&self) -> u64 {
        value_hash(&self.value)
    }
    #[pyo3(signature=(**changes))]
    fn replace<'py>(
        slf: PyRef<'py, Self>,
        changes: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        crate::tuning::replacement(
            slf.into_pyobject(py)?.as_any(),
            &["parameter", "direction", "channel", "family", "group"],
            changes,
        )
    }
    #[classattr]
    fn __annotations__(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
        let annotations = PyDict::new(py);
        annotations.set_item("parameter", py.get_type::<PyInt>())?;
        annotations.set_item("direction", py.get_type::<PyInt>())?;
        annotations.set_item(
            "channel",
            py.import("typing")?
                .getattr("Optional")?
                .get_item(py.get_type::<PyInt>())?,
        )?;
        annotations.set_item("family", py.get_type::<pyo3::types::PyString>())?;
        annotations.set_item(
            "group",
            py.import("typing")?
                .getattr("Optional")?
                .get_item(py.get_type::<PyInt>())?,
        )?;
        Ok(annotations)
    }
    #[classattr]
    fn __match_args__() -> (&'static str, &'static str, &'static str) {
        ("parameter", "direction", "channel")
    }
}

pub(crate) fn parameter_set_to_python(
    py: Python<'_>,
    value: messages::ParameterSet,
) -> PyResult<Py<PyAny>> {
    Ok(Py::new(
        py,
        PyClassInitializer::from(Message::default()).add_subclass(ParameterSet { value }),
    )?
    .into_any())
}

pub fn integer(value: &Bound<'_, PyAny>, field: &str) -> PyResult<i64> {
    let value = crate::units::field(value, field)?;
    if !value.get_type().is(value.py().get_type::<PyInt>()) {
        return Err(PyTypeError::new_err(format!("{field} must be an int")));
    }
    value
        .extract::<i64>()
        .map_err(|_| PyValueError::new_err(format!("{field} is out of range")))
}

fn payload(data: &Bound<'_, PyAny>) -> PyResult<messages::SysEx> {
    let buffer = PyBuffer::<u8>::get(data)?;
    if buffer.dimensions() != 1 || !buffer.is_c_contiguous() {
        return Err(PyTypeError::new_err(
            "data must be a one-dimensional contiguous unsigned-byte buffer",
        ));
    }
    messages::SysEx::new(buffer.to_vec(data.py())?).map_err(value_error)
}

fn value_error(error: messages::ValueError) -> PyErr {
    PyValueError::new_err(error.to_string())
}

pub(crate) fn value_hash(value: &impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// A performed note: the existing NoteOn value plus an authored duration.
#[pyclass(frozen, module = "middy.messages")]
struct Note {
    duration: Py<PyAny>,
    attack: Py<NoteOn>,
    start: Py<PyAny>,
    bends: Vec<(Py<PyAny>, f64)>,
}
#[pymethods]
impl Note {
    #[new]
    #[pyo3(signature=(duration, note, velocity=90, channel=None, *, start=None))]
    fn new(
        py: Python<'_>,
        duration: &Bound<'_, PyAny>,
        #[pyo3(from_py_with=strict_integer)] note: i64,
        #[pyo3(from_py_with=strict_integer)] velocity: i64,
        channel: Option<&Bound<'_, PyAny>>,
        start: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        validate_duration(duration)?;
        let zero = PyInt::new(py, 0);
        let start = start.unwrap_or(zero.as_any());
        validate_duration(start)?;
        Ok(Self {
            duration: duration.clone().unbind(),
            start: start.clone().unbind(),
            bends: Vec::new(),
            attack: Py::new(
                py,
                NoteOn::new(
                    note.into_pyobject(py)?.as_any(),
                    Some(velocity.into_pyobject(py)?.as_any()),
                    channel,
                    1,
                    None,
                    None,
                    None,
                )?,
            )?,
        })
    }
    #[getter]
    fn duration(&self, py: Python<'_>) -> Py<PyAny> {
        self.duration.clone_ref(py)
    }
    #[getter]
    fn start(&self, py: Python<'_>) -> Py<PyAny> {
        self.start.clone_ref(py)
    }
    fn bend(&self, at: &Bound<'_, PyAny>, semitones: f64) -> PyResult<Self> {
        validate_duration(at)?;
        if !(-48.0..=48.0).contains(&semitones) {
            return Err(PyValueError::new_err("bend must be within ±48 semitones"));
        }
        let py = at.py();
        let mut bends: Vec<_> = self
            .bends
            .iter()
            .map(|(offset, value)| (offset.clone_ref(py), *value))
            .collect();
        bends.push((at.clone().unbind(), semitones));
        Ok(Self {
            duration: self.duration.clone_ref(py),
            attack: self.attack.clone_ref(py),
            start: self.start.clone_ref(py),
            bends,
        })
    }
    #[getter]
    fn note(&self, py: Python<'_>) -> i64 {
        self.attack.borrow(py).note()
    }
    #[getter]
    fn velocity(&self, py: Python<'_>) -> i64 {
        self.attack.borrow(py).velocity()
    }
    #[getter]
    fn channel(&self, py: Python<'_>) -> Option<i64> {
        self.attack.borrow(py).channel().map(i64::from)
    }
    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        use std::fmt::Write as _;
        let mut representation = format!(
            "Note(duration={}, note={}, velocity={}, channel={}",
            self.duration.bind(py).repr()?,
            self.note(py),
            self.velocity(py),
            self.channel(py)
                .map_or_else(|| "None".into(), |v| v.to_string())
        );
        if !self.start.bind(py).eq(0)? {
            write!(representation, ", start={}", self.start.bind(py).repr()?)
                .expect("writing to a String succeeds");
        }
        representation.push(')');
        for (at, semitones) in &self.bends {
            write!(
                representation,
                ".bend(at={}, semitones={semitones:?})",
                at.bind(py).repr()?
            )
            .expect("writing to a String succeeds");
        }
        Ok(representation)
    }
}

#[pyclass(frozen, module = "middy.messages")]
struct Chord {
    notes: Py<PyTuple>,
}

#[pymethods]
impl Chord {
    #[new]
    #[pyo3(signature=(*notes))]
    fn new(notes: &Bound<'_, PyTuple>) -> PyResult<Self> {
        for note in notes {
            note.cast::<Note>()?;
        }
        Ok(Self {
            notes: notes.clone().unbind(),
        })
    }

    #[getter]
    fn notes(&self, py: Python<'_>) -> Py<PyTuple> {
        self.notes.clone_ref(py)
    }
}

type FractionParts = (i128, i128);
type AuthoredPlan = (Vec<(FractionParts, Py<PyAny>)>, FractionParts);

#[pyfunction]
pub(crate) fn schedule_notes(
    value: &Bound<'_, PyAny>,
    beat_period: &Bound<'_, PyAny>,
    channel: Option<u8>,
    limit: usize,
) -> PyResult<AuthoredPlan> {
    use middy::events::Rational;
    let py = value.py();
    let seconds = py.import("middy._units")?.getattr("sequence_duration")?;
    let resolve = |value: &Py<PyAny>| -> PyResult<messages::Beats> {
        let amount = seconds.call1((value, beat_period))?;
        let n = amount.getattr("numerator")?.extract()?;
        let d = amount.getattr("denominator")?.extract()?;
        let rational =
            Rational::new(n, d).map_err(|error| PyValueError::new_err(error.to_string()))?;
        messages::Beats::try_from(rational).map_err(value_error)
    };
    let mut remaining = limit;
    let mut resolve_note = |note: PyRef<'_, Note>| -> PyResult<messages::Note> {
        remaining = remaining
            .checked_sub(note.bends.len().saturating_add(2))
            .ok_or_else(|| PyValueError::new_err("untimed playback exceeds the sequence limit"))?;
        let mut resolved = messages::Note::new(resolve(&note.duration)?, note.note(py))
            .and_then(|v| v.with_velocity(note.velocity(py)))
            .map_err(value_error)?
            .with_start(resolve(&note.start)?);
        if let Some(channel) = note.channel(py) {
            resolved = resolved.with_channel(channel).map_err(value_error)?;
        }
        for (at, semitones) in &note.bends {
            resolved = resolved
                .bend(resolve(at)?, *semitones)
                .map_err(value_error)?;
        }
        Ok(resolved)
    };
    let notes = if let Ok(chord) = value.extract::<PyRef<'_, Chord>>() {
        chord
            .notes
            .bind(py)
            .iter()
            .map(|note| resolve_note(note.extract()?))
            .collect::<PyResult<_>>()?
    } else {
        vec![resolve_note(value.extract()?)?]
    };
    // At 60 BPM, these already-resolved beat magnitudes are seconds.
    let (events, duration) = messages::schedule_notes(&notes, Some(60.into()), channel)
        .map_err(|error| PyValueError::new_err(error.to_string()))?;
    if events.len() > limit {
        return Err(PyValueError::new_err(
            "untimed playback exceeds the sequence limit",
        ));
    }
    let parts = |v: Rational| (v.numerator(), v.denominator());
    let events = events
        .into_iter()
        .map(|(at, message)| Ok((parts(at), crate::event_values::to_python(py, message)?)))
        .collect::<PyResult<_>>()?;
    Ok((events, parts(duration)))
}

#[pyclass(frozen, module = "middy.messages")]
struct Rest {
    duration: Py<PyAny>,
}
#[pymethods]
impl Rest {
    #[new]
    #[pyo3(signature=(duration))]
    fn new(duration: &Bound<'_, PyAny>) -> PyResult<Self> {
        validate_duration(duration)?;
        Ok(Self {
            duration: duration.clone().unbind(),
        })
    }
    #[getter]
    fn duration(&self, py: Python<'_>) -> Py<PyAny> {
        self.duration.clone_ref(py)
    }
    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        Ok(format!("Rest(duration={})", self.duration.bind(py).repr()?))
    }
}

fn validate_duration(value: &Bound<'_, PyAny>) -> PyResult<()> {
    value
        .py()
        .import("middy._units")?
        .getattr("musical_duration")?
        .call1((value,))?;
    Ok(())
}

pub fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<Message>()?;
    module.add_class::<Note>()?;
    module.add_class::<Chord>()?;
    module.add_class::<Rest>()?;
    module.add_class::<SysEx>()?;
    register_short(module)?;
    register_commands(module)?;
    crate::messages_midi2::register(module)?;
    Ok(())
}

#[cfg(test)]
#[test]
fn python_representation_contract_and_measurements() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn count_allocations(
        class: *mut pyo3::ffi::PyTypeObject,
        count: pyo3::ffi::Py_ssize_t,
    ) -> *mut pyo3::ffi::PyObject {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { pyo3::ffi::PyType_GenericAlloc(class, count) }
    }

    struct RestoreAllocator(*mut pyo3::ffi::PyTypeObject, pyo3::ffi::allocfunc);
    impl Drop for RestoreAllocator {
        fn drop(&mut self) {
            unsafe { (*self.0).tp_alloc = Some(self.1) };
        }
    }

    Python::initialize();
    Python::attach(|py| {
        let module = PyModule::new(py, "middy.messages").unwrap();
        register(&module).unwrap();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("test/native");
        PyModule::import(py, "sys")
            .unwrap()
            .getattr("path")
            .unwrap()
            .call_method1("insert", (0, path.to_str().unwrap()))
            .unwrap();
        let fixture = PyModule::import(py, "middy_messages").unwrap();
        fixture
            .getattr("verify")
            .unwrap()
            .call1((&module,))
            .map_err(|error| error.display(py))
            .unwrap();
        let note = module.getattr("NoteOn").unwrap().call1((60,)).unwrap();
        let class = py.get_type::<NoteOn>().as_type_ptr();
        // The GIL stays held while this class's allocator is temporarily replaced.
        let original = unsafe { (*class).tp_alloc.unwrap() };
        assert!(std::ptr::fn_addr_eq(
            original,
            pyo3::ffi::PyType_GenericAlloc as pyo3::ffi::allocfunc
        ));
        let restore = RestoreAllocator(class, original);
        unsafe { (*class).tp_alloc = Some(count_allocations) };
        let changes = PyDict::new(py);
        changes.set_item("stream", "keyboard").unwrap();
        changes.set_item("note", 61).unwrap();
        ALLOCATIONS.store(0, Ordering::Relaxed);
        let tagged = note.call_method("replace", (), Some(&changes)).unwrap();
        assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 1);
        assert_eq!(tagged.getattr("note").unwrap().extract::<u8>().unwrap(), 61);
        assert_eq!(note.getattr("note").unwrap().extract::<u8>().unwrap(), 60);
        assert_eq!(
            tagged
                .getattr("stream")
                .unwrap()
                .extract::<String>()
                .unwrap(),
            "keyboard"
        );
        ALLOCATIONS.store(0, Ordering::Relaxed);
        let copied = tagged.call_method0("replace").unwrap();
        assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 1);
        assert!(copied.eq(&tagged).unwrap());
        changes.set_item("note", 128).unwrap();
        ALLOCATIONS.store(0, Ordering::Relaxed);
        assert!(tagged.call_method("replace", (), Some(&changes)).is_err());
        assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0);
        drop(restore);
        let results = fixture
            .getattr("measure")
            .unwrap()
            .call1((&module,))
            .unwrap();
        println!("prototype: {results}");
        println!(
            "Rust value sizes: NoteOn={}, SysEx={}",
            std::mem::size_of::<messages::NoteOn>(),
            std::mem::size_of::<messages::SysEx>()
        );
    });
}

pub(crate) fn strict_integer(value: &Bound<'_, PyAny>) -> PyResult<i64> {
    integer(value, "value")
}
