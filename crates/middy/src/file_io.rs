//! Bounded file I/O with byte locations. No native MIDI initialization.
use crate::messages::{Message, PrimitiveMessage};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub(crate) mod syx;

pub const DEFAULT_LIMIT: usize = 1_048_576;

/// Write a fallible record stream, choosing the format from the filename.
/// With overwrite enabled, replace the destination only after successful completion.
/// Exclusive creation can leave a partial new file on failure.
/// SYX output selects SysEx messages, discards timing and other MIDI traffic,
/// and rejects empty selections, recorded gaps and discontinuities.
pub fn write<T: WriteRecord>(
    records: impl IntoIterator<Item = io::Result<T>>,
    filename: impl AsRef<std::path::Path>,
    overwrite: bool,
) -> io::Result<()> {
    write_file(
        filename.as_ref(),
        overwrite,
        |error| error,
        |writer| {
            for record in records {
                writer.write(&record?)?;
            }
            Ok(())
        },
    )
}

/// Shared path-writing transaction used by the language binding.
#[doc(hidden)]
pub fn write_file<E>(
    path: &Path,
    overwrite: bool,
    map_error: impl Fn(io::Error) -> E,
    fill: impl FnOnce(&mut Writer) -> Result<(), E>,
) -> Result<(), E> {
    let path = std::path::absolute(path).map_err(&map_error)?;
    let extension = path_extension(&path);
    validate_extension(extension).map_err(&map_error)?;
    let (file, pending) = if overwrite {
        let (file, pending) = PendingFile::new(&path).map_err(&map_error)?;
        (file, Some(pending))
    } else {
        (
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(&map_error)?,
            None,
        )
    };
    let mut writer = Writer::new(file, extension, DEFAULT_LIMIT).map_err(&map_error)?;
    fill(&mut writer)?;
    if extension.eq_ignore_ascii_case("syx") && !writer.sysex_written {
        return Err(map_error(invalid(
            0,
            "SYX backup contains no SysEx messages",
        )));
    }
    writer.finish().map_err(&map_error)?;
    if let Some(mut pending) = pending {
        std::fs::rename(&pending.0, &path).map_err(&map_error)?;
        pending.0.clear();
    }
    Ok(())
}

struct PendingFile(PathBuf);

impl PendingFile {
    fn new(path: &Path) -> io::Result<(std::fs::File, Self)> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        for _ in 0..32 {
            let identity = crate::events::fresh_identity("write")?;
            let path = parent.join(format!(".middy-{:016x}.tmp", {
                use std::hash::{BuildHasher, RandomState};
                RandomState::new().hash_one(identity)
            }));
            match options.open(&path) {
                Ok(file) => return Ok((file, Self(path))),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a temporary MIDI file",
        ))
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

pub fn invalid(offset: u64, reason: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("byte {offset}: {reason}"),
    )
}

#[derive(Debug)]
struct ContextError {
    location: String,
    error: io::Error,
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.location, self.error)
    }
}

impl std::error::Error for ContextError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

