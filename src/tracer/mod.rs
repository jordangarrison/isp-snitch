//! mtr-style round logic for one target, independent of sockets and clocks.
//!
//! A round sends one probe per TTL from 1 to the horizon. Rounds overlap
//! (timeout > interval) and complete in start order. See the design spec,
//! "Probing", for the horizon and truncation rules.

#[cfg(test)]
mod fake;
pub mod runner;

use crate::icmp::TransportEvent;
use std::collections::{HashMap, VecDeque};
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeOutcome {
    pub ttl: u8,
    pub rtt: Option<Duration>,
    pub responder: Option<Ipv4Addr>,
    /// The target itself answered this probe (echo reply).
    pub reached: bool,
    pub send_error: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompletedRound {
    pub target_id: i64,
    /// Wall-clock start of the round, unix seconds. Buckets data by minute.
    pub started_unix: i64,
    pub probes: Vec<ProbeOutcome>,
}

struct Slot {
    seq: u16,
    sent_at: Instant,
    done: bool,
    outcome: ProbeOutcome,
}

struct RoundState {
    no: u64,
    started_unix: i64,
    slots: Vec<Slot>,
}

pub struct TracerCore {
    target_id: i64,
    max_ttl: u8,
    timeout: Duration,
    next_seq: u16,
    next_round_no: u64,
    horizon: u8,
    d_known: Option<u8>,
    rounds: VecDeque<RoundState>,
    /// seq -> (round number, slot index), for probes still waiting.
    inflight: HashMap<u16, (u64, usize)>,
}

impl TracerCore {
    pub fn new(target_id: i64, max_ttl: u8, timeout: Duration) -> Self {
        Self {
            target_id,
            max_ttl,
            timeout,
            next_seq: 1,
            next_round_no: 0,
            horizon: max_ttl,
            d_known: None,
            rounds: VecDeque::new(),
            inflight: HashMap::new(),
        }
    }

    pub fn horizon(&self) -> u8 {
        self.horizon
    }

    #[cfg(test)]
    fn set_next_seq(&mut self, seq: u16) {
        self.next_seq = seq;
    }

    /// Earliest instant at which a waiting probe times out.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.rounds
            .iter()
            .flat_map(|r| r.slots.iter())
            .filter(|s| !s.done)
            .map(|s| s.sent_at + self.timeout)
            .min()
    }

    pub fn is_idle(&self) -> bool {
        self.rounds.is_empty()
    }

    fn alloc_seq(&mut self) -> u16 {
        loop {
            let s = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            if !self.inflight.contains_key(&s) {
                return s;
            }
        }
    }

    /// Starts a round; returns the (seq, ttl) probes to send, in TTL order.
    pub fn begin_round(&mut self, now: Instant, started_unix: i64) -> Vec<(u16, u8)> {
        let no = self.next_round_no;
        self.next_round_no += 1;
        let mut slots = Vec::with_capacity(self.horizon as usize);
        let mut out = Vec::with_capacity(self.horizon as usize);
        for ttl in 1..=self.horizon {
            let seq = self.alloc_seq();
            self.inflight.insert(seq, (no, slots.len()));
            slots.push(Slot {
                seq,
                sent_at: now,
                done: false,
                outcome: ProbeOutcome {
                    ttl,
                    rtt: None,
                    responder: None,
                    reached: false,
                    send_error: false,
                },
            });
            out.push((seq, ttl));
        }
        self.rounds.push_back(RoundState {
            no,
            started_unix,
            slots,
        });
        out
    }

    /// Records a round in which no probe could be sent (no usable socket):
    /// one TTL-1 probe marked as a send error.
    pub fn begin_failed_round(&mut self, now: Instant, started_unix: i64) {
        let no = self.next_round_no;
        self.next_round_no += 1;
        self.rounds.push_back(RoundState {
            no,
            started_unix,
            slots: vec![Slot {
                seq: 0,
                sent_at: now,
                done: true,
                outcome: ProbeOutcome {
                    ttl: 1,
                    rtt: None,
                    responder: None,
                    reached: false,
                    send_error: true,
                },
            }],
        });
    }

    fn slot_mut(&mut self, seq: u16) -> Option<&mut Slot> {
        let (no, idx) = self.inflight.remove(&seq)?;
        let front = self.rounds.front()?.no;
        let round = self.rounds.get_mut((no - front) as usize)?;
        round.slots.get_mut(idx).filter(|s| s.seq == seq && !s.done)
    }

    /// Updates the probe's send time to when it actually left.
    pub fn mark_sent(&mut self, seq: u16, at: Instant) {
        if let Some(&(no, idx)) = self.inflight.get(&seq)
            && let Some(front) = self.rounds.front().map(|r| r.no)
            && let Some(slot) = self
                .rounds
                .get_mut((no - front) as usize)
                .and_then(|r| r.slots.get_mut(idx))
        {
            slot.sent_at = at;
        }
    }

    pub fn on_send_error(&mut self, seq: u16) {
        if let Some(slot) = self.slot_mut(seq) {
            slot.done = true;
            slot.outcome.send_error = true;
        }
    }

    pub fn on_event(&mut self, ev: TransportEvent, at: Instant) {
        let timeout = self.timeout;
        let Some(slot) = self.slot_mut(ev.seq()) else {
            return;
        };
        let rtt = at.saturating_duration_since(slot.sent_at);
        slot.done = true;
        if rtt > timeout {
            return; // arrived after its timeout: counts as lost
        }
        slot.outcome.rtt = Some(rtt);
        slot.outcome.responder = Some(ev.from());
        slot.outcome.reached = matches!(ev, TransportEvent::EchoReply { .. });
    }

    /// Times out overdue probes and returns completed rounds in start order.
    pub fn poll_completed(&mut self, now: Instant) -> Vec<CompletedRound> {
        let timeout = self.timeout;
        for round in &mut self.rounds {
            for slot in &mut round.slots {
                if !slot.done && now >= slot.sent_at + timeout {
                    slot.done = true;
                    self.inflight.remove(&slot.seq);
                }
            }
        }
        let mut out = Vec::new();
        while self
            .rounds
            .front()
            .is_some_and(|r| r.slots.iter().all(|s| s.done))
        {
            let round = self.rounds.pop_front().expect("front checked");
            out.push(self.finish(round));
        }
        out
    }

    fn finish(&mut self, round: RoundState) -> CompletedRound {
        let mut probes: Vec<ProbeOutcome> = round.slots.into_iter().map(|s| s.outcome).collect();
        let failed_round = probes.len() == 1 && probes[0].send_error;
        let d_round = probes.iter().filter(|p| p.reached).map(|p| p.ttl).min();
        let cut = match d_round {
            Some(d) => {
                self.d_known = Some(d);
                self.horizon = d;
                Some(d)
            }
            None => {
                if !failed_round {
                    self.horizon = self.max_ttl;
                }
                self.d_known
            }
        };
        if let Some(cut) = cut {
            probes.retain(|p| p.ttl <= cut);
        }
        CompletedRound {
            target_id: self.target_id,
            started_unix: round.started_unix,
            probes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icmp::TransportEvent;

    const T: Duration = Duration::from_secs(2);
    const TARGET: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);

    fn hop(ttl: u8) -> Ipv4Addr {
        Ipv4Addr::new(10, 0, 0, ttl)
    }

    /// Answers every probe like a path whose target sits at TTL `d`.
    fn answer_all(core: &mut TracerCore, probes: &[(u16, u8)], d: u8, at: Instant) {
        for &(seq, ttl) in probes {
            let ev = if ttl >= d {
                TransportEvent::EchoReply { seq, from: TARGET }
            } else {
                TransportEvent::TimeExceeded {
                    seq,
                    from: hop(ttl),
                }
            };
            core.on_event(ev, at);
        }
    }

    fn ttls(r: &CompletedRound) -> Vec<u8> {
        r.probes.iter().map(|p| p.ttl).collect()
    }

    #[test]
    fn first_round_probes_to_max_ttl() {
        let mut core = TracerCore::new(1, 30, T);
        assert_eq!(core.horizon(), 30);
        let probes = core.begin_round(Instant::now(), 0);
        let got: Vec<u8> = probes.iter().map(|p| p.1).collect();
        assert_eq!(got, (1..=30).collect::<Vec<u8>>());
    }

    #[test]
    fn horizon_shrinks_to_target_distance() {
        let t0 = Instant::now();
        let mut core = TracerCore::new(1, 30, T);
        let p = core.begin_round(t0, 100);
        answer_all(&mut core, &p, 5, t0 + Duration::from_millis(10));
        let done = core.poll_completed(t0 + Duration::from_millis(20));
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].target_id, 1);
        assert_eq!(done[0].started_unix, 100);
        assert_eq!(core.horizon(), 5);
        assert_eq!(core.begin_round(t0 + Duration::from_secs(1), 101).len(), 5);
    }

    #[test]
    fn path_lengthens_heals() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut core = TracerCore::new(1, 30, T);
        let p = core.begin_round(t0, 0);
        answer_all(&mut core, &p, 5, t0 + ms(5));
        core.poll_completed(t0 + ms(10));
        assert_eq!(core.horizon(), 5);
        // Path is now 7 hops: probes up to TTL 5 never reach the target.
        let p = core.begin_round(t0 + ms(1000), 1);
        answer_all(&mut core, &p, 7, t0 + ms(1005));
        core.poll_completed(t0 + ms(1010));
        assert_eq!(core.horizon(), 30);
        let p = core.begin_round(t0 + ms(2000), 2);
        answer_all(&mut core, &p, 7, t0 + ms(2005));
        let done = core.poll_completed(t0 + ms(2010));
        assert_eq!(core.horizon(), 7);
        assert_eq!(ttls(&done[0]), (1..=7).collect::<Vec<u8>>());
        assert!(done[0].probes[6].reached);
    }

    #[test]
    fn truncates_probes_past_target() {
        let t0 = Instant::now();
        let mut core = TracerCore::new(1, 30, T);
        let p = core.begin_round(t0, 0);
        answer_all(&mut core, &p, 5, t0 + Duration::from_millis(3));
        let done = core.poll_completed(t0 + Duration::from_millis(4));
        assert_eq!(ttls(&done[0]), vec![1, 2, 3, 4, 5]);
        let last = &done[0].probes[4];
        assert!(last.reached);
        assert_eq!(last.responder, Some(TARGET));
        assert_eq!(last.rtt, Some(Duration::from_millis(3)));
        assert_eq!(done[0].probes[0].responder, Some(hop(1)));
        assert!(!done[0].probes[0].reached);
    }

    #[test]
    fn no_reply_round_truncates_to_known_distance() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut core = TracerCore::new(1, 30, T);
        let p = core.begin_round(t0, 0);
        answer_all(&mut core, &p, 5, t0 + ms(5));
        core.poll_completed(t0 + ms(10));
        // Total loss: nothing answers, every probe times out.
        core.begin_round(t0 + ms(1000), 1);
        assert!(core.poll_completed(t0 + ms(2999)).is_empty());
        let done = core.poll_completed(t0 + ms(3000));
        assert_eq!(done.len(), 1);
        assert_eq!(ttls(&done[0]), vec![1, 2, 3, 4, 5]);
        assert!(
            done[0]
                .probes
                .iter()
                .all(|p| p.rtt.is_none() && !p.send_error)
        );
        assert_eq!(core.horizon(), 30);
    }

    #[test]
    fn no_known_distance_keeps_all_probes() {
        let t0 = Instant::now();
        let mut core = TracerCore::new(1, 10, T);
        core.begin_round(t0, 0);
        let done = core.poll_completed(t0 + T);
        assert_eq!(done[0].probes.len(), 10);
    }

    #[test]
    fn late_and_duplicate_replies_ignored() {
        let t0 = Instant::now();
        let mut core = TracerCore::new(1, 3, T);
        let p = core.begin_round(t0, 0);
        let (seq1, _) = p[0];
        let (seq3, _) = p[2];
        core.on_event(
            TransportEvent::TimeExceeded {
                seq: seq1,
                from: hop(1),
            },
            t0 + Duration::from_millis(1),
        );
        // Duplicate answer for the same probe.
        core.on_event(
            TransportEvent::TimeExceeded {
                seq: seq1,
                from: hop(9),
            },
            t0 + Duration::from_millis(2),
        );
        let done = core.poll_completed(t0 + T);
        // Late answer after the round timed out.
        core.on_event(
            TransportEvent::EchoReply {
                seq: seq3,
                from: TARGET,
            },
            t0 + T + Duration::from_millis(1),
        );
        // Answer for a seq this tracer never sent.
        core.on_event(
            TransportEvent::EchoReply {
                seq: seq3.wrapping_add(100),
                from: TARGET,
            },
            t0 + T,
        );
        assert_eq!(done.len(), 1);
        let r = &done[0];
        assert_eq!(r.probes[0].responder, Some(hop(1)));
        assert_eq!(r.probes.iter().filter(|p| p.rtt.is_some()).count(), 1);
        assert!(core.poll_completed(t0 + T * 2).is_empty());
    }

    #[test]
    fn seq_wraps() {
        let t0 = Instant::now();
        let mut core = TracerCore::new(1, 4, T);
        core.set_next_seq(65533);
        for i in 0..3u32 {
            let at = t0 + Duration::from_secs(i as u64);
            let p = core.begin_round(at, i as i64);
            answer_all(&mut core, &p, 4, at + Duration::from_millis(1));
            let done = core.poll_completed(at + Duration::from_millis(2));
            assert_eq!(done.len(), 1, "round {i}");
            assert!(done[0].probes.last().unwrap().reached, "round {i}");
            assert!(done[0].probes.iter().all(|p| p.rtt.is_some()), "round {i}");
        }
    }

    #[test]
    fn send_error_counts_as_lost() {
        let t0 = Instant::now();
        let mut core = TracerCore::new(1, 2, T);
        let p = core.begin_round(t0, 0);
        core.on_send_error(p[0].0);
        core.on_send_error(p[1].0);
        let done = core.poll_completed(t0);
        assert_eq!(
            done.len(),
            1,
            "a round whose sends all failed is complete at once"
        );
        assert!(
            done[0]
                .probes
                .iter()
                .all(|p| p.send_error && p.rtt.is_none())
        );
    }

    #[test]
    fn failed_round_is_single_ttl1_send_error() {
        let t0 = Instant::now();
        let mut core = TracerCore::new(1, 30, T);
        core.begin_failed_round(t0, 50);
        let done = core.poll_completed(t0);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].started_unix, 50);
        assert_eq!(ttls(&done[0]), vec![1]);
        assert!(done[0].probes[0].send_error);
    }

    #[test]
    fn overlapping_rounds_complete_in_start_order() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut core = TracerCore::new(1, 2, T);
        let a = core.begin_round(t0, 0);
        let b = core.begin_round(t0 + ms(1000), 1);
        answer_all(&mut core, &b, 2, t0 + ms(1001));
        // Round b is fully answered but round a is still waiting.
        assert!(core.poll_completed(t0 + ms(1002)).is_empty());
        answer_all(&mut core, &a, 2, t0 + ms(1003));
        let done = core.poll_completed(t0 + ms(1004));
        let order: Vec<i64> = done.iter().map(|r| r.started_unix).collect();
        assert_eq!(order, vec![0, 1]);
    }
}
