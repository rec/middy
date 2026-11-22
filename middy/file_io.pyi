"""Detect input formats; select output formats from filenames.

SYX output selects SysEx messages, discards timing and other MIDI traffic,
and rejects recorded gaps and discontinuities.
"""

from collections.abc import Iterable, Iterator
from os import PathLike
from types import TracebackType
from typing import BinaryIO, Literal

from . import archive, clip, events, messages, smf

def write(
    records: Reader
    | Iterable[
        events.Clock
        | archive.Stream
        | messages.Message
        | archive.Gap
        | archive.Metadata
        | archive.Discontinuity
        | archive.Opaque
        | smf.Header
        | smf.Event
    ],
    filename: str | PathLike[str],
    overwrite: bool = False,
) -> None:
    """Write records to a path. Overwrite replaces it after successful completion.

    A SYX backup must contain at least one SysEx message.
    """
    ...

class Reader:
    """Own one cursor; choose ordinary, merged or timed iteration before reading.

    Closing Reader invalidates its views. Borrowed streams remain open.
    """
    def __init__(
        self,
        source: str | PathLike[str] | BinaryIO,
        *,
        payload_limit: int | str = 1_048_576,
        recover: bool = False,
        preserve_unknown: bool = False,
    ) -> None: ...
    def __iter__(
        self,
    ) -> Iterator[
        events.Clock
        | archive.Stream
        | messages.Message
        | archive.Gap
        | archive.Metadata
        | archive.Discontinuity
        | archive.Opaque
        | smf.Header
        | smf.Event
        | clip.Entry
    ]: ...
    def __next__(
        self,
    ) -> (
        events.Clock
        | archive.Stream
        | messages.Message
        | archive.Gap
        | archive.Metadata
        | archive.Discontinuity
        | archive.Opaque
        | smf.Header
        | smf.Event
        | clip.Entry
    ): ...
    def merged(
        self,
        *,
        sequence: int | None = None,
        track_limit: int = 128,
        buffer_limit: int | str = 8_388_608,
    ) -> MergedEvents: ...
    def timed(
        self,
        *,
        stream: str,
        sequence: int | None = None,
        track_limit: int = 128,
        buffer_limit: int | str = 8_388_608,
        include_configuration: bool = False,
    ) -> TimedMessages: ...
    def __enter__(self) -> Reader: ...
    def __exit__(
        self,
        kind: type[BaseException] | None,
        error: BaseException | None,
        traceback: TracebackType | None,
    ) -> None: ...
    def close(self) -> None: ...
    @property
    def complete(self) -> bool: ...
    @property
    def problem(self) -> str | None: ...

class MergedEvents:
    @property
    def header(self) -> smf.Header: ...
    def __iter__(self) -> Iterator[smf.Event]: ...
    def __next__(self) -> smf.Event: ...

class TimedMessages:
    @property
    def header(self) -> smf.Header | None: ...
    @property
    def clock(self) -> events.Clock: ...
    @property
    def origin(self) -> events.Timestamp: ...
    def __iter__(self) -> Iterator[messages.Message | events.StreamDiagnostic]: ...
    def __next__(self) -> messages.Message | events.StreamDiagnostic: ...

class Writer:
    def __init__(
        self,
        destination: str | PathLike[str] | BinaryIO,
        *,
        format: Literal["mid", "midi", "syx", "middy"] | None = None,
        payload_limit: int | str = 1_048_576,
    ) -> None: ...
    def write(
        self,
        record: events.Clock
        | archive.Stream
        | messages.Message
        | archive.Gap
        | archive.Metadata
        | archive.Discontinuity
        | archive.Opaque
        | smf.Header
        | smf.Event,
    ) -> None: ...
    def finish(self, *, complete: bool = True, reason: str = "") -> None: ...
    def __enter__(self) -> Writer: ...
    def __exit__(
        self,
        kind: type[BaseException] | None,
        error: BaseException | None,
        traceback: TracebackType | None,
    ) -> None: ...