pub(crate) fn at_location(location: String, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), ContextError { location, error })
}
pub struct Source<R> {
    pub inner: R,
    pub offset: u64,
    pub limit: usize,
}
impl<R: Read> Source<R> {
    pub fn new(inner: R, limit: usize) -> io::Result<Self> {
        if limit == 0 {
            return Err(invalid(0, "payload limit must be positive"));
        }
        Ok(Self {
            inner,
            offset: 0,
            limit,
        })
    }
    pub fn byte(&mut self) -> io::Result<u8> {
        Ok(self.array::<1>()?[0])
    }
    pub fn optional_byte(&mut self) -> io::Result<Option<u8>> {
        let mut b = [0];
        loop {
            match self.inner.read(&mut b) {
                Ok(0) => return Ok(None),
                Ok(_) => {
                    self.offset += 1;
                    return Ok(Some(b[0]));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(at_location(format!("byte {}", self.offset), e)),
            }
        }
    }
    pub fn array<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let mut b = [0; N];
        self.read_exact(&mut b)?;
        Ok(b)
    }
    pub fn read_exact(&mut self, mut b: &mut [u8]) -> io::Result<()> {
        while !b.is_empty() {
            match self.inner.read(b) {
                Ok(0) => return Err(invalid(self.offset, "truncated input")),
                Ok(n) => {
                    self.offset += n as u64;
                    b = &mut b[n..];
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(at_location(format!("byte {}", self.offset), e)),
            }
        }
        Ok(())
    }
    pub fn payload(&mut self, n: usize) -> io::Result<Vec<u8>> {
        if n > self.limit {
            return Err(invalid(self.offset, "payload exceeds configured limit"));
        }
        let mut b = vec![0; n];
        self.read_exact(&mut b)?;
        Ok(b)
    }
}
pub fn write_all(w: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    w.write_all(bytes)
}

/// A lossless file record. MIDI files begin with their header, followed by events.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Record {
    Archive(crate::archive::Record),
    Header(crate::smf::Header),
    Event(crate::smf::Event),
    Clip(crate::clip::Entry),
}
impl Record {
    pub fn channel(&self) -> Option<u8> {
        use crate::{archive, events::Payload, messages::AnyCommand};
        match self {
            Self::Event(event) => event.channel(),
            Self::Archive(archive::Record::Event(message)) => match message.as_ref() {
                Payload::Message(message) => message.channel(),
                Payload::Ump(message) => {
                    let words = crate::ump::encode(message).ok()?;
                    matches!(words[0] >> 28, 2 | 4).then_some(((words[0] >> 16) as u8 & 15) + 1)
                }
                Payload::Command(command) => match command {
                    AnyCommand::ControlChange(v) => v.channel(),
                    AnyCommand::ParameterSet(v) => v.channel(),
                    AnyCommand::ParameterStep(v) => v.channel(),
                    AnyCommand::ProgramChange(v) => v.channel(),
                    AnyCommand::ZoneConfiguration(v) => Some(v.manager_channel()),
                },
            },
            _ => None,
        }
    }
    pub fn transpose(self, semitones: i64) -> Self {
        use crate::messages::Message;
        match self {
            Self::Event(event) => Self::Event(event.transpose(semitones)),
            Self::Archive(crate::archive::Record::Event(message)) => Self::Archive(
                crate::archive::Record::Event(Box::new(message.transpose(semitones))),
            ),
            other => other,
        }
    }
}

type Prefixed<R> = std::io::Chain<std::io::Cursor<Vec<u8>>, R>;
enum Input<R> {
    Archive(crate::archive::Reader<Prefixed<R>>),
    Smf(crate::smf::Reader<Prefixed<R>>),
    Syx(syx::Reader<Prefixed<R>>),
    Clip(crate::clip::Reader<Prefixed<R>>),
}

