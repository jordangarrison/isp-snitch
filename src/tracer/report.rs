//! mtr-style text table for the one-shot `trace` command.

use super::CompletedRound;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::net::Ipv4Addr;

#[derive(Default)]
struct Row {
    sent: u32,
    recv: u32,
    sum_ms: f64,
    best_ms: Option<f64>,
    worst_ms: Option<f64>,
    hosts: HashMap<Ipv4Addr, u32>,
    send_errors: u32,
}

pub fn trace_table(rounds: &[CompletedRound]) -> String {
    let mut rows: BTreeMap<u8, Row> = BTreeMap::new();
    for p in rounds.iter().flat_map(|r| &r.probes) {
        let row = rows.entry(p.ttl).or_default();
        row.sent += 1;
        row.send_errors += p.send_error as u32;
        if let (Some(rtt), Some(host)) = (p.rtt, p.responder) {
            let ms = rtt.as_secs_f64() * 1000.0;
            row.recv += 1;
            row.sum_ms += ms;
            row.best_ms = Some(row.best_ms.map_or(ms, |b| b.min(ms)));
            row.worst_ms = Some(row.worst_ms.map_or(ms, |w| w.max(ms)));
            *row.hosts.entry(host).or_default() += 1;
        }
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:>3}  {:<16} {:>6} {:>5} {:>8} {:>8} {:>8} {:>6}",
        "TTL", "HOST", "LOSS%", "SENT", "AVG", "BEST", "WORST", "SENDERR"
    );
    let fmt = |v: Option<f64>| v.map_or("-".to_string(), |v| format!("{v:.1}"));
    for (ttl, r) in &rows {
        let host = r
            .hosts
            .iter()
            .max_by_key(|(ip, n)| (**n, std::cmp::Reverse(**ip)))
            .map_or("???".to_string(), |(ip, _)| ip.to_string());
        let loss = 100.0 * (r.sent - r.recv) as f64 / r.sent as f64;
        let avg = (r.recv > 0).then(|| r.sum_ms / r.recv as f64);
        let _ = writeln!(
            out,
            "{:>3}  {:<16} {:>6.1} {:>5} {:>8} {:>8} {:>8} {:>6}",
            ttl,
            host,
            loss,
            r.sent,
            fmt(avg),
            fmt(r.best_ms),
            fmt(r.worst_ms),
            r.send_errors
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracer::ProbeOutcome;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    fn probe(ttl: u8, ms: Option<u64>, host: [u8; 4], reached: bool) -> ProbeOutcome {
        ProbeOutcome {
            ttl,
            rtt: ms.map(Duration::from_millis),
            responder: ms.map(|_| Ipv4Addr::from(host)),
            reached,
            send_error: false,
        }
    }

    #[test]
    fn table_summarizes_rounds_per_ttl() {
        let rounds = vec![
            CompletedRound {
                target_id: 0,
                started_unix: 0,
                probes: vec![
                    probe(1, Some(1), [192, 168, 1, 1], false),
                    probe(2, Some(10), [1, 1, 1, 1], true),
                ],
            },
            CompletedRound {
                target_id: 0,
                started_unix: 1,
                probes: vec![
                    probe(1, Some(3), [192, 168, 1, 1], false),
                    probe(2, None, [0, 0, 0, 0], false),
                ],
            },
        ];
        let out = trace_table(&rounds);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].contains("HOST") && lines[0].contains("LOSS%"));
        assert!(lines[1].contains("192.168.1.1"), "{}", lines[1]);
        assert!(lines[1].contains("0.0"), "{}", lines[1]);
        assert!(lines[1].contains("2.0"), "avg of 1 and 3 ms: {}", lines[1]);
        assert!(lines[2].contains("1.1.1.1"), "{}", lines[2]);
        assert!(lines[2].contains("50.0"), "{}", lines[2]);
    }

    #[test]
    fn silent_hop_shows_stars() {
        let rounds = vec![CompletedRound {
            target_id: 0,
            started_unix: 0,
            probes: vec![probe(1, None, [0, 0, 0, 0], false)],
        }];
        assert!(trace_table(&rounds).lines().nth(1).unwrap().contains("???"));
    }
}
