# Open issues and risks

Source audit of `70633cf` on 2026-11-22. The findings below describe code
paths and give scenarios for regression tests. Driver-dependent outcomes and
release readiness still need the evidence listed at the end.

## Reliability and data integrity

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

## Release, privacy, and remaining qualification

- **P2. Verify a release candidate and its public dependency contract.**
  Candidate CI supports pull requests and manual runs, checks installed
  playback, and separates locked development dependencies from a fresh
  index-only install. The macOS candidate wheel passed its locked-dependency
  smoke tests. The index-only install chose uFor 0.1.0, which rejected the
  current `FrequencyTable(frequencies=...)` API. A compatible published
  dependency and a passing `check_index` run are still required before public
  distribution. Hosted verification is pending.
  ([Workflow](../.github/workflows/ci.yml),
  [tuning integration](../middy/tuning.py),
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
