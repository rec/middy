//! Full notes and rests use typed elapsed time or exact musical beats.

use crate::events::{Payload, Rational};
use crate::messages::{AnyCommand, AnyMessage, NoteOff, NoteOn, PitchBend, ValueError};
use crate::mpe::{Allocator, ZoneConfiguration};
use std::cmp::Ordering;
use std::io;
use std::time::Duration;

/// Quarter-note beats, retained as an exact nonnegative fraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Beats(Rational);

impl Beats {
    pub fn new(numerator: u64, denominator: u64) -> io::Result<Self> {
        Ok(Self(Rational::new(numerator.into(), denominator.into())?))
    }

    pub fn value(self) -> Rational {
        self.0
    }
}

impl From<u32> for Beats {
    fn from(value: u32) -> Self {
        Self(Rational::new(value.into(), 1).expect("whole beats fit Rational"))
    }
}

impl TryFrom<Rational> for Beats {
    type Error = ValueError;

    fn try_from(value: Rational) -> Result<Self, ValueError> {
        if value.numerator() < 0 {
            return Err(ValueError("beats must be nonnegative".into()));
        }
        Ok(Self(value))
    }
}

/// Internal scheduling retains fractions until cumulative deadlines are rounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoteDuration {
    Time(Duration),
    Beats(Beats),
}

impl NoteDuration {
    /// Resolve musical time at an explicit tempo in quarter-note beats per minute.
    pub fn seconds(self, bpm: Option<Rational>) -> io::Result<Rational> {
        match self {
            Self::Time(duration) => Rational::new(duration.as_nanos() as i128, 1_000_000_000),
            Self::Beats(beats) => {
                let bpm = bpm.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "beats require a tempo")
                })?;
                if bpm.numerator() <= 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "tempo must be positive",
                    ));
                }
                beats
                    .value()
                    .multiply(60, 1)?
                    .multiply(bpm.denominator(), bpm.numerator())
            }
        }
    }
}

impl From<Duration> for NoteDuration {
    fn from(value: Duration) -> Self {
        Self::Time(value)
    }
}

impl From<Beats> for NoteDuration {
    fn from(value: Beats) -> Self {
        Self::Beats(value)
    }
}

/// An attack and release separated by its duration. Never synthesized by input.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Note {
    duration: NoteDuration,
    attack: NoteOn,
    start: NoteDuration,
    bends: Vec<(NoteDuration, i64)>,
}

impl Note {
    pub fn new(duration: impl Into<NoteDuration>, note: i64) -> Result<Self, ValueError> {
        Ok(Self {
            duration: duration.into(),
            attack: NoteOn::new(note)?,
            start: Duration::ZERO.into(),
            bends: Vec::new(),
        })
    }
    pub fn with_velocity(mut self, velocity: i64) -> Result<Self, ValueError> {
        self.attack = self.attack.with_velocity(velocity)?;
        Ok(self)
    }
    pub fn with_channel(mut self, channel: i64) -> Result<Self, ValueError> {
        self.attack = self.attack.with_channel(channel)?;
        Ok(self)
    }
    pub fn duration(&self) -> NoteDuration {
        self.duration
    }

    /// Delay this note relative to its chord or sequence position.
    pub fn with_start(mut self, start: impl Into<NoteDuration>) -> Self {
        self.start = start.into();
        self
    }

    pub fn start(&self) -> NoteDuration {
        self.start
    }

    /// Set pitch relative to the original note at an offset from its attack.
    pub fn bend(mut self, at: impl Into<NoteDuration>, semitones: f64) -> Result<Self, ValueError> {
        if !(-48.0..=48.0).contains(&semitones) {
            return Err(ValueError("bend must be within ±48 semitones".into()));
        }
        let value = (semitones * 8192.0 / 48.0).round_ties_even() as i64;
        self.bends.push((at.into(), value.min(8191)));
        Ok(self)
    }

    pub fn note(&self) -> u8 {
        self.attack.note()
    }

    pub fn velocity(&self) -> u8 {
        self.attack.velocity() as u8
    }

    pub fn channel(&self) -> Option<u8> {
        self.attack.channel()
    }
}

/// Notes whose start offsets share a common origin.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    notes: Vec<Note>,
}

impl Chord {
    pub fn new(notes: impl IntoIterator<Item = Note>) -> Self {
        Self {
            notes: notes.into_iter().collect(),
        }
    }

    pub fn notes(&self) -> &[Note] {
        &self.notes
    }
}

#[derive(Clone, Copy)]
enum Action {
    Release,
    Attack,
    Bend(i64),
}

pub(crate) fn check_event_limit(notes: &[Note], mut remaining: usize) -> io::Result<()> {
    for note in notes {
        remaining = remaining
            .checked_sub(note.bends.len().saturating_add(2))
            .ok_or_else(|| invalid("untimed playback exceeds the sequence limit"))?;
    }
    Ok(())
}

/// Shared authored-note planner for the Rust and Python output APIs.
#[doc(hidden)]
pub struct NoteSchedule {
    pub events: Vec<(Rational, Payload)>,
    pub duration: Rational,
    pub members: u8,
}

