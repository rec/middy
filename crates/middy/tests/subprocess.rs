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
    std::fs::write(&path, format!("{names}Unselected new device\nFuture1\n")).unwrap();
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
        std::fs::write(&path, "Pad\nKeyboard A\nKeyboard B\n").unwrap();
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
    std::fs::write(&path, "Keyboard\nKeyboard\n").unwrap();
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
    std::fs::write(&path, "").unwrap();
    for _ in 0..2 {
        assert!(matches!(
            input.receive(Some(Duration::from_secs(3)), None).unwrap(),
            Some(InputItem::Diagnostic(_))
        ));
    }
    std::fs::write(&path, "Keyboard\n").unwrap();
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
