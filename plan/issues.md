# Deferred hardware checks

- Verify discovery and teardown on Windows WinMM, Linux ALSA sequencer and raw MIDI (including available UMP variants), and additional macOS CoreMIDI configurations.
- Verify physical MIDI 1 and UMP communication on supported routes. Check recovery after unplugging and reconnecting a device, rebooting the host, or losing the MIDI service.
- Exercise teardown while callbacks are active on real drivers. Measure thread and handle counts after repeated teardown failures and check behavior under resource exhaustion.

For the platform matrix and test procedures, see the [qualification plan](qualification.md#open-gates).