#[doc(hidden)]
pub fn schedule_notes(
    notes: &[Note],
    bpm: Option<Rational>,
    channel: Option<u8>,
    encoding: crate::transport::Encoding,
) -> io::Result<NoteSchedule> {
    let mpe = notes.iter().any(|note| !note.bends.is_empty());
    if mpe && (channel.is_some() || notes.iter().any(|note| note.channel().is_some())) {
        return Err(invalid(
            "automatic MPE requires automatic channel assignment",
        ));
    }

    let mut actions = Vec::new();
    let mut end = Rational::from(0);
    for (i, note) in notes.iter().enumerate() {
        let start = note.start.seconds(bpm)?;
        let duration = note.duration.seconds(bpm)?;
        let release = start.checked_add(duration)?;
        if compare_time(release, end).is_gt() {
            end = release;
        }
        if mpe && (duration.numerator() == 0 || note.velocity() == 0) {
            return Err(invalid("MPE notes require positive duration and velocity"));
        }
        actions.push((start, false, i, Action::Attack));
        for &(at, value) in &note.bends {
            let at = at.seconds(bpm)?;
            if !compare_time(at, duration).is_lt() {
                return Err(invalid("bend offset must be before the note ends"));
            }
            actions.push((start.checked_add(at)?, false, i, Action::Bend(value)));
        }
        actions.push((release, duration.numerator() != 0, i, Action::Release));
    }
    // At a shared boundary, release sounding notes before reusing their channels.
    actions.sort_by(|a, b| compare_time(a.0, b.0).then_with(|| b.1.cmp(&a.1)));

    let mut allocator = Allocator::new(ZoneConfiguration::new(1, 15)?);
    let mut voices = vec![None; notes.len()];
    let mut members = 0;
    let mut events = Vec::new();
    for (at, _, i, action) in actions {
        let note = &notes[i];
        let channel = i64::from(channel.or(note.channel()).unwrap_or(1));
        let message = match action {
            Action::Attack if mpe => {
                let voice = allocator.allocate(note.note().into(), note.velocity().into())?;
                members = members.max(voice.channel() - 1);
                let attack = voice.note_on();
                voices[i] = Some(voice);
                AnyMessage::NoteOn(attack)
            }
            Action::Attack => AnyMessage::NoteOn(note.attack.clone().with_channel(channel)?),
            Action::Release if mpe => {
                let voice = voices[i].take().expect("attack precedes release");
                let release = allocator.release(voice.id(), 0)?;
                events.push((at, AnyMessage::NoteOff(release).into()));
                AnyMessage::PitchBend(PitchBend::new()?.with_channel(voice.channel().into())?)
            }
            Action::Release => {
                AnyMessage::NoteOff(NoteOff::new(note.note().into())?.with_channel(channel)?)
            }
            Action::Bend(value) => {
                let voice = voices[i].as_ref().expect("bend is within note");
                AnyMessage::PitchBend(
                    PitchBend::new()?
                        .with_value(value)?
                        .with_channel(voice.channel().into())?,
                )
            }
        };
        events.push((at, message.into()));
    }
    if mpe {
        let zero = Rational::from(0);
        let mut setup = vec![(
            zero,
            Payload::Command(AnyCommand::ZoneConfiguration(ZoneConfiguration::new(
                1,
                members.into(),
            )?)),
        )];
        for channel in 2..=i64::from(members) + 1 {
            setup.push((
                zero,
                Payload::Command(AnyCommand::ParameterSet(
                    crate::controls::pitch_bend_sensitivity(channel, 48, 0)?,
                )),
            ));
            setup.push((
                zero,
                AnyMessage::PitchBend(PitchBend::new()?.with_channel(channel)?).into(),
            ));
        }
        setup.append(&mut events);
        setup.push((
            end,
            Payload::Command(AnyCommand::ZoneConfiguration(ZoneConfiguration::new(1, 0)?)),
        ));
        events = setup;
        if encoding == crate::transport::Encoding::Ump {
            for (_, payload) in &mut events {
                *payload = match payload {
                    Payload::Command(command) => Payload::Command(command.clone().with_group(1)?),
                    Payload::Message(message) => Payload::Message(message.clone().with_group(1)?),
                    Payload::Ump(_) => unreachable!("authored MPE uses MIDI 1 messages"),
                };
            }
        }
    }
    Ok(NoteSchedule {
        events,
        duration: end,
        members,
    })
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

// Compare nonnegative fractions without multiplying large denominators.
fn compare_time(a: Rational, b: Rational) -> Ordering {
    let (mut n, mut d) = (a.numerator(), a.denominator());
    let (mut m, mut e) = (b.numerator(), b.denominator());
    let mut order = Ordering::Equal;
    loop {
        let whole = (n / d).cmp(&(m / e));
        if !whole.is_eq() {
            return if order.is_lt() {
                whole.reverse()
            } else {
                whole
            };
        }
        let (r, s) = (n % d, m % e);
        if r == 0 || s == 0 {
            let remainder = r.cmp(&s);
            return if order.is_lt() {
                remainder.reverse()
            } else {
                remainder
            };
        }
        ((n, d), (m, e)) = ((d, r), (e, s));
        order = if order.is_lt() {
            Ordering::Greater
        } else {
            Ordering::Less
        };
    }
}

/// Advance a sequence without sending a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rest {
    duration: NoteDuration,
}

impl Rest {
    pub fn new(duration: impl Into<NoteDuration>) -> Self {
        Self {
            duration: duration.into(),
        }
    }

    pub fn duration(self) -> NoteDuration {
        self.duration
    }
}
