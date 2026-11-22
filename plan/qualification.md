# Remaining platform and resource qualification

Additional work beyond the prompt

None.

Qualification is specific to OS, architecture, backend, protocol, transport mode
and workload. No particular Linux board or host is required. Historical Mac
software-loopback and synthetic measurements remain in Git history; they do not
qualify current revisions, physical instruments or other platforms.

## Open gates

| Scope | Remaining evidence |
| --- | --- |
| macOS CoreMIDI MIDI 1 and UMP | Other versions/architectures, physical interoperability, faults and deployment resource behavior |
| Windows WinMM | An agreed loopback route and runtime checks on the target host |
| Windows MIDI Services | SDK/build integration and native UMP qualification; currently disabled |
| Linux ALSA sequencer/raw and available UMP variants | Separately authorized runtime checks |
| Resources | Parent/child memory and CPU, thread/handle counts, sustained load and overload behavior against agreed application budgets |
| Release | A compatible public uFor release and a passing `check_index` run for the candidate. Archive-format freeze, future compatibility promises, and publication remain owner decisions. |

See the staged [Linux procedure](linux-qualification.md) and
[open issues](issues.md). Compilation and device-free tests do not establish
native runtime or physical-device behavior.

The [candidate run for `cc3df4d`](https://github.com/rec/middy/actions/runs/45019531170)
passed device-free native and installed-wheel testing on Linux, both macOS
architectures and Windows. It included WinMM teardown and ANSI/Unicode
discovery regressions and CPython 3.14 on Linux. It used the locked dependencies.

## Setup and toolchain policy

Use the current installed Rust compiler locally. Minimum-version compatibility
belongs to release CI; do not install older compilers locally for qualification.
Record the actual compiler without making minimum-version claims from that run.

Use an available supported Python version and record its exact version. No local
interpreter matrix is required. Install non-editably with locked dependencies and
selected extras. Consult [build instructions](../README.ai.md) and the
[release workflow](../.github/workflows/ci.yml) for prerequisites and checks.
A missing prerequisite is an open gate, not permission to install tools.

Use a complete checkout and a new evidence directory. Record commit/dirty status,
OS/kernel/libc, CPU/RAM, storage/free space, C++/CMake/Rust/Python versions,
backend/protocol/mode, power/thermal settings and background load.
Run Python suites and linked Cargo tests sequentially because callback tests
rebuild binding artifacts. Build the wheel from the sdist and test it outside
the checkout. Retain logs, dynamic dependencies, license/provenance contents,
wheel identity and all pass/fail/skip counts.

## Existing measurement tools

- [Rust codec/file workloads](../crates/middy/examples/qualification.rs) measure
  batch wall time for messages, commands, SysEx, tuning, Clip and archives.
- [Python workloads](../test/qualification/measure.py) measure imports, codecs,
  files and paced/burst recording. Default pytest does not collect these files.
- The same Python file supervises native cases through
  [native.py](../test/qualification/native.py), with child-process deadlines.
  Build the [Rust transport example](../crates/middy/examples/qualification_transport.rs)
  before selecting its case. A missing binary or backend is a skip, not a pass.
- [CoreMIDI access](../test/qualification/access.py) and
  [discovery/reopen](../test/qualification/reopen.py) probes diagnose macOS
  access/discovery failures; they are not recurring release requirements.

For an authorized device-free baseline:

```bash
uv sync --frozen --no-editable --python 3.13 --all-extras
cargo build --locked --release -p middy --examples
target/release/examples/qualification
uv run --no-sync pytest -q -s test/qualification/measure.py -k 'not native'
```

Record first-run costs and build profiles. Debug-versus-release measurements
are not language-overhead comparisons. Batch throughput is not per-message
latency, and filesystem-cache timing is not sustained disk throughput.

Parent peak RSS is a lifetime high-water mark, not incremental memory or an
isolated-worker total. Measure the whole process tree with suitable host tools
and report unavailable measurements and sampling blind spots.

### Failed disposal on simulated CoreMIDI endpoints

Run the capacity regression alone on macOS. It uses the linked CoreMIDI fixture
and deliberately retains three failed connections until the test process exits.

```sh
cargo test --locked -p middy --test coremidi_boundary repeated_failed_disposals_each_retain_one_endpoint_and_client_slot -- --exact --ignored
```

## Separately authorized native checks

Fixtures must select unique test-owned endpoints, never a pre-existing instrument.
Use the supervisor rather than launching a native child directly when backend
liveness is unknown. Software endpoints do not authorize physical traffic,
instrument tuning/reset writes, service restarts or fault injection.

- MIDI 1 loopback does not qualify UMP; ALSA sequencer does not qualify ALSA raw.
- WinMM needs an independently arranged loopback route; do not install a driver
  implicitly.
- Typed capture/replay, metrics, SysEx and synthetic MTS queries are distinct
  workloads and do not establish physical instrument support.
- Reopen cases use fresh high-level resources for every cycle; closure is
  terminal. Include process-lifetime CoreMIDI keeper/notification resources in
  accounting.

If CoreMIDI creation fails in the agent context, compare the maintained direct
SDK access probe in an ordinary Terminal before changing code or restarting
services. An access-context failure is not evidence of a library defect.

## Application acceptance

Agree stream counts, rates, maximum payloads, capture duration/storage and
resource/latency budgets after measuring the intended host. Rerun normal and
overload workloads. Inspect loss and completion reports; structural file
completion does not imply loss-free capture. Queue size alone cannot establish
sustained throughput or latency.

Physical tests require named equipment, authorization and a recovery procedure
for unplug/replug, reboot, driver faults and uncertain sends. Keep codec, file,
state-machine, software-loopback and physical claims separate. This document
authorizes no runtime test, installation, publication or hardware operation.


## Release acceptance and withdrawal

For each candidate, build a wheel from the source archive, run the installed
wheel checks outside the checkout, and run `pytest test/release/artifacts.py`
against the files in `dist/`. Record the commit, filenames and results.
The platform matrix runs on pull requests to `main`, on manual dispatch, and
when a GitHub release is published. Use manual dispatch for the candidate
commit, with `check_index` enabled to test the published dependency contract.
Candidate CI tests the wheel in a separate environment with the locked runtime
dependencies and also builds a source distribution. For source distribution
qualification, install the wheel built from the unpacked source archive.

Use the uFor commit pinned in `uv.lock` for checkout installs and candidate
CI. Publish middy wheels once a compatible uFor wheel (0.2.0 or newer) is
available from PyPI and the candidate passes `check_index` on every matrix
target. The check installs middy and all dependencies from wheels.

If a released platform fails, pause distribution, identify affected filenames
and environments in the release notes, and mark the release as affected. If
those files reached a package index, yank the affected version there. Publish
a corrected version after verifying the failing case and the replacement
artifacts.

When refreshing libremidi, reconcile each patch in the source manifest with the
new pin, update the notices, and run its listed backend regressions on each
available platform. Retain outstanding platform checks in the matrix above.