/// Detect MIDI files, MIDI Clips, SysEx streams and middy archives from contents.
/// Use format-specific readers for other formats and detailed codec control.
pub struct Reader<R = std::io::BufReader<std::fs::File>> {
    input: Input<R>,
    header: Option<crate::smf::Header>,
    complete: bool,
    problem: Option<String>,
    started: bool,
    limit: usize,
}
impl Reader {
    pub fn open(path: impl AsRef<std::path::Path>) -> io::Result<Self> {
        Self::new(
            std::io::BufReader::new(std::fs::File::open(path)?),
            DEFAULT_LIMIT,
            false,
            false,
        )
    }
}
impl<R: Read> Reader<R> {
    pub fn new(
        mut inner: R,
        limit: usize,
        recover: bool,
        preserve_unknown: bool,
    ) -> io::Result<Self> {
        if limit == 0 {
            return Err(invalid(0, "payload limit must be positive"));
        }
        let mut prefix = Vec::new();
        (&mut inner).take(1).read_to_end(&mut prefix)?;
        let syx = prefix.is_empty() || prefix[0] == 240;
        if !syx {
            (&mut inner).take(3).read_to_end(&mut prefix)?;
        }
        let smf = prefix == b"MThd";
        let clip = prefix == b"SMF2";
        let inner = std::io::Cursor::new(prefix).chain(inner);
        let (input, header) = if syx {
            if recover || preserve_unknown {
                return Err(invalid(
                    0,
                    "archive recovery options do not apply to SysEx files",
                ));
            }
            (Input::Syx(syx::Reader::new(inner, limit)?), None)
        } else if smf {
            if recover || preserve_unknown {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "archive recovery options do not apply to MIDI files",
                ));
            }
            let reader = crate::smf::Reader::new(inner, limit)?;
            let header = Some(reader.header.clone());
            (Input::Smf(reader), header)
        } else if clip {
            if recover {
                return Err(invalid(0, "archive recovery does not apply to Clip files"));
            }
            (
                Input::Clip(crate::clip::Reader::new(inner, preserve_unknown)?),
                None,
            )
        } else {
            (
                Input::Archive(crate::archive::Reader::new(
                    inner,
                    limit,
                    recover,
                    preserve_unknown,
                )?),
                None,
            )
        };
        Ok(Self {
            input,
            header,
            complete: false,
            problem: None,
            started: false,
            limit,
        })
    }
    pub fn complete(&self) -> bool {
        self.complete
    }
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }
}
impl<R: Read> Iterator for Reader<R> {
    type Item = io::Result<Record>;
    fn next(&mut self) -> Option<Self::Item> {
        self.started = true;
        if let Some(header) = self.header.take() {
            return Some(Ok(Record::Header(header)));
        }
        match &mut self.input {
            Input::Archive(reader) => {
                let result = reader.next().map(|v| v.map(Record::Archive));
                self.complete = reader.complete;
                self.problem = reader.problem.clone();
                result
            }
            Input::Smf(reader) => {
                let result = reader.next().map(|v| v.map(Record::Event));
                if let Some(Err(error)) = &result {
                    self.problem = Some(error.to_string());
                }
                if result.is_none() && self.problem.is_none() {
                    self.complete = true;
                }
                result
            }
            Input::Syx(reader) => {
                let result = reader.next().map(|v| {
                    v.map(|m| {
                        Record::Archive(crate::archive::Record::Event(Box::new(
                            crate::events::Payload::Message(crate::messages::AnyMessage::SysEx(m)),
                        )))
                    })
                });
                if let Some(Err(error)) = &result {
                    self.problem = Some(error.to_string());
                }
                if result.is_none() && self.problem.is_none() {
                    self.complete = true;
                }
                result
            }
            Input::Clip(reader) => {
                let result = reader.next().map(|v| v.map(Record::Clip));
                if let Some(Err(error)) = &result {
                    self.problem = Some(error.to_string());
                }
                if result.is_none() && self.problem.is_none() {
                    self.complete = true;
                }
                result
            }
        }
    }
}

/// One bounded lookahead event per track, in stable tick/track order.
pub struct MergedEvents<R> {
    reader: crate::smf::MergedReader<R>,
}
impl<R: Read + Seek> MergedEvents<R> {
    pub fn header(&self) -> &crate::smf::Header {
        &self.reader.header
    }
}
impl<R: Read + Seek> Iterator for MergedEvents<R> {
    type Item = io::Result<crate::smf::Event>;
    fn next(&mut self) -> Option<Self::Item> {
        self.reader.next()
    }
}

enum TimedInput<R> {
    Smf(crate::file_timing::SmfReader<R>),
    Clip(crate::file_timing::ClipReader<Prefixed<R>>),
}
/// Timestamped performance messages. Reading does not wait in real time.
pub struct TimedMessages<R> {
    input: TimedInput<R>,
}
impl<R: Read + Seek> TimedMessages<R> {
    pub fn header(&self) -> Option<&crate::smf::Header> {
        match &self.input {
            TimedInput::Smf(reader) => Some(reader.header()),
            TimedInput::Clip(_) => None,
        }
    }
    pub fn clock(&self) -> &crate::events::Clock {
        match &self.input {
            TimedInput::Smf(reader) => reader.clock(),
            TimedInput::Clip(reader) => reader.clock(),
        }
    }
    pub fn origin(&self) -> crate::events::Timestamp {
        match &self.input {
            TimedInput::Smf(reader) => reader.origin(),
            TimedInput::Clip(reader) => reader.origin(),
        }
    }
}
impl<R: Read + Seek> Iterator for TimedMessages<R> {
    type Item = io::Result<crate::live::Item>;
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.input {
            TimedInput::Smf(reader) => reader.next(),
            TimedInput::Clip(reader) => reader.next(),
        }
    }
}

