# Open issues and risks

Source audit of `70633cf` on 2026-11-22. The findings below describe code
paths and give scenarios for regression tests. Driver-dependent outcomes and
release readiness still need the evidence listed at the end.

## Address first

### P1. Hot-plug outputs can miss MPE setup

The chord plan configures the zone and bend ranges once, before the first
attack. A matched output group reconciles devices before each logical send.
If a new synth appears after setup, it can receive an attack or pitch bend
with its old configuration, or receive zone disable without having received
zone enable. A second synth that joins between the two attacks of a staggered
chord is enough to expose the problem. Keep the destinations stable for the
chord or require setup for each destination before admitting its first event.
Test a topology change between setup and attack with a fake catalog.
([Chord planner](../crates/middy/src/sequence.rs#L247),
[output group](../crates/middy/src/output_group.rs#L158))

### P1. Interrupted automatic MPE leaves the instrument configured

Zone disable is the last event in the chord. Cancellation, an interrupt, or
a send failure can skip it, along with some releases and bend resets. MIDI 1
byte output cleanup tracks notes and selected controllers, so closing can
release notes and center bends but does not restore the MPE zone. UMP output
does not get that byte-based cleanup. A retained output can return a cancelled
report with the synth still sounding or its channel layout changed. Define zone
ownership, cleanup on interrupted playback, and how to report incomplete
cleanup. Exercise cancellation after setup, after an attack, and during a
compound setup command. Successful playback also leaves the configured member
bend range, so using these channels alongside another application can change
that application's pitch response.
([MPE setup and teardown](../crates/middy/src/sequence.rs#L247),
[Rust cancellation](../crates/middy/src/output.rs#L661),
[output cleanup](../crates/middy/src/output.rs#L833),
[Python cancellation](../src/playback.rs#L182))

### P1. Python close can race with output connection publication

`Output.connect()` checks the Python client's state and publishes
`_connection` only after a native open that releases the GIL. Meanwhile,
`close()` can set `_closed`, observe no connection, and return. The opener can
then publish a live native connection on the closed client. Subsequent
`close()` calls short-circuit on `_closed`. Test with a barrier in native
open, covering both explicit connect and the lazy connect in `send()`. Make
ownership of the opening connection explicit across the operation, including
retirement when close wins. Review virtual creation against the same rule.
([Python connect](../middy/_clients.py#L506),
[Python close](../middy/_clients.py#L737),
[native open](../src/output.rs#L304))

### P1. Concurrent Python sends can reuse the same MPE channels

Each `send()` creates its own timeline and channel assignment. Playback
releases the GIL while waiting, and the native connection protects individual
sends. Two threads sharing one Python output can start separate expressive
chords, both using channel 2, or disable the zone while the other chord is
still active. Ordinary messages can also target those channels during the
wait. Define whether concurrent sends on one output are supported and enforce
the chosen rule for the whole playback operation. A fake clock can pause one
call after its first attack while a second call attempts to send.
([Python send](../middy/_clients.py#L540),
[playback wait](../src/playback.rs#L182),
[channel allocation](../crates/middy/src/sequence.rs#L211))

### P1. A stuck native call can block discovery, close, and retirement

In-process worker startup and command replies use unbounded waits. The catalog
waits for its first snapshot, and explicit followed-input close joins its
discovery thread, which can be waiting on a source health check or retirement.
A driver that never returns can strand the caller and the native client slot.
Detaching the worker at drop avoids a join there, but provides no deadline for
these explicit operations. This is a documented limitation with a larger
impact for shared catalogs. Test with a gated fake worker and define the
supported timeout and recovery guarantees for supervised processes and
in-process callers. A timed-out driver call must retain any callback state
the driver can still access.
([Native worker](../crates/middy/src/transport/worker.rs#L43),
[first catalog snapshot](../crates/middy/src/transport/scanner.rs#L181),
[followed input close](../crates/middy/src/transport/all.rs#L176),
[ownership contract](../README.ai.md#L889))

## Reliability and data integrity

### P2. Output followers treat a recoverable empty match as terminal

When the matching group has no destinations, or refresh raises while opening
one of several destinations, `broadcast()` stores the error permanently.
A first send with no device, followed by hot-plug and a second send on the
same output, therefore keeps returning the original failure. A new endpoint
can also prevent a healthy existing destination from receiving traffic.
Decide separately how to report a send attempted with no destinations, a
retryable topology change, and an uncertain partial delivery. The latter
requires explicit handling to avoid transmitting a note or command twice.
([Refresh](../crates/middy/src/output_group.rs#L83),
[sticky failure](../crates/middy/src/output_group.rs#L158),
[public hot-plug contract](../doc/examples.md#L64))

### P2. One polling error ends an entire catalog subscription

A single failed `ports()` call exits the poll loop and replaces the last
snapshot with an error. Reopening can allocate a fresh catalog. Existing
followers have no recovery path through that catalog, even if discovery
recovers on the next attempt. An input follower also publishes its terminal
error after retiring the sources, so a stalled retirement can postpone the
visible failure. Test one failed scan followed by a successful scan with the
same endpoints, and distinguish transient enumeration from terminal backend
failure before changing retry behavior.
([Catalog error publication](../crates/middy/src/transport/scanner.rs#L165),
[polling](../crates/middy/src/transport/scanner.rs#L222),
[input error publication](../crates/middy/src/transport/all.rs#L415))

### P2. Untimed playback can exhaust memory or wait forever before sending

Both APIs collect an entire untimed iterable, then allocate a prepared plan.
An infinite iterator sends nothing, and a large one retains both source values
and prepared packets at peak. Cancellation is checked outside collection, so
even a fast infinite generator can keep the caller busy after cancellation.
Preserve the existing preflight guarantee for finite batches and define a
bounded admission or explicitly streaming contract for longer input. Include
cancellation during iteration in the regression cases.
([Rust preparation](../crates/middy/src/output.rs#L408),
[Python collection](../middy/_clients.py#L623),
[Python cancellation](../src/playback.rs#L182))

### P2. A failed overwrite can destroy the last usable backup

`write(..., overwrite=True)` truncates the named file before it reads the
first record. An invalid record, iterator exception, or disk error can replace
a valid tuning dump or capture with an empty file or a valid-looking prefix.
The low-level partial-file behavior is documented. The one-call backup
operation would benefit from committing a completed sibling file atomically,
with explicit treatment of exclusive creation and pre-existing destinations.
Test failure before the first record and after a valid prefix.
([Rust write contract](../crates/middy/src/file_io.rs#L9),
[Rust file open](../crates/middy/src/file_io.rs#L457),
[Python write](../src/file_io.rs#L400))

### P2. A SysEx backup can succeed without any SysEx

The `.syx` writer skips all non-SysEx events. A capture from the wrong
device, or a request that gets no reply, can therefore produce an empty
successful backup. The file write API returns no counts to distinguish that
case from an actual backup. Keep mixed-traffic selection and make the empty
outcome detectable for callers using the backup example. Include gap and
discontinuity behavior in any regression test of the reporting contract.
([Writer filter](../crates/middy/src/file_io.rs#L506),
[backup example](../doc/examples.md#L163))

### P2. Reconnect churn can exhaust a recording's archive descriptor budget

An attached recorder declares each new stream and clock, while the archive
allows 128 descriptors across both categories for its entire lifetime.
Followed input creates new identities on reconnection. A long-running
session with only a few devices can hit this limit even when its queue is
empty and disk space is ample. Memory recording accumulates records outside
the file writer's registry, so the same limit can surface only when saving
the result. Test repeated connection generations against the archive
budget and decide how to expose capacity or rotate long recordings.
([Recording declarations](../crates/middy/src/input_recording.rs#L111),
[archive registry](../crates/middy/src/archive.rs#L165),
[memory recorder](../crates/middy/src/recording.rs#L183))

## API, diagnostics, and maintenance

- **P3. Python file errors lose their causes and error codes.** The stream
  adapter stringifies a caller-owned stream exception, then the outer
  conversion creates a plain `OSError` from text. Path errors also lose
  their numeric code. This makes missing-file, permission, and interrupted
  I/O handling depend on message text. Preserve Python causes and map native
  error codes where available. ([File adapter](../src/files.rs#L49),
  [conversion](../src/files.rs#L130))
- **P3. Equivalent Rust and Python playback can round differently.** The
  Rust tuple form prepares each scalar as a separate segment and sums
  rounded nanoseconds. A Rust iterable and adjacent Python scalar values
  retain fractions within one segment. Three one-third-beat rests at 120 BPM
  end at 500,000,001 ns in a Rust tuple and 500,000,000 ns in one rational
  batch. A fake-clock test can determine whether composition should preserve
  exact elapsed time across ordinary segments.
  ([Rust tuple expansion](../crates/middy/src/output.rs#L164),
  [Rust segment timing](../crates/middy/src/output.rs#L421),
  [Python batching](../middy/_clients.py#L597))
- **P3. Public names and documentation hide important behavior.**
  `set_scan_interval` is process-wide, but there is one polling worker per
  backend, direction, and subprocess configuration. The example says there
  is one shared scanner. The MPE reference describes explicit setup only
  and says bend ranges are never configured. Python `repr(Note)` omits
  both start offsets and bends, so plain and expressive notes can look
  identical in logs. Update the contracts and make the authored timing and
  expression inspectable. ([Scanner key](../crates/middy/src/transport/scanner.rs#L57),
  [scanner example](../doc/examples.md#L36),
  [MPE reference](../README.ai.md#L631),
  [note representation](../src/messages.rs#L1320))
- **P3. Playback and port policy have overlapping owners.** The Rust output
  module combines source normalization, scheduling, transport encoding,
  teardown tracking, port-pattern parsing and tests. The Python client
  module combines input, recording, output, playback and lifecycle code.
  The shared chord planner still crosses through Python fraction conversion,
  Rust beat values at a fixed 60 BPM, Python message conversion, and native
  packet preparation. Python compound sends can expand the same command
  during preparation, validation and cleanup tracking. Measure the live
  path before optimizing and give timing and prepared-command handling
  clearer ownership when changing these paths. File length alone is a weak
  reason to split a module. ([Rust output](../crates/middy/src/output.rs),
  [Python clients](../middy/_clients.py),
  [Python note bridge](../src/messages.rs#L1360),
  [native command validation](../src/output.rs#L206))

## Release, privacy, and remaining qualification

- **P2. Published releases are the first automated cross-platform gate.**
  The workflow triggers on a published release and tests CPython 3.13,
  while package metadata admits later versions. The wheel install first
  resolves dependencies from the index, then installs the exported lock
  dependencies. The repository obtains uFor from a pinned Git revision but
  the wheel declares unversioned `ufor`. Before publication, verify that
  an ordinary wheel install resolves a compatible uFor without the
  repository lock, run the wheel matrix on a candidate, and include
  `test_output.py` in the installed-wheel subset. The current workflow
  builds and uploads artifacts, so it leaves publication policy to the
  owner. ([Workflow trigger](../.github/workflows/ci.yml#L3),
  [wheel install and test list](../.github/workflows/ci.yml#L138),
  [package metadata](../pyproject.toml#L5),
  [development source](../pyproject.toml#L43))
- **P3. Shared captures can disclose device and performance information.**
  `.middy` archives include device names, backend IDs and connection
  identities. Timestamped performances and opaque SysEx can also contain
  material that users intend to keep private, including instrument settings.
  There is no general way to redact arbitrary manufacturer payloads while
  preserving a restorable backup. Document what to review before attaching
  a capture to a public issue or sharing qualification artifacts.
  ([Port serialization](../crates/middy/src/archive.rs#L311),
  [SysEx backup example](../doc/examples.md#L163))
- **Native resource and lifetime evidence remains open.** Failed in-process
  disposal deliberately retains state and consumes one of the 32 native
  client slots because freeing state that a driver can still reach would be
  unsafe. A synthetic CoreMIDI capacity regression covers that policy.
  Verify active-callback teardown, handle and thread pressure, service
  failure, unplug/replug, and physical MIDI 1 and UMP interoperability on
  the target backends. Windows WinMM, Linux ALSA sequencer/raw and applicable
  UMP variants, and more CoreMIDI configurations are open. This source
  review found no confirmed use-after-free and does not qualify those
  native paths. The distribution checks cover vendored notices and source
  provenance. Rerun them for the candidate artifacts before release.
  ([Disposal ownership](../crates/middy/src/transport/native.rs#L497),
  [platform matrix and procedures](qualification.md#open-gates),
  [distribution checks](../test/release/artifacts.py))

## Additional work beyond the prompt

None.
