//! ICMP probing: the transport seam between the tracer and the network.

pub mod packet;
pub mod socket;

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// One answer to a probe, keyed by the probe's ICMP sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportEvent {
    /// The target answered: the probe's TTL reached it.
    EchoReply { seq: u16, from: Ipv4Addr },
    /// A router on the path dropped the probe when its TTL ran out.
    TimeExceeded { seq: u16, from: Ipv4Addr },
    /// A router reported the target as unreachable.
    Unreachable { seq: u16, from: Ipv4Addr },
}

impl TransportEvent {
    pub fn seq(&self) -> u16 {
        match *self {
            TransportEvent::EchoReply { seq, .. }
            | TransportEvent::TimeExceeded { seq, .. }
            | TransportEvent::Unreachable { seq, .. } => seq,
        }
    }

    pub fn from(&self) -> Ipv4Addr {
        match *self {
            TransportEvent::EchoReply { from, .. }
            | TransportEvent::TimeExceeded { from, .. }
            | TransportEvent::Unreachable { from, .. } => from,
        }
    }
}

/// Sends TTL-limited echo probes to one target and reads the answers.
pub trait Transport: Send {
    fn send(&mut self, seq: u16, ttl: u8) -> std::io::Result<()>;
    /// Waits at most `timeout`. Returns the event and the instant it was read.
    fn recv(&mut self, timeout: Duration) -> std::io::Result<Option<(TransportEvent, Instant)>>;
}
