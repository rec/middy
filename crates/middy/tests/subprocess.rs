//! No MIDI endpoints: the real subprocess client talks to a controlled native peer.
use middy::{midi, transport, Input, NoteOn, Output, SubprocessOptions};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
static SERIAL: Mutex<()> = Mutex::new(());
fn fixture() -> PathBuf {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| {
            let project = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let directory = project.join("../../target/subprocess-tests");
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join(format!(
                "worker-{}{}",
                std::process::id(),
                std::env::consts::EXE_SUFFIX
            ));
            assert!(std::process::Command::new("rustc")
                .args(["--edition=2021", "-O"])
                .arg(project.join("tests/support/subprocess_worker.rs"))
                .arg("-o")
                .arg(&path)
                .status()
                .unwrap()
                .success());
            path
        })
        .clone()
}
fn options(mode: &str) -> SubprocessOptions {
    SubprocessOptions {
        program: fixture(),
        args: vec![mode.into()],
        timeout: Duration::from_secs(1),
    }
}

fn output_fixture(names: &str) -> (PathBuf, PathBuf, SubprocessOptions) {
    let topology = fixture().with_extension("outputs");
    let log = fixture().with_extension("output-log");
    std::fs::write(&topology, names).unwrap();
    std::fs::write(&log, "").unwrap();
    let scans = log.with_extension("scans");
    std::fs::write(&scans, "").unwrap();
    let mut worker = options("broadcast");
    worker.args.extend([
        topology.clone().into_os_string(),
        log.clone().into_os_string(),
        "0".into(),
        scans.into_os_string(),
    ]);
    (topology, log, worker)
}
fn write_ports(topology: &std::path::Path, names: &str) {
    let pending = topology.with_extension("pending");
    std::fs::write(&pending, names).unwrap();
    std::fs::rename(pending, topology).unwrap();
}
fn change_outputs(topology: &std::path::Path, log: &std::path::Path, names: &str) {
    write_ports(topology, names);
    let expected = names.replace('\n', "|");
    let deadline = Instant::now() + Duration::from_secs(3);
    while std::fs::read_to_string(log.with_extension("scans"))
        .unwrap()
        .lines()
        .filter(|s| s.split('\t').nth(2) == Some(expected.as_str()))
        .count()
        < 2
    {
        assert!(
            Instant::now() < deadline,
            "shared scanner did not observe topology"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn sent_to(log: &std::path::Path, name: &str) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .filter_map(|s| {
            let fields: Vec<_> = s.split('\t').collect();
            (fields[0] == name && fields[2] != "close").then(|| fields[2].to_owned())
        })
        .collect()
}
#[test]
fn inputs_and_outputs_share_polling_and_sending_does_not_trigger_scans() {
    use std::collections::BTreeMap;
    let _serial = SERIAL.lock().unwrap();
    let (_, log, worker) = output_fixture("Synth\n");
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    for _ in 0..3 {
        let mut input = Input::new()
            .with_run_in_subprocess(true)
            .unwrap()
            .with_subprocess_options(worker.clone())
            .unwrap()
            .with_port(std::iter::empty::<&str>())
            .unwrap();
        assert!(input.receive(Some(Duration::ZERO), None).unwrap().is_none());
        inputs.push(input);
        let mut output = Output::new()
            .with_run_in_subprocess(true)
            .unwrap()
            .with_subprocess_options(worker.clone())
            .unwrap()
            .with_port("Synth")
            .unwrap();
        output.send(middy::TimingClock::new().unwrap()).unwrap();
        outputs.push(output);
    }
    let scans = log.with_extension("scans");
    let before = std::fs::read_to_string(&scans).unwrap().lines().count();
    let start = Instant::now();
    for _ in 0..100 {
        for output in &mut outputs {
            output.send(middy::TimingClock::new().unwrap()).unwrap();
        }
    }
    std::thread::sleep(Duration::from_millis(350));
    let records = std::fs::read_to_string(&scans).unwrap();
    let budget = 2 * (start.elapsed().as_millis() / 100 + 2) as usize;
    assert!(records.lines().count() - before <= budget);
    let mut calls = BTreeMap::new();
    for record in records.lines() {
        let mut fields = record.split('\t');
        *calls
            .entry((fields.next().unwrap(), fields.next().unwrap()))
            .or_insert(0) += 1;
    }
    assert_eq!(
        calls
            .iter()
            .filter(|((_, input), _)| *input == "true")
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|((_, input), _)| *input == "false")
            .count(),
        4
    );
    assert_eq!(calls.values().filter(|n| **n > 1).count(), 2);
    inputs.pop().unwrap().close().unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert!(std::fs::read_to_string(&scans).unwrap().lines().count() > records.lines().count());
    for input in &mut inputs {
        input.close().unwrap();
    }
    for output in &mut outputs {
        output.close().unwrap();
    }
    drop(inputs);
    drop(outputs);
    std::thread::sleep(Duration::from_millis(150));
    let stopped = std::fs::read_to_string(&scans).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(std::fs::read_to_string(&scans).unwrap(), stopped);
}
#[test]
fn selected_outputs_follow_new_matches_and_keep_independent_cleanup_state() {
    let _serial = SERIAL.lock().unwrap();
    for collection in [false, true] {
        let (topology, log, worker) = output_fixture("Synth A\nSynth B\nOther\n");
        let output = Output::new()
            .with_run_in_subprocess(true)
            .unwrap()
            .with_subprocess_options(worker)
            .unwrap();
        let mut output = if collection {
            output.with_port(["Synth *", "* A"])
        } else {
            output.with_port("Synth *")
        }
        .unwrap();
        assert_eq!(
            output.send(NoteOn::new(60).unwrap()).unwrap().sent_events,
            1
        );
        change_outputs(&topology, &log, "Synth A\nSynth B\nSynth C\nOther\n");
        output.send(NoteOn::new(61).unwrap()).unwrap();
        change_outputs(&topology, &log, "Synth A\nSynth C\nOther\n");
        output.send(NoteOn::new(62).unwrap()).unwrap();
        output.close().unwrap();
        assert_eq!(
            sent_to(&log, "Synth A"),
            ["b:903c5a", "b:903d5a", "b:903e5a", "b:803c00", "b:803d00", "b:803e00"]
        );
        assert_eq!(
            sent_to(&log, "Synth B"),
            ["b:903c5a", "b:903d5a", "b:803c00", "b:803d00"]
        );
        assert_eq!(
            sent_to(&log, "Synth C"),
            ["b:903d5a", "b:903e5a", "b:803d00", "b:803e00"]
        );
        assert!(sent_to(&log, "Other").is_empty());
    }
}
#[test]
fn scan_interval_changes_existing_and_future_inputs_and_outputs() {
    let _serial = SERIAL.lock().unwrap();
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            middy::set_scan_interval(Duration::from_millis(100)).unwrap();
        }
    }
    let _reset = Reset;
    middy::set_scan_interval(Duration::from_secs(30)).unwrap();
    let (_, log, worker) = output_fixture("Synth\n");
    let mut input = Input::new()
        .with_run_in_subprocess(true)
        .unwrap()
        .with_subprocess_options(worker.clone())
        .unwrap()
        .with_port(std::iter::empty::<&str>())
        .unwrap();
    assert!(input.receive(Some(Duration::ZERO), None).unwrap().is_none());
    let mut output = Output::new()
        .with_run_in_subprocess(true)
        .unwrap()
        .with_subprocess_options(worker)
        .unwrap()
        .with_port("Synth")
        .unwrap();
    output.send(middy::TimingClock::new().unwrap()).unwrap();
    let scans = log.with_extension("scans");
    let before = std::fs::read_to_string(&scans).unwrap();
    for invalid in [Duration::ZERO, Duration::MAX] {
        assert!(matches!(
            middy::set_scan_interval(invalid),
            Err(transport::Error::Invalid(_))
        ));
    }
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(std::fs::read_to_string(&scans).unwrap(), before);
    middy::set_scan_interval(Duration::from_millis(20)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let after = std::fs::read_to_string(&scans).unwrap();
        if after.lines().count() >= before.lines().count() + 6 {
            for direction in ["true", "false"] {
                let calls = after
                    .lines()
                    .filter(|s| s.split('\t').nth(1) == Some(direction))
                    .count();
                assert!(calls >= 4, "both input and output polling must change");
            }
            break;
        }
        assert!(
            Instant::now() < deadline,
            "interval update did not wake the scanner"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    input.close().unwrap();
    output.close().unwrap();
}
#[test]
fn blocked_input_scan_allows_interval_changes_and_output_discovery() {
    let _serial = SERIAL.lock().unwrap();
    let (topology, log, mut worker) = output_fixture("Synth\n");
    worker.timeout = Duration::from_secs(4);
    let mut input = Input::new()
        .with_run_in_subprocess(true)
        .unwrap()
        .with_subprocess_options(worker.clone())
        .unwrap()
        .with_port(std::iter::empty::<&str>())
        .unwrap();
    input.receive(Some(Duration::ZERO), None).unwrap();
    write_ports(&topology, "!wait!\n");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !std::fs::read_to_string(log.with_extension("scans"))
        .unwrap()
        .contains("!wait!")
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let start = Instant::now();
    middy::set_scan_interval(Duration::from_millis(100)).unwrap();
    let mut output = Output::new()
        .with_run_in_subprocess(true)
        .unwrap()
        .with_subprocess_options(worker)
        .unwrap()
        .with_port("!wait!")
        .unwrap();
    output.send(middy::TimingClock::new().unwrap()).unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    write_ports(&topology, "Synth\n");
    input.close().unwrap();
    output.close().unwrap();
}

#[test]
fn shared_scan_failure_reaches_each_input_subscriber() {
    let _serial = SERIAL.lock().unwrap();
    let (topology, _, worker) = output_fixture("Synth\n");
    let mut inputs = Vec::new();
    for _ in 0..2 {
        let mut input = Input::new()
            .with_run_in_subprocess(true)
            .unwrap()
            .with_subprocess_options(worker.clone())
            .unwrap()
            .with_port(std::iter::empty::<&str>())
            .unwrap();
        assert!(input.receive(Some(Duration::ZERO), None).unwrap().is_none());
        inputs.push(input);
    }
    write_ports(&topology, "!fail!\n");
    for input in &mut inputs {
        let error = input
            .receive(Some(Duration::from_secs(3)), None)
            .unwrap_err();
        assert!(error.to_string().contains("device enumeration failed"));
    }
    write_ports(&topology, "Synth\n");
    let mut reopened = Input::new()
        .with_run_in_subprocess(true)
        .unwrap()
        .with_subprocess_options(worker)
        .unwrap()
        .with_port(std::iter::empty::<&str>())
        .unwrap();
    assert!(reopened
        .receive(Some(Duration::ZERO), None)
        .unwrap()
        .is_none());
    reopened.close().unwrap();
    for input in &mut inputs {
        let _ = input.close();
    }
}
#[test]
fn broadcast_failure_reports_destinations_and_partial_command_progress() {
    use middy::events::PlaybackCause;
    let _serial = SERIAL.lock().unwrap();
    for compound in [false, true] {
        let (_, log, mut worker) = output_fixture("Good\nFail\nLater\n");
        worker.args[3] = if compound { "1" } else { "0" }.into();
        let mut output = Output::new()
            .with_run_in_subprocess(true)
            .unwrap()
            .with_subprocess_options(worker)
            .unwrap()
            .with_port("*")
            .unwrap();
        let failure = if compound {
            output
                .send(middy::ControlChange::new_with_resolution(1, 10000, 14).unwrap())
                .unwrap_err()
        } else {
            output.send(NoteOn::new(60).unwrap()).unwrap_err()
        };
        assert_eq!(failure.report.sent_events, 0);
        let PlaybackCause::Send(midi::Error::Shared(error)) = failure.cause else {
            panic!("broadcast error expected")
        };
        let midi::Error::BroadcastSend(error) = error.as_ref() else {
            panic!("destination progress expected")
        };
        assert_eq!(
            error
                .completed_ports
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["Good"]
        );
        assert_eq!(error.failed_port.name, "Fail");
        assert_eq!(error.total_ports, 3);
        if compound {
            assert!(matches!(
                &*error.cause,
                midi::Error::Send(transport::Error::CompoundSend {
                    completed_messages: Some(1),
                    ..
                })
            ));
        }
        assert!(output.close().is_err());
        assert!(sent_to(&log, "Later").is_empty());
        assert!(sent_to(&log, "Good")
            .contains(&if compound { "b:b00100" } else { "b:803c00" }.to_owned()));
    }
}
#[test]
fn default_output_stays_single_and_selected_ump_outputs_do_not_generate_midi1_cleanup() {
    let _serial = SERIAL.lock().unwrap();
    let (_, log, worker) = output_fixture("Synth\nSynth\nOther\n");
    let mut output = Output::new()
        .with_run_in_subprocess(true)
        .unwrap()
        .with_subprocess_options(worker)
        .unwrap();
    output.send(middy::TimingClock::new().unwrap()).unwrap();
    output.close().unwrap();
    assert_eq!(sent_to(&log, "Synth"), ["b:f8"]);
    let (_, log, worker) = output_fixture("Synth\nSynth\nOther\n");
    let mut output = Output::new()
        .with_transport(transport::OutputOptions {
            encoding: transport::Encoding::Ump,
            backend: None,
        })
        .unwrap()
        .with_run_in_subprocess(true)
        .unwrap()
        .with_subprocess_options(worker)
        .unwrap()
        .with_port(["Synth", "Synth*"])
        .unwrap();
    output
        .send(transport::Packet::Ump(vec![0x20903c64]))
        .unwrap();
    output.close().unwrap();
    assert_eq!(sent_to(&log, "Synth"), ["u:20903c64", "u:20903c64"]);
    assert!(sent_to(&log, "Other").is_empty());
}
#[test]
fn selected_inputs_follow_the_union_of_patterns_without_duplicate_connections() {
    use middy::{events::InputItem, messages::Message};
    let _serial = SERIAL.lock().unwrap();
    let path = fixture().with_extension("selected-ports");
    let names = "Ignored\nKeyboard A\nKeyboard B\nPad1\nPad2\nkeyboard C\n";
    std::fs::write(&path, names).unwrap();
    let mut worker = options("hotplug");
    worker.args.push(path.clone().into_os_string());
    let mut input = Input::new()
        .with_run_in_subprocess(true)
        .unwrap()
        .with_subprocess_options(worker)
        .unwrap()
        .with_port(["Keyboard *", "* A", "Pad[!2]", "Future?"])
        .unwrap();
    let mut selected = std::collections::BTreeSet::new();
    for _ in 0..3 {
        let Some(InputItem::Message(message)) =
            input.receive(Some(Duration::from_secs(3)), None).unwrap()
        else {
            panic!("message expected")
        };
        selected.insert(message.port().unwrap().name.clone());
    }
    assert_eq!(
        selected,
        [
            "Keyboard A".to_owned(),
            "Keyboard B".to_owned(),
            "Pad1".to_owned()
        ]
        .into()
    );
    assert!(input
        .receive(Some(Duration::from_millis(200)), None)
        .unwrap()
        .is_none());
    write_ports(&path, &format!("{names}Unselected new device\nFuture1\n"));
    let Some(InputItem::Message(message)) =
        input.receive(Some(Duration::from_secs(3)), None).unwrap()
    else {
        panic!("new matching input expected")
    };
    assert_eq!(message.port().unwrap().name, "Future1");
    assert!(input
        .receive(Some(Duration::from_millis(200)), None)
        .unwrap()
        .is_none());
    input.close().unwrap();
}

#[test]
fn string_and_collection_selectors_follow_all_matches_even_when_initially_absent() {
    use middy::{events::InputItem, messages::Message};
    let _serial = SERIAL.lock().unwrap();
    let path = fixture().with_extension("one-pattern");
    let pattern = "Keyboard *".to_owned();
    let owned_patterns = vec![pattern.clone()];
    let borrowed_patterns = ["Keyboard *"];
    for form in 0..8 {
        std::fs::write(&path, "Pad\n").unwrap();
        let mut worker = options("hotplug");
        worker.args.push(path.clone().into_os_string());
        let input = Input::new()
            .with_run_in_subprocess(true)
            .unwrap()
            .with_subprocess_options(worker)
            .unwrap();
        let mut input = match form {
            0 => input.with_port("Keyboard *"),
            1 => input.with_port(["Keyboard *"]),
            2 => input.with_port(pattern.clone()),
            3 => input.with_port(&pattern),
            4 => input.with_port(owned_patterns.clone()),
            5 => input.with_port(&owned_patterns),
            6 => input.with_port(&borrowed_patterns[..]),
            _ => input.with_port(borrowed_patterns.iter()),
        }
        .unwrap();
        assert!(input
            .receive(Some(Duration::from_millis(200)), None)
            .unwrap()
            .is_none());
        write_ports(&path, "Pad\nKeyboard A\nKeyboard B\n");
        let mut names = std::collections::BTreeSet::new();
        for _ in 0..2 {
            let Some(InputItem::Message(message)) =
                input.receive(Some(Duration::from_secs(3)), None).unwrap()
            else {
                panic!("matching input expected")
            };
            names.insert(message.port().unwrap().name.clone());
        }
        assert_eq!(
            names,
            ["Keyboard A".to_owned(), "Keyboard B".to_owned()].into()
        );
        assert!(input
            .receive(Some(Duration::from_millis(200)), None)
            .unwrap()
            .is_none());
        input.close().unwrap();
    }
}

#[test]
fn an_empty_input_selection_never_falls_back_to_all_ports() {
    let _serial = SERIAL.lock().unwrap();
    let mut input = Input::new()
        .raw()
        .unwrap()
        .with_run_in_subprocess(true)
        .unwrap()
        .with_subprocess_options(options("early"))
        .unwrap()
        .with_port(std::iter::empty::<String>())
        .unwrap();
    assert!(input
        .receive(Some(Duration::from_millis(200)), None)
        .unwrap()
        .is_none());
    input.close().unwrap();
    assert!(Input::new().with_port(["Keyboard", ""]).is_err());
    assert!(Input::new().with_port("").is_err());
    assert!(Input::new().with_port(String::new()).is_err());
    assert!(Input::new().with_port(&["Keyboard", ""][..]).is_err());
}

#[test]
fn all_port_recording_marks_pending_fragments_incomplete_without_completed_messages() {
    let _serial = SERIAL.lock().unwrap();
    let mut input = Input::new()
        .with_run_in_subprocess(true)
        .unwrap()
        .with_subprocess_options(options("fragment"))
        .unwrap();
    let recorder = input
        .start(None::<&std::path::Path>, &Default::default())
        .unwrap();
    assert!(input.last_message_time().is_some());
    let report = input.stop(Duration::from_secs(1)).unwrap();
    assert!(!report.complete);
    assert!(report.failure.unwrap().contains("unfinished"));
    assert!(recorder.records().is_some());
    input.close().unwrap();
}

#[test]
fn simultaneous_removal_finishes_pending_source_after_first_close_fails() {
    use middy::events::{InputItem as Item, StreamDiagnosticKind as DiagnosticKind};
    use transport::all::AllItem;
    let _serial = SERIAL.lock().unwrap();
    let path = fixture().with_extension("retire");
    write_ports(&path, "First\nSecond\n");
    let mut worker = options("retire_fault");
    worker.args.push(path.clone().into_os_string());
    let client = transport::Input::new_subprocess(Default::default(), worker.clone()).unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let follower = client
        .connect_all(
            "inputs".into(),
            1024,
            false,
            Some(worker),
            None,
            move |item| send.send(item).unwrap(),
        )
        .unwrap();
    let next = || receive.recv_timeout(Duration::from_secs(3)).unwrap();
    for _ in 0..2 {
        assert!(matches!(next(), AllItem::Decoded(Item::Message(_))));
    }
    write_ports(&path, "");
    let diagnostics = follower.diagnostics();
    let mut disconnects = 0;
    let mut truncated = false;
    while disconnects < 2 {
        if let AllItem::Decoded(Item::Diagnostic(d)) = next() {
            disconnects += usize::from(d.kind == DiagnosticKind::ClockChange);
            truncated |= d.kind == DiagnosticKind::Truncated;
        }
    }
    assert!(truncated);
    assert!(follower
        .close()
        .err()
        .unwrap()
        .to_string()
        .contains("first disposal failed"));
    assert_eq!(diagnostics.dropped_packets(), 7);
}

#[test]
fn default_input_follows_hotplug_and_preserves_port_descriptions_in_recordings() {
    use middy::{archive, events::InputItem, messages::Message};
    let _serial = SERIAL.lock().unwrap();
    let path = fixture().with_extension("ports");
    std::fs::write(&path, "").unwrap();
    let mut worker = options("hotplug");
    worker.args.push(path.clone().into_os_string());
    let mut input = Input::new()
        .with_run_in_subprocess(true)
        .unwrap()
        .with_subprocess_options(worker)
        .unwrap();
    assert!(input.receive(Some(Duration::ZERO), None).unwrap().is_none());
    let recorder = input
        .start(None::<&std::path::Path>, &Default::default())
        .unwrap();
    write_ports(&path, "Keyboard\nKeyboard\n");
    let mut ports = Vec::new();
    for _ in 0..2 {
        let InputItem::Message(message) = input
            .receive(Some(Duration::from_secs(3)), None)
            .unwrap()
            .unwrap()
        else {
            panic!("message expected")
        };
        let port = message.port().unwrap().clone();
        assert_eq!(port.name, "Keyboard");
        ports.push(port);
    }
    assert_ne!(ports[0].identity, ports[1].identity);
    assert!(input.last_message_time().is_some());
    write_ports(&path, "");
    for _ in 0..2 {
        assert!(matches!(
            input.receive(Some(Duration::from_secs(3)), None).unwrap(),
            Some(InputItem::Diagnostic(_))
        ));
    }
    write_ports(&path, "Keyboard\n");
    let Some(InputItem::Message(message)) =
        input.receive(Some(Duration::from_secs(3)), None).unwrap()
    else {
        panic!("reconnected input expected")
    };
    assert!(!ports.contains(message.port().unwrap()));
    ports.push(message.port().unwrap().clone());
    input.close().unwrap();
    assert!(input.last_message_time().is_some());
    let records = recorder.records().unwrap();
    let mut bytes = Vec::new();
    let mut writer = archive::Writer::new(&mut bytes, 1_048_576).unwrap();
    for record in records {
        writer.write(&record).unwrap();
    }
    writer.finish(true, "").unwrap();
    drop(writer);
    let saved: Vec<_> = archive::Reader::new(std::io::Cursor::new(bytes), 1_048_576, false, true)
        .unwrap()
        .filter_map(|r| match r.unwrap() {
            archive::Record::Event(m) => m.port().cloned(),
            _ => None,
        })
        .collect();
    assert_eq!(saved, ports);
}
#[test]
fn high_level_input_and_output_use_the_shared_worker_without_python() {
    let _serial = SERIAL.lock().unwrap();
    let mut output = Output::new()
        .with_run_in_subprocess(true)
        .unwrap()
        .with_subprocess_options(options("healthy"))
        .unwrap();
    assert_eq!(output.send(Vec::<NoteOn>::new()).unwrap().sent_events, 0);
    let ports = output.ports().unwrap();
    assert_eq!(ports[0].name(), "output port");
    let mut output = output.connect(&ports[0]).unwrap();
    output.send(NoteOn::new(60).unwrap()).unwrap();
    output.close().unwrap();
    assert!(output.closed());
    let mut input = Input::new()
        .with_run_in_subprocess(true)
        .unwrap()
        .with_subprocess_options(options("early"))
        .unwrap();
    let ports = input.ports().unwrap();
    let mut input = input.connect(&ports[0]).unwrap();
    let message = input.receive(Some(Duration::from_secs(1)), None).unwrap();
    assert!(message.is_some());
    input.close().unwrap();
    assert!(input.closed());
}
#[test]
fn initialization_send_close_and_child_crash_have_bounded_failure() {
    let _serial = SERIAL.lock().unwrap();
    let start = Instant::now();
    let mut output = Output::new()
        .with_run_in_subprocess(true)
        .unwrap()
        .with_subprocess_options(options("hang_init"))
        .unwrap();
    assert!(matches!(
        output.ports(),
        Err(midi::Error::Init(transport::Error::Timeout(_)))
    ));
    assert!(start.elapsed() < Duration::from_secs(4));
    for mode in ["hang_send", "crash_send", "native_error", "hang_close"] {
        let client = transport::Output::new_subprocess(Default::default(), options(mode)).unwrap();
        let ports = client.ports().unwrap();
        let connection = client.connect(&ports[0]).unwrap();
        let start = Instant::now();
        if mode == "hang_close" {
            assert!(matches!(
                connection.close(),
                Err(transport::Error::Timeout(_))
            ));
        } else {
            let error = connection
                .send(&transport::Packet::Bytes(vec![0xf8]))
                .unwrap_err();
            assert!(matches!(
                error,
                transport::Error::Timeout(_)
                    | transport::Error::Worker(_)
                    | transport::Error::Native(_)
            ));
            assert!(connection.worker_failure().is_some());
        }
        assert!(start.elapsed() < Duration::from_secs(4));
    }
}
#[test]
fn failures_reap_workers_without_retaining_process_capacity() {
    let _serial = SERIAL.lock().unwrap();
    let mut failures = Vec::new();
    for _ in 0..40 {
        let client =
            transport::Output::new_subprocess(Default::default(), options("crash_send")).unwrap();
        let ports = client.ports().unwrap();
        let connection = client.connect(&ports[0]).unwrap();
        assert!(connection
            .send(&transport::Packet::Bytes(vec![0xf8]))
            .is_err());
        failures.push(connection);
    }
}
#[test]
fn worker_limit_and_spawn_failure_keep_distinct_resource_and_worker_categories() {
    let _serial = SERIAL.lock().unwrap();
    for _ in 0..35 {
        let mut missing = options("healthy");
        missing.program = fixture().with_extension("missing");
        assert!(matches!(
            transport::Output::new_subprocess(Default::default(), missing),
            Err(transport::Error::Worker(_))
        ));
    }
    let mut clients = Vec::new();
    for _ in 0..32 {
        clients.push(
            transport::Output::new_subprocess(Default::default(), options("healthy")).unwrap(),
        );
    }
    assert!(matches!(
        transport::Output::new_subprocess(Default::default(), options("healthy")),
        Err(transport::Error::Resource(_))
    ));
    drop(clients);
}

#[path = "../src/transport/subprocess/wire.rs"]
mod wire;
#[test]
fn standalone_worker_can_start_and_exit_without_creating_a_native_client() {
    use std::io::{Read, Write};
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_middy-worker"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(wire::MAGIC).unwrap();
    wire::write(
        &mut input,
        &wire::Value::Tuple(vec![
            wire::Value::String("shutdown".into()),
            wire::Value::List(vec![]),
        ]),
    )
    .unwrap();
    input.flush().unwrap();
    let mut output = child.stdout.take().unwrap();
    let mut magic = [0; 8];
    output.read_exact(&mut magic).unwrap();
    assert_eq!(&magic, wire::MAGIC);
    assert_eq!(
        wire::read(&mut output).unwrap(),
        wire::Value::Tuple(vec![
            wire::Value::String("shutdown".into()),
            wire::Value::Tuple(vec![wire::Value::Null, wire::Value::Integer(0)])
        ])
    );
    drop(input);
    assert!(child.wait().unwrap().success());
}

#[test]
fn bounded_forwarding_retains_the_prefix_and_combines_loss_during_silence() {
    let _serial = SERIAL.lock().unwrap();
    let (packets, received) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let client = transport::Input::new_subprocess(
        transport::InputOptions {
            receive_byte_limit: 2,
            ..Default::default()
        },
        options("pressure"),
    )
    .unwrap();
    let ports = client.ports().unwrap();
    let connection = client
        .connect(&ports[0], move |packet| {
            let first = packet.sequence == 0;
            packets.send(packet).unwrap();
            if first {
                wait.recv_timeout(Duration::from_secs(2)).unwrap();
            }
        })
        .unwrap();
    let first = received.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!((first.sequence, first.timestamp_ns), (0, Some(17)));
    assert_eq!(connection.dropped_packets(), 8);
    release.send(()).unwrap();
    let last = received.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!((last.sequence, last.timestamp_ns), (2, None));
    let diagnostics = connection.diagnostics();
    connection.close().unwrap();
    assert_eq!(diagnostics.dropped_packets(), 8);
}
