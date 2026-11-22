# Open issues and risks

Source audit of `70633cf` on 2026-11-22. The findings below describe code
paths and give scenarios for regression tests. Driver-dependent outcomes and
release readiness still need the evidence listed at the end.

## Release and remaining qualification

- **P2. PyPI release deferred pending a compatible uFor version.**
  Candidate CI supports pull requests and manual runs, checks installed
  playback, and separates locked development dependencies from a fresh
  index-only install. All nine hosted candidate jobs passed. A macOS wheel
  built from the unpacked source distribution also passed the distribution
  checks. The index-only install chose uFor 0.1.0, which rejected the current
  `FrequencyTable(frequencies=...)` API. Keep using the exact GitHub commit in
  `uv.lock` for checkout installs and candidate CI. Resume PyPI release work
  once a compatible uFor version is published. Require a passing
  `check_index` run for the release candidate before publishing middy.
  ([Candidate run](https://github.com/rec/middy/actions/runs/45019531170),
  [Workflow](../.github/workflows/ci.yml),
  [tuning integration](../middy/tuning.py),
  [development source](../pyproject.toml#L43),
  [PyPI metadata validation](https://github.com/pypi/warehouse/blob/main/warehouse/forklift/metadata.py))
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
