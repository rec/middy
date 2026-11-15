//! Shared selected-output ownership, per-device cleanup and broadcast progress.
use super::{port_matches, OutputState};
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
    destinations: Vec<Destination>,
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
        let mut group = Self {
            encoding: client.encoding,
            client: Some(client),
            patterns,
            subprocess,
            destinations: Vec::new(),
            failure: None,
        };
        group.refresh()?;
        Ok(group)
    }
    pub fn failure(&self) -> Option<Arc<midi::Error>> {
        self.failure.clone()
    }
    fn refresh(&mut self) -> midi::Result<()> {
        let client = self.client.as_ref().ok_or(midi::Error::Closed)?;
        let ports = client.ports().map_err(midi::Error::Connect)?;
        let visible: Vec<_> = transport::port_keys(&ports)
            .into_iter()
            .filter(|p| self.patterns.iter().any(|s| port_matches(s, &p.name)))
            .collect();
        let mut i = 0;
        while i < self.destinations.len() {
            if visible.contains(&self.destinations[i].key) {
                i += 1;
            } else {
                let removed = self.destinations.remove(i);
                if let Err(error) = removed.close() {
                    super::report(&error);
                }
            }
        }
        let total_ports = visible.len();
        for key in visible {
            if self.destinations.iter().any(|p| p.key == key) {
                continue;
            }
            let port = ports
                .iter()
                .find(|p| p.name() == key.name && p.backend_id() == key.id)
                .expect("visible output");
            let info = PortInfo::new(key.name.clone(), port.backend(), key.id)?;
            let options = transport::OutputOptions {
                backend: Some(port.backend()),
                encoding: port.backend().encoding(),
            };
            let opened = (|| {
                let child = if let Some(options_worker) = &self.subprocess {
                    transport::Output::new_subprocess(options, options_worker.clone())
                } else {
                    transport::Output::new(options)
                }
                .map_err(midi::Error::Init)?;
                let snapshots = child.ports().map_err(midi::Error::Connect)?;
                let Some(selected) = snapshots
                    .iter()
                    .filter(|p| p.name() == key.name && p.backend_id() == key.id)
                    .nth(key.occurrence)
                else {
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
                    return Err(midi::Error::BroadcastSend(Box::new(
                        midi::BroadcastSendError {
                            completed_ports: Vec::new(),
                            failed_port: info,
                            total_ports,
                            cause: Box::new(error),
                        },
                    )))
                }
            }
        }
        Ok(())
    }
    fn broadcast(
        &mut self,
        mut send: impl FnMut(&mut Destination) -> midi::Result<()>,
    ) -> midi::Result<()> {
        if let Some(error) = &self.failure {
            return Err(midi::Error::Shared(Arc::clone(error)));
        }
        let result = (|| {
            self.refresh()?;
            if self.destinations.is_empty() {
                return Err(midi::Error::Connect(transport::Error::Invalid(
                    "no matching MIDI outputs".into(),
                )));
            }
            for i in 0..self.destinations.len() {
                if let Err(cause) = send(&mut self.destinations[i]) {
                    return Err(midi::Error::BroadcastSend(Box::new(
                        midi::BroadcastSendError {
                            completed_ports: self.destinations[..i]
                                .iter()
                                .map(|p| p.port.clone())
                                .collect(),
                            failed_port: self.destinations[i].port.clone(),
                            total_ports: self.destinations.len(),
                            cause: Box::new(cause),
                        },
                    )));
                }
            }
            Ok(())
        })();
        result.map_err(|error| {
            let error = Arc::new(error);
            self.failure = Some(Arc::clone(&error));
            midi::Error::Shared(error)
        })
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
