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

## Reliability and data integrity

### P2. Large packets can exceed memory budgets during untimed preflight

Both APIs now cap untimed source items and generated events, and observe
cancellation between iterator calls. A single packet can still carry a large
buffer, and Python retains input values while building its prepared plan.
Bound retained packet bytes as well as item count, and verify that exceeding
the byte budget rejects the batch before sending any of it.
([Rust preparation](../crates/middy/src/output.rs),
[Python collection](../middy/_clients.py))

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
