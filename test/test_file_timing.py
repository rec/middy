"""Standard files retain exact tempo/SMPTE time and require deliberate exports."""

from contextlib import nullcontext
from io import BytesIO
from pathlib import Path
from typing import Literal

import pytest

from middy import clip, events, file_io, messages, smf, translation, ump


def song(format: int = 1, division: int = 480) -> bytes:
    stream = BytesIO()
    writer = smf.Writer(
        stream, header=smf.Header(format=format, tracks=2, division=division)
    )
    for value in [
        smf.Event(track=0, tick=0, record=smf.Meta(kind=81, data=b"\x07\xa1\x20")),
        smf.Event(track=0, tick=240, record=smf.Meta(kind=81, data=b"\x03\xd0\x90")),
        smf.Event(track=0, tick=480, record=smf.Meta(kind=47, data=b"")),
        smf.Event(
            track=1, tick=240, record=messages.NoteOn(channel=1, note=60, velocity=100)
        ),
        smf.Event(
            track=1, tick=480, record=messages.NoteOn(channel=1, note=61, velocity=100)
        ),
        smf.Event(track=1, tick=480, record=smf.Meta(kind=47, data=b"")),
    ]:
        writer.write(value)
    writer.finish()
    return stream.getvalue()


@pytest.mark.parametrize("kind", ["merged", "smf", "clip"])
@pytest.mark.parametrize("exit_with_error", [False, True])
def test_reader_context_closes_on_early_exit_and_preserves_borrowed_stream(
    kind: Literal["merged", "smf", "clip"], exit_with_error: bool
) -> None:
    if kind == "clip":
        source = BytesIO()
        writer = clip.Writer(source)
        for m in [
            ump.DeltaClockstamp(ticks=0),
            ump.DeltaClockstampTicksPerQuarter(ticks=480),
            ump.DeltaClockstamp(ticks=0),
            ump.start_of_clip(),
            ump.DeltaClockstamp(ticks=1),
            messages.NoteOn(60, channel=1).replace(group=1),
            ump.DeltaClockstamp(ticks=0),
            ump.end_of_clip(),
        ]:
            writer.write(m)
        writer.finish()
        source.seek(0)
    else:
        source = BytesIO(song())
    reader = file_io.Reader(source)
    with (
        pytest.raises(RuntimeError, match="body failed")
        if exit_with_error
        else nullcontext()
    ):
        with reader as opened:
            assert opened is reader
            view = opened.merged() if kind == "merged" else opened.timed(stream="song")
            assert next(view) is not None
            if exit_with_error:
                raise RuntimeError("body failed")
    assert not source.closed
    with pytest.raises(ValueError, match="closed"):
        next(view)
    with pytest.raises(ValueError, match="closed"):
        with reader:
            pytest.fail("closed reader was entered")
    reader.close()


def test_incremental_smf_timeline_matches_file_timing_and_finishes_fragments() -> None:
    with file_io.Reader(BytesIO(song())) as reader:
        merged = reader.merged()
        timeline = smf.Timeline(merged.header, stream="incremental")
        values = [v for e in merged for v in timeline.feed(e)] + timeline.finish()
    with file_io.Reader(BytesIO(song())) as reader:
        reference = list(reader.timed(stream="reference"))
        assert reader.complete and reader.problem is None
    assert values == reference
    assert timeline.origin.clock == timeline.clock
    for a, b in zip(values, reference, strict=True):
        assert isinstance(a, messages.Message) and isinstance(b, messages.Message)
        assert a.stream == "incremental"
        assert a.timestamp is not None and b.timestamp is not None
        assert a.timestamp.seconds == b.timestamp.seconds
    partial = smf.Timeline(
        smf.Header(format=0, tracks=1, division=480), stream="partial"
    )
    assert (
        partial.feed(
            smf.Event(
                track=0, tick=0, record=smf.SysExFragment(status=240, data=b"\x7d")
            )
        )
        == []
    )
    assert isinstance(partial.finish()[0], events.StreamDiagnostic)
    assert partial.finish() == []
    with pytest.raises(OSError, match="sequence"):
        smf.Timeline(smf.Header(format=2, tracks=1, division=480), stream="s")
    with pytest.raises(OSError, match="merged time order"):
        timeline.feed(smf.Event(track=0, tick=0, record=smf.Meta(kind=47, data=b"")))


def test_incremental_clip_timeline_acquires_clock_and_preserves_leading_time() -> None:
    data = BytesIO()
    writer = clip.Writer(data)
    for m in [
        ump.DeltaClockstamp(ticks=0),
        ump.DeltaClockstampTicksPerQuarter(ticks=480),
        ump.DeltaClockstamp(ticks=0),
        ump.start_of_clip(),
        ump.DeltaClockstamp(ticks=480),
        messages.NoteOn(60, version=2, channel=1),
        ump.DeltaClockstamp(ticks=0),
        ump.end_of_clip(),
    ]:
        writer.write(m)
    writer.finish()
    timeline = clip.Timeline(stream="incremental")
    assert timeline.clock is None and timeline.origin is None
    values = [
        v for e in clip.Reader(BytesIO(data.getvalue())) for v in timeline.feed(e)
    ]
    assert timeline.clock is not None and timeline.origin is not None
    assert timeline.origin.clock == timeline.clock
    assert len(values) == 1 and isinstance(values[0], messages.NoteOn)
    assert values[0].timestamp is not None
    assert values[0].timestamp.seconds == events.Rational(numerator=1, denominator=2)
    assert timeline.finish() == []


