//! Shared selected-output ownership, per-device cleanup and broadcast progress.
use super::OutputState;
use crate::{
    events::PortInfo,
    messages::{AnyCommand, Command, PrimitiveMessage},
    midi, transport,
};
use std::sync::Arc;

struct Destination {
    key: transport::PortKey,
    port: PortInfo,
    connection: transport::OutputConnection,
    state: OutputState,
}
impl Destination {
    fn close(mut self) -> midi::Result<()> {
        let mut result = Ok(());
        for message in std::mem::take(&mut self.state).cleanup() {
            if let Err(error) = self
                .connection
                .send(&transport::Packet::Bytes(message.encode()?))
            {
                result = Err(midi::Error::Send(error));
                break;
            }
        }
        let closed = self
            .connection
            .close()
            .map(drop)
            .map_err(midi::Error::Transport);
        result.and(closed)
    }
}

/// Internal shared sender for the Rust API and Python binding.
#[doc(hidden)]
pub struct OutputGroup {
    encoding: transport::Encoding,
    client: Option<transport::Output>,
    patterns: Vec<String>,
    subprocess: Option<transport::subprocess::SubprocessOptions>,
    scanner: transport::scanner::Subscription,
    revision: Option<u64>,
    reconciled_ports: Option<Arc<Vec<transport::PortKey>>>,
    backend: transport::Backend,
    destinations: Vec<Destination>,
    unavailable: Option<(PortInfo, Arc<midi::Error>)>,
    selected_count: usize,
    failure: Option<Arc<midi::Error>>,
}
impl OutputGroup {
    pub fn new(
        client: transport::Output,
        patterns: Vec<String>,
        subprocess: Option<transport::subprocess::SubprocessOptions>,
    ) -> midi::Result<Self> {
        if patterns.iter().any(String::is_empty) {
            return Err(midi::Error::Connect(transport::Error::Invalid(
                "port pattern must not be empty".into(),
            )));
        }
        let backend = client.backend;
        let scanner = transport::scanner::Subscription::new(backend, false, subprocess.clone())
            .map_err(midi::Error::Connect)?;
        let mut group = Self {
            encoding: client.encoding,
            client: Some(client),
            patterns,
            subprocess,
            scanner,
            revision: None,
            reconciled_ports: None,
            backend,
            destinations: Vec::new(),
            unavailable: None,
            selected_count: 0,
            failure: None,
        };
        group.refresh()?;
        Ok(group)
    }
    pub fn failure(&self) -> Option<Arc<midi::Error>> {
        self.failure.clone()
    }
    fn refresh(&mut self) -> midi::Result<()> {
        let snapshot = self.scanner.snapshot().map_err(midi::Error::Connect)?;
        if self.revision == Some(snapshot.revision)
            || self
                .reconciled_ports
                .as_ref()
                .is_some_and(|ports| Arc::ptr_eq(ports, &snapshot.ports))
        {
            return Ok(());
        }
        let visible = snapshot
            .selected(Some(&self.patterns))
            .map_err(midi::Error::Connect)?;
        let visible_keys: std::collections::HashSet<_> = visible.iter().collect();
        for removed in self
            .destinations
            .extract_if(.., |dest| !visible_keys.contains(&dest.key))
        {
            if let Err(error) = removed.close() {
                super::report(&error);
            }
        }
        let total_ports = visible.len();
        self.unavailable = None;
        self.selected_count = total_ports;
        let connected: std::collections::HashSet<_> = self
            .destinations
            .iter()
            .map(|dest| dest.key.clone())
            .collect();
        for key in visible {
            if connected.contains(&key) {
                continue;
            }
            let info = PortInfo::new(key.name.clone(), self.backend, key.id)?;
            let options = transport::OutputOptions {
                backend: Some(self.backend),
                encoding: self.encoding,
            };
            let opened = (|| {
                let child = if let Some(options_worker) = &self.subprocess {
                    transport::Output::new_subprocess(options, options_worker.clone())
                } else {
                    transport::Output::new(options)
                }
                .map_err(midi::Error::Init)?;
                let snapshots = child.ports().map_err(midi::Error::Connect)?;
                let Some(selected) = key.resolve(&snapshots).map_err(midi::Error::Connect)? else {
                    return Ok(None);
                };
                let connection = child.connect(selected).map_err(midi::Error::Connect)?;
                Ok(Some(connection))
            })();
            match opened {
                Ok(Some(connection)) => self.destinations.push(Destination {
                    key,
                    port: info,
                    connection,
                    state: OutputState::default(),
                }),
                Ok(None) => (),
                Err(error) => {
                    self.unavailable.get_or_insert((info, Arc::new(error)));
                }
            }
        }
        self.reconciled_ports = (self.destinations.len() == total_ports).then_some(snapshot.ports);
        self.revision = Some(snapshot.revision);
        Ok(())
    }
    fn broadcast(
        &mut self,
        mut send: impl FnMut(&mut Destination) -> midi::Result<()>,
    ) -> midi::Result<()> {
        if let Some(error) = &self.failure {
            return Err(midi::Error::Shared(Arc::clone(error)));
        }
        if let Err(error) = self.refresh() {
            return Err(self.fail(error));
        }
        for i in 0..self.destinations.len() {
            if let Err(cause) = send(&mut self.destinations[i]) {
                let error = self.broadcast_error(i, &self.destinations[i].port, cause);
                return Err(self.fail(error));
            }
        }
        if let Some((port, error)) = &self.unavailable {
            return Err(self.broadcast_error(
                self.destinations.len(),
                port,
                midi::Error::Shared(Arc::clone(error)),
            ));
        }
        if self.destinations.is_empty() {
            return Err(midi::Error::Connect(transport::Error::Invalid(
                "no matching MIDI outputs".into(),
            )));
        }
        Ok(())
    }