impl<R: Read + Seek> Reader<R> {
    /// Select merged SMF events before reading begins. Consumes this reader.
    pub fn merged(
        self,
        sequence: Option<u16>,
        track_limit: usize,
        buffer_limit: usize,
    ) -> io::Result<MergedEvents<R>> {
        if self.started {
            return Err(invalid(0, "select a reading mode before iteration starts"));
        }
        let Input::Smf(reader) = self.input else {
            return Err(invalid(0, "merged requires a Standard MIDI File"));
        };
        let source = reader.into_source();
        let (_, mut inner) = source.inner.into_inner();
        let rewind = i64::try_from(source.offset)
            .map_err(|_| invalid(0, "MIDI header exceeds seek range"))?;
        inner.seek(SeekFrom::Current(-rewind))?;
        Ok(MergedEvents {
            reader: crate::smf::MergedReader::new(
                inner,
                self.limit,
                track_limit,
                buffer_limit,
                sequence,
            )?,
        })
    }

    /// Select timestamped SMF/Clip messages before reading begins. SMF merging
    /// requires seeking. Clip configuration is emitted only when requested.
    pub fn timed(
        self,
        stream: String,
        sequence: Option<u16>,
        track_limit: usize,
        buffer_limit: usize,
        include_configuration: bool,
    ) -> io::Result<TimedMessages<R>> {
        if self.started {
            return Err(invalid(0, "select a reading mode before iteration starts"));
        }
        let input = match self.input {
            Input::Smf(_) => {
                if include_configuration {
                    return Err(invalid(
                        0,
                        "include_configuration applies only to Clip files",
                    ));
                }
                let limit = self.limit;
                let merged = self.merged(sequence, track_limit, buffer_limit)?;
                TimedInput::Smf(crate::file_timing::SmfReader::new(
                    merged.reader,
                    stream,
                    sequence,
                    limit,
                )?)
            }
            Input::Clip(reader) => {
                if sequence.is_some() {
                    return Err(invalid(0, "sequence applies only to Standard MIDI Files"));
                }
                if track_limit != 128 || buffer_limit != 8_388_608 {
                    return Err(invalid(0, "merge limits apply only to Standard MIDI Files"));
                }
                TimedInput::Clip(crate::file_timing::ClipReader::new(
                    reader,
                    stream,
                    include_configuration,
                )?)
            }
            _ => {
                return Err(invalid(
                    0,
                    "timed requires a Standard MIDI File or MIDI Clip",
                ))
            }
        };
        Ok(TimedMessages { input })
    }
}