@pytest.mark.parametrize("mode", ["records", "merged", "timed"])
def test_reader_allows_only_one_consumption_mode(mode: str) -> None:
    with file_io.Reader(BytesIO(song())) as reader:
        if mode == "records":
            next(reader)
        elif mode == "merged":
            reader.merged()
        else:
            reader.timed(stream="song")
        with pytest.raises(ValueError, match="before iteration"):
            reader.merged()
        with pytest.raises(ValueError, match="before iteration"):
            reader.timed(stream="song")
        if mode != "records":
            with pytest.raises(ValueError, match="selected view"):
                next(reader)


def test_merged_uses_open_stream_and_respects_its_start_offset(tmp_path: Path) -> None:
    path = tmp_path / "song.mid"
    path.write_bytes(song())
    with file_io.Reader(path) as reader:
        path.rename(tmp_path / "moved.mid")
        assert len(list(reader.merged())) == 6
        assert reader.complete
    source = BytesIO(b"prefix" + song())
    source.seek(6)
    with file_io.Reader(source) as reader:
        assert len(list(reader.merged())) == 6
    assert not source.closed


def test_performance_modes_reject_unsupported_inputs_and_options() -> None:
    with file_io.Reader(BytesIO(b"\xf0\x7d\xf7")) as reader:
        with pytest.raises(OSError, match="Standard MIDI File"):
            reader.merged()
    with file_io.Reader(BytesIO(b"\xf0\x7d\xf7")) as reader:
        with pytest.raises(OSError, match="Standard MIDI File or MIDI Clip"):
            reader.timed(stream="song")
    with file_io.Reader(BytesIO(song())) as reader:
        with pytest.raises(OSError, match="only to Clip"):
            reader.timed(stream="song", include_configuration=True)

    class Nonseekable(BytesIO):
        def seek(self, offset: int, whence: int = 0) -> int:
            raise OSError("not seekable")

    with file_io.Reader(Nonseekable(song())) as reader:
        with pytest.raises(OSError, match="not seekable"):
            reader.merged()
    assert len(list(file_io.Reader(Nonseekable(song())))) == 7


def test_raw_ump_word_counts_and_standalone_meta_validation() -> None:
    assert [
        ump.word_count(n) for n in (0x20903C64, 0x40903C00, 0xB0000000, 0xF0000000)
    ] == [1, 2, 3, 4]
    for n in (True, 1.0, "1"):
        with pytest.raises(TypeError):
            ump.word_count(n)  # ty: ignore[invalid-argument-type]
    for n in (-1, 2**32):
        with pytest.raises(ValueError):
            ump.word_count(n)
    assert smf.validate_meta(81, b"\x07\xa1\x20") is None
    assert smf.validate_meta(1, memoryview(b"text")) is None
    with pytest.raises(OSError, match="byte 23"):
        smf.validate_meta(81, b"\x00", offset=23)
    with pytest.raises(OSError):
        smf.validate_meta(81, b"\x00\x00\x00")


def test_merged_tempo_map_is_exact_and_keeps_leading_silence() -> None:
    reader = file_io.Reader(BytesIO(song())).timed(stream="song")
    origin = reader.origin
    values = list(reader)
    assert len(values) == 2 and all(isinstance(v, messages.Message) for v in values)
    first = values[0]
    assert isinstance(first, messages.Message) and first.timestamp is not None
    assert first.timestamp.seconds == events.Rational(numerator=1, denominator=4)
    second = values[1]
    assert isinstance(second, messages.Message) and second.timestamp is not None
    assert second.timestamp.seconds == events.Rational(numerator=3, denominator=8)
    assert events.PlaybackPlan(origin=origin).deadline(first).delay_ns == 250_000_000
    merged = list(file_io.Reader(BytesIO(song())).merged())
    assert [(v.tick, v.track) for v in merged] == [
        (0, 0),
        (240, 0),
        (240, 1),
        (480, 0),
        (480, 1),
        (480, 1),
    ]
    with pytest.raises(OSError, match="track merge limit"):
        file_io.Reader(BytesIO(song())).merged(track_limit=1)
    assert file_io.Reader(BytesIO(song())).timed(stream="song").clock != reader.clock


