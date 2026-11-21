# Open issues and risks

Reviewed at `50049c0` on 2026-11-21. Resolution evidence below includes device-free tests, simulated driver faults and local distribution checks. Real-driver and physical qualification remains in the [platform matrix](qualification.md#open-gates).

## Discovery and shutdown

1. **Resolved: unrelated catalogs poll independently.** The [scanner](../crates/middy/src/transport/scanner.rs) runs one worker per active backend, direction and subprocess configuration. The [regression](../crates/middy/tests/subprocess.rs) holds an input enumeration open while an interval change, output discovery and send finish. Clients sharing the blocked catalog still depend on its driver returning.

2. **Resolved: disconnect cleanup preserves diagnostics.** The [input follower](../crates/middy/src/transport/all.rs) retires every removed source, counts its drops and returns the first disposal failure. The [subprocess regression](../crates/middy/tests/subprocess.rs) removes two sources together and checks both disconnects, the second source’s unfinished SysEx diagnostic, seven reported drops and the first source’s error.

3. **High consequence, backend-dependent: native callback retirement is a memory-safety boundary.** [Native teardown and borrow_receiver](../crates/middy/src/transport/native.rs) rely on native unregistration keeping the original strong reference alive until a callback can acquire its own. A late callback after a falsely successful teardown would access freed state. A reported teardown failure takes the opposite path and can retain the owner thread and its resources. The checked paths, backend exceptions and process-lifetime CoreMIDI keeper are documented in [vendor provenance](../crates/middy/third_party/libremidi/SOURCE.md). No use-after-free was established here. Exercise callback entry concurrently with successful and failed close on each backend, and count surviving threads/handles after repeated failures. Qualify embedded-interpreter finalization and reinitialization separately.

4. **Resolved: an explicit reopen retries discovery.** Failed subscriptions retain the error, and a new subscription creates a fresh catalog. Thread creation failures leave no registry entry. The [subprocess test](../crates/middy/tests/subprocess.rs) reopens while the failed subscribers still exist.

5. **Resolved: followers reject indistinguishable selected endpoints.** Both APIs check the selected catalog and the fresh client enumeration for duplicate (name, backend ID) pairs before connecting. On hot-plug, the [broadcast regression](../crates/middy/tests/subprocess.rs) reorders the list and introduces a duplicate, then checks that the send fails without delivering another packet. Single discovery snapshots retain their native handles.

## Resource costs and API behavior

6. **Resolved: unchanged catalogs skip topology reconciliation.** Each [scanner](../crates/middy/src/transport/scanner.rs) builds keys in one pass and signals topology changes. Inputs retain a separate 100 ms health check. Followers reuse a reconciled snapshot and use hash sets for added/removed membership. A missing endpoint during open remains eligible for retry. The [subprocess test](../crates/middy/tests/subprocess.rs) exercises 257 visible names, six followers and 300 sends, checking that only the two direction-specific catalogs repeat enumeration. A new connection still enumerates its own client to obtain a valid native handle.

7. **Resolved by contract: each input delivers callbacks serially and close drains them.** The [two-source regression](../crates/middy/tests/subprocess.rs) holds the first handler open, starts close, verifies the second delivery waits, then releases the handler and checks that both sources deliver. The [guide](../README.ai.md) describes the effect of blocking. The [memory workloads](../test/qualification/measure.py) exercised 32,768 recorded events and 4,096 cached parameters. On this arm64 macOS host the combined pytest process peaked at 72,286,208 bytes. This is a process high-water mark. Application load and resource budgets remain platform qualification items.

8. **Resolved: the selector contract describes literal-name collisions.** The [guide](../README.ai.md) shows how both input and output selectors can follow extra devices and how to connect a single snapshot in Python and Rust. The [port-selection regression](../crates/middy/tests/port_selection.rs) checks the ambiguity between `Keys[1]` and `Keys1`.

9. **Resolved: recording and teardown ownership are documented beside the state.** [\_InputState](../middy/_clients.py) owns the transport, callback bridge and recorder. Input owns the public closed flag. See the [Python close-error regression](../test/test_live.py) and [Rust native boundary regressions](../crates/middy/tests/coremidi_boundary.rs).

## Release, interoperability and stewardship

10. **Resolved: live support and release recovery are documented.** The [README](../README.md) names libremidi and distinguishes packet/file support from live backend capabilities. The [release procedure](qualification.md#release-acceptance-and-withdrawal) covers installed artifacts and withdrawal if the publication-triggered matrix finds a failure.

11. **Deferred: no Linux or Windows host or physical route is available today.** Track Windows WinMM discovery and teardown, Linux ALSA sequencer/raw/UMP, additional macOS configurations, physical MIDI 1/UMP, unplug/replug, reboot, service failure and resource exhaustion against the [qualification matrix](qualification.md) and [Linux procedure](linux-qualification.md). Use authorized equipment and distinguish simulated results from driver and physical-device results. MIDI-CI Profiles, Property Exchange, Process Inquiry, MIDI Clip profile negotiation and additional manufacturer dumps remain separate interoperability work. An opaque dump ending in silence can still be incomplete; preserve the distinction between an unverified capture and a verified backup in user-facing workflows. See [SysEx coverage](../doc/sysex-dumps.md).

12. **Release control implemented.** [Distribution checks](../test/release/artifacts.py) compare the wheel’s notices, licenses and vendor provenance to the checkout and check the source archive inventory. The [refresh procedure](qualification.md#release-acceptance-and-withdrawal) preserves the patch review and platform checks. Tom should send the fork patches and their regression evidence upstream at project completion.