enum Output<W> {
    Archive(crate::archive::Writer<W>),
    AwaitingHeader(W),
    Smf(crate::smf::Writer<W>),
    Syx(W),
}
/// Choose the output format from .mid/.midi, .syx or .middy; creation is exclusive.
/// Call finish() to validate completion and report final write errors.
pub struct Writer<W = std::fs::File> {
    output: Option<Output<W>>,
    limit: usize,
    sysex_written: bool,
}
impl Writer {
    pub fn create(path: impl AsRef<std::path::Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let extension = path_extension(path);
        validate_extension(extension)?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Self::new(file, extension, DEFAULT_LIMIT)
    }
}
fn path_extension(path: &Path) -> &str {
    path.extension().and_then(|v| v.to_str()).unwrap_or("")
}
fn validate_extension(extension: &str) -> io::Result<()> {
    match extension.to_ascii_lowercase().as_str() {
        "mid" | "midi" | "middy" | "syx" => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected .mid, .midi, .syx or .middy destination",
        )),
    }
}
impl<W: Write + std::io::Seek> Writer<W> {
    /// For streams, specify the same extension used by a filename destination.
    pub fn new(inner: W, extension: &str, limit: usize) -> io::Result<Self> {
        validate_extension(extension)?;
        if limit == 0 {
            return Err(invalid(0, "payload limit must be positive"));
        }
        let output = if extension.eq_ignore_ascii_case("middy") {
            Output::Archive(crate::archive::Writer::new(inner, limit)?)
        } else if extension.eq_ignore_ascii_case("syx") {
            Output::Syx(inner)
        } else {
            Output::AwaitingHeader(inner)
        };
        Ok(Self {
            output: Some(output),
            limit,
            sysex_written: false,
        })
    }
    /// SYX output selects SysEx messages and ignores clock/stream declarations,
    /// metadata and other MIDI traffic. Gaps and discontinuities are errors.
    pub fn write(&mut self, record: &(impl WriteRecord + ?Sized)) -> io::Result<()> {
        record.write_to(self)
    }
    fn write_record(&mut self, record: &Record) -> io::Result<()> {
        match (self.output.as_mut(), record) {
            (
                Some(Output::Syx(writer)),
                Record::Archive(crate::archive::Record::Event(message)),
            ) => {
                if let crate::events::Payload::Message(crate::messages::AnyMessage::SysEx(
                    message,
                )) = message.as_ref()
                {
                    syx::write(writer, message, self.limit)?;
                    self.sysex_written = true;
                }
                Ok(())
            }
            (
                Some(Output::Syx(_)),
                Record::Archive(
                    crate::archive::Record::Clock(_)
                    | crate::archive::Record::Stream { .. }
                    | crate::archive::Record::Metadata { .. },
                ),
            ) => Ok(()),
            (
                Some(Output::Syx(_)),
                Record::Archive(
                    crate::archive::Record::Gap { .. }
                    | crate::archive::Record::Discontinuity { .. },
                ),
            ) => Err(invalid(0, "SYX cannot preserve a gap or discontinuity")),
            (Some(Output::Archive(writer)), Record::Archive(record)) => writer.write(record),
            (Some(Output::Smf(writer)), Record::Event(event)) => writer.write(event),
            (Some(Output::AwaitingHeader(_)), Record::Header(header)) => {
                let Some(Output::AwaitingHeader(inner)) = self.output.take() else {
                    unreachable!()
                };
                self.output = Some(Output::Smf(crate::smf::Writer::new(
                    inner,
                    header.clone(),
                    self.limit,
                )?));
                Ok(())
            }
            (None, _) => Err(invalid(0, "writer closed or failed")),
            _ => Err(invalid(
                0,
                "record does not match destination format or MIDI header order",
            )),
        }
    }
    pub fn finish(&mut self) -> io::Result<()> {
        self.finish_with_status(true, "")
    }
    pub fn finish_with_status(&mut self, complete: bool, reason: &str) -> io::Result<()> {
        match self.output.as_mut() {
            Some(Output::Archive(writer)) => writer.finish(complete, reason)?,
            Some(Output::Smf(writer)) => {
                if !complete || !reason.is_empty() {
                    return Err(invalid(0, "completion status applies only to archives"));
                }
                writer.finish()?;
            }
            Some(Output::Syx(writer)) => {
                if !complete || !reason.is_empty() {
                    return Err(invalid(0, "completion status applies only to archives"));
                }
                writer.flush()?;
            }
            Some(Output::AwaitingHeader(_)) => {
                return Err(invalid(0, "MIDI header must be written first"))
            }
            None => return Err(invalid(0, "writer closed or failed")),
        }
        self.output = None;
        Ok(())
    }
}

/// Values accepted by the generic writer: file records and primitive messages.
pub trait WriteRecord {
    fn write_to<W: Write + std::io::Seek>(&self, writer: &mut Writer<W>) -> io::Result<()>;
}
impl WriteRecord for Record {
    fn write_to<W: Write + std::io::Seek>(&self, writer: &mut Writer<W>) -> io::Result<()> {
        writer.write_record(self)
    }
}
impl WriteRecord for crate::archive::Record {
    fn write_to<W: Write + std::io::Seek>(&self, writer: &mut Writer<W>) -> io::Result<()> {
        writer.write_record(&Record::Archive(self.clone()))
    }
}
impl<T: Clone + Into<crate::events::Payload>> WriteRecord for T {
    fn write_to<W: Write + std::io::Seek>(&self, writer: &mut Writer<W>) -> io::Result<()> {
        writer.write_record(&Record::Archive(crate::archive::Record::Event(Box::new(
            self.clone().into(),
        ))))
    }
}

macro_rules! profile_records {
    ($module:ident: $($name:ident),*) => { $(impl WriteRecord for crate::$module::$name {
        fn write_to<W: Write + std::io::Seek>(&self, writer: &mut Writer<W>) -> io::Result<()> {
            let wire = self.encode()?;
            let message = crate::messages::SysEx::new(wire[1..wire.len()-1].to_vec())?;
            let message = if let Some(metadata) = self.metadata() { message.with_metadata(metadata.clone())? } else { message };
            message.write_to(writer)
        }
    })* };
}
profile_records!(gm: GmMode);
profile_records!(universal: IdentityRequest,IdentityReply,MasterVolume,MasterFineTuning,MasterCoarseTuning);
profile_records!(timecode: FullFrame);
profile_records!(ci: Management);
profile_records!(tuning: TuningMessage,BulkDumpRequest,BulkDump,ScaleOctaveDump,NoteChange,ScaleOctaveChange);
