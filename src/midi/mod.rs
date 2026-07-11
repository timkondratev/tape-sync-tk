use std::fmt;

use midir::MidiOutputConnection;

use crate::runtime::RuntimeError;

#[cfg(unix)]
use midir::os::unix::VirtualOutput;

pub struct MidiOutputPort<C = MidiOutputConnection> {
    pub port_name: String,
    connection: C,
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

impl MidiOutputPort<MidiOutputConnection> {
    pub fn send(&mut self, message: &[u8]) -> Result<(), RuntimeError> {
        self.connection
            .send(message)
            .map_err(|source| RuntimeError::Midi(source.to_string()))
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
    struct FakeConnection;

    #[test]
    fn preserves_port_name() {
        let port = MidiOutputPort::new("TapeSync MIDI Out".to_string(), FakeConnection);
        assert_eq!(port.port_name, "TapeSync MIDI Out");
    }

    #[test]
    fn exposes_inner_connection() {
        let port = MidiOutputPort::new("TapeSync MIDI Out".to_string(), FakeConnection);
        assert_eq!(port.into_inner(), FakeConnection);
    }
}