    fn fail(&mut self, error: midi::Error) -> midi::Error {
        let error = Arc::new(error);
        self.failure = Some(Arc::clone(&error));
        midi::Error::Shared(error)
    }

    fn broadcast_error(
        &self,
        completed: usize,
        port: &PortInfo,
        cause: midi::Error,
    ) -> midi::Error {
        midi::Error::BroadcastSend(Box::new(midi::BroadcastSendError {
            completed_ports: self.destinations[..completed]
                .iter()
                .map(|p| p.port.clone())
                .collect(),
            failed_port: port.clone(),
            total_ports: self.selected_count,
            cause: Box::new(cause),
        }))
    }
    pub fn send(&mut self, packet: &transport::Packet) -> midi::Result<()> {
        transport::validate_packet(self.encoding, packet).map_err(midi::Error::Send)?;
        self.broadcast(|destination| {
            destination
                .connection
                .send(packet)
                .map_err(midi::Error::Send)?;
            if let transport::Packet::Bytes(bytes) = packet {
                destination.state.feed(bytes)?;
            }
            Ok(())
        })
    }
    pub fn send_command(&mut self, command: &AnyCommand) -> midi::Result<()> {
        let packets = if self.encoding == transport::Encoding::Bytes {
            command.expand()?
        } else {
            Vec::new()
        };
        self.broadcast(|destination| {
            let result = destination.connection.send_command(command);
            let completed = match &result {
                Ok(()) => packets.len(),
                Err(transport::Error::CompoundSend {
                    completed_messages, ..
                }) => completed_messages.unwrap_or(0),
                Err(_) => 0,
            };
            for message in packets.iter().take(completed) {
                destination.state.feed(&message.encode()?)?;
            }
            result.map_err(midi::Error::Send)
        })
    }
    fn close_destinations(&mut self) -> midi::Result<()> {
        let mut failure = None;
        for destination in self.destinations.drain(..) {
            if let Err(error) = destination.close() {
                if failure.is_none() {
                    failure = Some(error);
                } else {
                    super::report(&error);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
    pub fn close(mut self) -> midi::Result<transport::Output> {
        let result = self.close_destinations();
        let client = self.client.take().ok_or(midi::Error::Closed)?;
        result.map(|_| client)
    }
}
impl Drop for OutputGroup {
    fn drop(&mut self) {
        if let Err(error) = self.close_destinations() {
            super::report(&error);
        }
    }
}
