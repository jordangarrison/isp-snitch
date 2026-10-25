//! Folds completed rounds into one row per (target, minute, ttl).

use crate::tracer::CompletedRound;
use std::collections::{BTreeMap, HashMap};
use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq)]
pub struct HopMinute {
    pub target_id: i64,
    /// Minute start, unix seconds.
    pub ts: i64,
    pub ttl: u8,
    pub sent: u32,
    pub recv: u32,
    pub rtt_sum_us: u64,
    pub rtt_sumsq_us2: f64,
    pub rtt_min_us: Option<u32>,
    pub rtt_max_us: Option<u32>,
    /// Responder seen most often at this TTL.
    pub addr: Option<String>,
    /// Number of distinct responders (more than 1 = path change or ECMP).
    pub addr_count: u32,
    pub reached: bool,
    pub send_errors: u32,
}

#[derive(Default)]
struct Acc {
    sent: u32,
    recv: u32,
    rtt_sum_us: u64,
    rtt_sumsq_us2: f64,
    rtt_min_us: Option<u32>,
    rtt_max_us: Option<u32>,
    addrs: HashMap<Ipv4Addr, u32>,
    reached: bool,
    send_errors: u32,
}

#[derive(Default)]
pub struct MinuteAggregator {
    buckets: BTreeMap<(i64, i64, u8), Acc>,
}

impl MinuteAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, r: &CompletedRound) {
        let ts = r.started_unix.div_euclid(60) * 60;
        for p in &r.probes {
            let acc = self.buckets.entry((ts, r.target_id, p.ttl)).or_default();
            acc.sent += 1;
            acc.send_errors += p.send_error as u32;
            acc.reached |= p.reached;
            if let (Some(rtt), Some(addr)) = (p.rtt, p.responder) {
                let us = rtt.as_micros().min(u32::MAX as u128) as u32;
                acc.recv += 1;
                acc.rtt_sum_us += us as u64;
                acc.rtt_sumsq_us2 += (us as f64) * (us as f64);
                acc.rtt_min_us = Some(acc.rtt_min_us.map_or(us, |m| m.min(us)));
                acc.rtt_max_us = Some(acc.rtt_max_us.map_or(us, |m| m.max(us)));
                *acc.addrs.entry(addr).or_default() += 1;
            }
        }
    }

    /// Removes and returns every row whose minute starts before `ts`.
    pub fn drain_before(&mut self, ts: i64) -> Vec<HopMinute> {
        let keep = self.buckets.split_off(&(ts, i64::MIN, 0));
        let done = std::mem::replace(&mut self.buckets, keep);
        done.into_iter().map(to_row).collect()
    }

    pub fn drain_all(&mut self) -> Vec<HopMinute> {
        std::mem::take(&mut self.buckets)
            .into_iter()
            .map(to_row)
            .collect()
    }
}

fn to_row(((ts, target_id, ttl), acc): ((i64, i64, u8), Acc)) -> HopMinute {
    let addr = acc
        .addrs
        .iter()
        .max_by_key(|(ip, n)| (**n, std::cmp::Reverse(**ip)))
        .map(|(ip, _)| ip.to_string());
    HopMinute {
        target_id,
        ts,
        ttl,
        sent: acc.sent,
        recv: acc.recv,
        rtt_sum_us: acc.rtt_sum_us,
        rtt_sumsq_us2: acc.rtt_sumsq_us2,
        rtt_min_us: acc.rtt_min_us,
        rtt_max_us: acc.rtt_max_us,
        addr,
        addr_count: acc.addrs.len() as u32,
        reached: acc.reached,
        send_errors: acc.send_errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracer::ProbeOutcome;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    fn p(ttl: u8, ms: Option<u64>, host: u8, reached: bool) -> ProbeOutcome {
        ProbeOutcome {
            ttl,
            rtt: ms.map(Duration::from_millis),
            responder: ms.map(|_| Ipv4Addr::new(10, 0, 0, host)),
            reached,
            send_error: false,
        }
    }

    fn round(started_unix: i64, probes: Vec<ProbeOutcome>) -> CompletedRound {
        CompletedRound {
            target_id: 7,
            started_unix,
            probes,
        }
    }

    #[test]
    fn rounds_in_one_minute_are_summed() {
        let mut a = MinuteAggregator::new();
        a.add(&round(
            120,
            vec![p(1, Some(2), 1, false), p(2, Some(10), 2, true)],
        ));
        a.add(&round(
            150,
            vec![p(1, Some(4), 1, false), p(2, None, 0, false)],
        ));
        let rows = a.drain_all();
        assert_eq!(rows.len(), 2);
        let h1 = &rows[0];
        assert_eq!((h1.target_id, h1.ts, h1.ttl), (7, 120, 1));
        assert_eq!((h1.sent, h1.recv), (2, 2));
        assert_eq!(h1.rtt_sum_us, 6000);
        assert_eq!(h1.rtt_sumsq_us2, 2000f64.powi(2) + 4000f64.powi(2));
        assert_eq!((h1.rtt_min_us, h1.rtt_max_us), (Some(2000), Some(4000)));
        assert_eq!(h1.addr.as_deref(), Some("10.0.0.1"));
        assert!(!h1.reached);
        let h2 = &rows[1];
        assert_eq!((h2.sent, h2.recv), (2, 1));
        assert!(h2.reached);
    }

    #[test]
    fn rounds_in_different_minutes_are_separate_rows() {
        let mut a = MinuteAggregator::new();
        a.add(&round(59, vec![p(1, Some(1), 1, true)]));
        a.add(&round(60, vec![p(1, Some(1), 1, true)]));
        let ts: Vec<i64> = a.drain_all().iter().map(|r| r.ts).collect();
        assert_eq!(ts, vec![0, 60]);
    }

    #[test]
    fn drain_before_keeps_current_minute() {
        let mut a = MinuteAggregator::new();
        a.add(&round(30, vec![p(1, Some(1), 1, true)]));
        a.add(&round(90, vec![p(1, Some(1), 1, true)]));
        let first = a.drain_before(60);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].ts, 0);
        let rest = a.drain_all();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].ts, 60);
    }

    #[test]
    fn most_common_responder_wins() {
        let mut a = MinuteAggregator::new();
        a.add(&round(0, vec![p(3, Some(1), 9, false)]));
        a.add(&round(1, vec![p(3, Some(1), 5, false)]));
        a.add(&round(2, vec![p(3, Some(1), 5, false)]));
        let rows = a.drain_all();
        assert_eq!(rows[0].addr.as_deref(), Some("10.0.0.5"));
        assert_eq!(rows[0].addr_count, 2);
    }

    #[test]
    fn send_errors_counted_and_lost() {
        let mut a = MinuteAggregator::new();
        let mut probe = p(1, None, 0, false);
        probe.send_error = true;
        a.add(&round(0, vec![probe]));
        let r = &a.drain_all()[0];
        assert_eq!((r.sent, r.recv, r.send_errors), (1, 0, 1));
        assert_eq!(r.addr, None);
        assert_eq!(r.rtt_min_us, None);
    }
}
