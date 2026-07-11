use std::fmt;

use midir::MidiOutputConnection;

use crate::runtime::RuntimeError;

#[cfg(unix)]
use midir::os::unix::VirtualOutput;

pub struct MidiOutputPort<C = MidiOutputConnection> {
    pub port_name: String,
    connection: C,
}

pub trait MidiSink {
    fn send(&mut self, message: &[u8]) -> Result<(), String>;
}

impl MidiSink for MidiOutputConnection {
    fn send(&mut self, message: &[u8]) -> Result<(), String> {
        MidiOutputConnection::send(self, message).map_err(|source| source.to_string())
    }
}

impl<C> MidiOutputPort<C> {
    pub fn new(port_name: String, connection: C) -> Self {
        Self {
            port_name,
            connection,
        }
    }

    pub fn into_inner(self) -> C {
        self.connection
    }
}

impl<C> fmt::Debug for MidiOutputPort<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MidiOutputPort")
            .field("port_name", &self.port_name)
            .finish_non_exhaustive()
    }
}

impl<C: MidiSink> MidiOutputPort<C> {
    pub fn send(&mut self, message: &[u8]) -> Result<(), RuntimeError> {
        self.connection
            .send(message)
            .map_err(RuntimeError::Midi)
    }

    pub fn send_start(&mut self) -> Result<(), RuntimeError> {
        self.send(&[0xFA])
    }

    pub fn send_stop(&mut self) -> Result<(), RuntimeError> {
        self.send(&[0xFC])
    }

    pub fn send_continue(&mut self) -> Result<(), RuntimeError> {
        self.send(&[0xFB])
    }

    pub fn send_clock(&mut self) -> Result<(), RuntimeError> {
        self.send(&[0xF8])
    }

    pub fn send_song_position_pointer(&mut self, position: u16) -> Result<(), RuntimeError> {
        if position > 0x3FFF {
            return Err(RuntimeError::Midi(format!(
                "song position pointer {position} exceeds the 14-bit MIDI limit"
            )));
        }

        self.send(&[0xF2, (position & 0x7F) as u8, ((position >> 7) & 0x7F) as u8])
    }
}

#[cfg(unix)]
pub fn create_virtual_output(port_name: &str) -> Result<MidiOutputPort<MidiOutputConnection>, RuntimeError> {
    let midi_output = midir::MidiOutput::new("TapeSync runtime init")
        .map_err(|source| RuntimeError::Midi(source.to_string()))?;
    let connection = midi_output
        .create_virtual(port_name)
        .map_err(|source| RuntimeError::Midi(source.to_string()))?;

    Ok(MidiOutputPort::new(port_name.to_string(), connection))
}

#[cfg(not(unix))]
pub fn create_virtual_output(_port_name: &str) -> Result<MidiOutputPort<MidiOutputConnection>, RuntimeError> {
    Err(RuntimeError::Midi(
        "virtual MIDI output is unsupported on this platform".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct FakeConnection {
        messages: Vec<Vec<u8>>,
    }

    impl MidiSink for FakeConnection {
        fn send(&mut self, message: &[u8]) -> Result<(), String> {
            self.messages.push(message.to_vec());
            Ok(())
        }
    }

    #[test]
    fn preserves_port_name() {
        let port = MidiOutputPort::new(
            "TapeSync MIDI Out".to_string(),
            FakeConnection { messages: Vec::new() },
        );
        assert_eq!(port.port_name, "TapeSync MIDI Out");
    }

    #[test]
    fn exposes_inner_connection() {
        let port = MidiOutputPort::new(
            "TapeSync MIDI Out".to_string(),
            FakeConnection { messages: Vec::new() },
        );
        assert_eq!(port.into_inner(), FakeConnection { messages: Vec::new() });
    }

    #[test]
    fn encodes_transport_and_clock_bytes() {
        let mut port = MidiOutputPort::new(
            "TapeSync MIDI Out".to_string(),
            FakeConnection { messages: Vec::new() },
        );

        port.send_start().expect("start should send");
        port.send_continue().expect("continue should send");
        port.send_stop().expect("stop should send");
        port.send_clock().expect("clock should send");

        let connection = port.into_inner();
        assert_eq!(connection.messages, vec![vec![0xFA], vec![0xFB], vec![0xFC], vec![0xF8]]);
    }

    #[test]
    fn encodes_song_position_pointer_bytes() {
        let mut port = MidiOutputPort::new(
            "TapeSync MIDI Out".to_string(),
            FakeConnection { messages: Vec::new() },
        );

        port.send_song_position_pointer(0x1234)
            .expect("spp should send");

        let connection = port.into_inner();
        assert_eq!(connection.messages, vec![vec![0xF2, 0x34, 0x24]]);
    }
}
