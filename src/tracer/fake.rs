//! Scripted network path for tracer tests.

use crate::icmp::{Transport, TransportEvent};
use std::collections::{HashSet, VecDeque};
use std::io;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// Hop `i` answers TimeExceeded from 10.0.0.i; the target answers at TTL `distance`.
pub struct FakeNet {
    pub distance: u8,
    pub target: Ipv4Addr,
    /// TTLs whose probes get no answer.
    pub silent: HashSet<u8>,
    queue: VecDeque<TransportEvent>,
    pub sent: Vec<(u16, u8)>,
}

impl FakeNet {
    pub fn new(distance: u8) -> Self {
        Self {
            distance,
            target: Ipv4Addr::new(1, 1, 1, 1),
            silent: HashSet::new(),
            queue: VecDeque::new(),
            sent: Vec::new(),
        }
    }
}

impl Transport for FakeNet {
    fn send(&mut self, seq: u16, ttl: u8) -> io::Result<()> {
        self.sent.push((seq, ttl));
        if self.silent.contains(&ttl) {
            return Ok(());
        }
        let ev = if ttl >= self.distance {
            TransportEvent::EchoReply {
                seq,
                from: self.target,
            }
        } else {
            TransportEvent::TimeExceeded {
                seq,
                from: Ipv4Addr::new(10, 0, 0, ttl),
            }
        };
        self.queue.push_back(ev);
        Ok(())
    }

    fn recv(&mut self, timeout: Duration) -> io::Result<Option<(TransportEvent, Instant)>> {
        match self.queue.pop_front() {
            Some(ev) => Ok(Some((ev, Instant::now()))),
            None => {
                std::thread::sleep(timeout.min(Duration::from_millis(1)));
                Ok(None)
            }
        }
    }
}