def test_independent_sequences_and_smpte_export_have_explicit_loss_reports() -> None:
    data = song(format=2, division=0xE328)
    with pytest.raises(OSError, match="sequence"):
        file_io.Reader(BytesIO(data)).timed(stream="s")
    reader = file_io.Reader(BytesIO(data)).timed(stream="s", sequence=1)
    values = list(reader)
    value = values[0]
    assert isinstance(value, messages.Message)
    header = smf.Header(format=0, tracks=1, division=480)
    denied = smf.export_event(value, header=header, origin=reader.origin)
    assert (
        denied.status == translation.TranslationStatus.Quantized and denied.events == []
    )
    accepted = smf.export_event(
        value, header=header, origin=reader.origin, allow_loss=True
    )
    assert accepted.events and accepted.rounding_error_ticks.numerator != 0
    wide = messages.NoteOn(
        version=2,
        group=2,
        channel=1,
        note=60,
        velocity=65535,
        attribute_type=0,
        attribute_data=0,
    ).replace(stream=value.stream, timestamp=value.timestamp, span=value.span)
    assert (
        smf.export_event(
            wide, header=header, origin=reader.origin, allow_loss=True
        ).status
        == translation.TranslationStatus.ContextRequired
    )


def test_clip_timed_reader_omits_file_markers_and_preserves_tempo() -> None:
    stream = BytesIO()
    writer = clip.Writer(stream)
    for value in [
        ump.DeltaClockstamp(ticks=0),
        ump.DeltaClockstampTicksPerQuarter(ticks=480),
        ump.tempo(group=1, bpm=100.0),
        ump.DeltaClockstamp(ticks=0),
        ump.start_of_clip(),
        ump.tempo(group=1, bpm=120.0),
        ump.DeltaClockstamp(ticks=480),
        messages.NoteOn(
            version=2,
            group=1,
            channel=1,
            note=60,
            velocity=65535,
            attribute_type=0,
            attribute_data=0,
        ),
        ump.DeltaClockstamp(ticks=0),
        ump.end_of_clip(),
    ]:
        writer.write(value)
    writer.finish()
    with file_io.Reader(BytesIO(stream.getvalue())) as reader:
        assert all(isinstance(v, clip.Entry) for v in reader)
    with file_io.Reader(BytesIO(stream.getvalue())) as reader:
        timed = reader.timed(stream="clip")
        origin = timed.origin
        values = list(timed)
    assert len(values) == 2 and all(isinstance(v, messages.Message) for v in values)
    note = values[1]
    assert isinstance(note, messages.Message) and note.timestamp is not None
    assert note.timestamp.seconds == events.Rational(numerator=1, denominator=2)
    assert events.PlaybackPlan(origin=origin).deadline(note).delay_ns == 500_000_000
    with file_io.Reader(BytesIO(stream.getvalue())) as reader:
        configured = list(reader.timed(stream="clip", include_configuration=True))
    assert len(configured) > len(values)
    with file_io.Reader(BytesIO(stream.getvalue())) as reader:
        with pytest.raises(OSError, match="sequence applies only"):
            reader.timed(stream="clip", sequence=0)


def test_timed_failure_is_reported_by_the_owning_reader() -> None:
    source = BytesIO()
    writer = smf.Writer(source, header=smf.Header(format=0, tracks=1, division=480))
    writer.write(smf.Event(track=0, tick=0, record=smf.Meta(kind=33, data=b"\x01")))
    writer.write(smf.Event(track=0, tick=0, record=smf.Meta(kind=47, data=b"")))
    writer.finish()
    with file_io.Reader(BytesIO(source.getvalue())) as reader:
        timed = reader.timed(stream="song")
        with pytest.raises(OSError, match="nonzero SMF port"):
            next(timed)
        assert not reader.complete and reader.problem
        assert list(timed) == []
        assert not reader.complete


def test_split_sysex_is_assembled_per_track_when_other_tracks_interleave() -> None:
    stream = BytesIO()
    writer = smf.Writer(stream, header=smf.Header(format=1, tracks=2, division=480))
    for record in [
        smf.Event(
            track=0, tick=0, record=smf.SysExFragment(status=240, data=b"\x7d\x01")
        ),
        smf.Event(
            track=0, tick=2, record=smf.SysExFragment(status=247, data=b"\x02\xf7")
        ),
        smf.Event(track=0, tick=2, record=smf.Meta(kind=47, data=b"")),
        smf.Event(
            track=1, tick=1, record=messages.NoteOn(channel=1, note=60, velocity=1)
        ),
        smf.Event(track=1, tick=2, record=smf.Meta(kind=47, data=b"")),
    ]:
        writer.write(record)
    writer.finish()
    values = list(file_io.Reader(BytesIO(stream.getvalue())).timed(stream="s"))
    assert len(values) == 2
    assert [v for v in values if isinstance(v, messages.Message)] == [
        messages.NoteOn(channel=1, note=60, velocity=1),
        messages.SysEx(data=b"\x7d\x01\x02"),
    ]
    sysex = values[1]
    assert isinstance(sysex, messages.Message) and sysex.span is not None
    assert sysex.span.first_packet == 0 and sysex.span.last_packet == 2
