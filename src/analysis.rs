//! Turns stored hop buckets into what the UI shows: loss, effective loss,
//! rate-limit flags, and the target's own loss and latency.

pub use crate::store::{HopBucket, Table, TargetBucket};
use serde::Serialize;

pub const STEPS: [i64; 8] = [60, 300, 900, 1800, 3600, 10_800, 21_600, 86_400];
pub const MAX_POINTS: i64 = 720;
/// Raw minus effective loss, in percentage points, that marks a hop as rate-limited.
const RATE_LIMIT_GAP_PCT: f64 = 5.0;
const RATE_LIMIT_MIN_SENT: u32 = 10;

/// Smallest step from the ladder that keeps `[from, to)` at or under `max_points` buckets.
pub fn pick_step(from: i64, to: i64, max_points: i64) -> i64 {
    let span = (to - from).max(1);
    STEPS
        .into_iter()
        .find(|s| (span + s - 1) / s <= max_points)
        .unwrap_or(STEPS[STEPS.len() - 1])
}

pub fn pick_table(from: i64, now: i64, step: i64, minute_days: u32) -> Table {
    if step >= 3600 || from < now - minute_days as i64 * 86_400 {
        Table::Hour
    } else {
        Table::Minute
    }
}

/// Step and table for a range. Hourly rows never go into buckets shorter
/// than an hour, which would show 11 of 12 buckets as gaps.
pub fn pick_step_table(from: i64, to: i64, now: i64, minute_days: u32) -> (i64, Table) {
    let step = pick_step(from, to, MAX_POINTS);
    match pick_table(from, now, step, minute_days) {
        Table::Hour => (step.max(3600), Table::Hour),
        Table::Minute => (step, Table::Minute),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HopStat {
    pub ttl: u8,
    pub addr: Option<String>,
    pub sent: u32,
    pub recv: u32,
    pub loss_pct: f64,
    /// Lowest loss of this hop and every hop after it. Loss that does not
    /// carry through to the target is ICMP rate limiting, not real loss.
    pub eff_loss_pct: f64,
    /// Effective loss that starts at this hop: its `eff_loss_pct` minus the
    /// previous hop's. Summed over a path it equals the target's loss.
    pub added_loss_pct: f64,
    pub rate_limited: bool,
    pub reached: bool,
    pub avg_ms: Option<f64>,
    pub min_ms: Option<f64>,
    pub max_ms: Option<f64>,
}

/// Analyzes one bucket's rows: sorted by ttl, cut at the first hop the target answered.
pub fn analyze(rows: &[HopBucket]) -> Vec<HopStat> {
    let mut rows: Vec<&HopBucket> = rows.iter().collect();
    rows.sort_by_key(|r| r.ttl);
    if let Some(cut) = rows.iter().position(|r| r.reached) {
        rows.truncate(cut + 1);
    }
    let mut stats: Vec<HopStat> = rows
        .iter()
        .map(|r| {
            let loss_pct = if r.sent == 0 {
                0.0
            } else {
                100.0 * (r.sent.saturating_sub(r.recv)) as f64 / r.sent as f64
            };
            HopStat {
                ttl: r.ttl,
                addr: r.addr.clone(),
                sent: r.sent,
                recv: r.recv,
                loss_pct,
                eff_loss_pct: loss_pct,
                added_loss_pct: 0.0,
                rate_limited: false,
                reached: r.reached,
                avg_ms: (r.recv > 0).then(|| r.rtt_sum_us as f64 / r.recv as f64 / 1000.0),
                min_ms: r.rtt_min_us.map(|v| v as f64 / 1000.0),
                max_ms: r.rtt_max_us.map(|v| v as f64 / 1000.0),
            }
        })
        .collect();
    let mut floor = f64::INFINITY;
    for s in stats.iter_mut().rev() {
        floor = floor.min(s.loss_pct);
        s.eff_loss_pct = floor;
        s.rate_limited =
            s.sent >= RATE_LIMIT_MIN_SENT && s.loss_pct - s.eff_loss_pct >= RATE_LIMIT_GAP_PCT;
    }
    let mut prev = 0.0;
    for s in stats.iter_mut() {
        s.added_loss_pct = s.eff_loss_pct - prev;
        prev = s.eff_loss_pct;
    }
    stats
}

/// Sums many buckets into one row per ttl (for a whole range or a slice).
/// `addr` comes from the bucket with the most answers at that ttl.
pub fn merge_by_ttl(rows: &[HopBucket]) -> Vec<HopBucket> {
    let mut out: std::collections::BTreeMap<u8, (HopBucket, u32)> = Default::default();
    for r in rows {
        let (m, best_recv) = out.entry(r.ttl).or_insert_with(|| {
            (
                HopBucket {
                    ts: r.ts,
                    ttl: r.ttl,
                    sent: 0,
                    recv: 0,
                    rtt_sum_us: 0,
                    rtt_min_us: None,
                    rtt_max_us: None,
                    addr: None,
                    reached: false,
                },
                0,
            )
        });
        m.ts = m.ts.min(r.ts);
        m.sent += r.sent;
        m.recv += r.recv;
        m.rtt_sum_us += r.rtt_sum_us;
        m.rtt_min_us = match (m.rtt_min_us, r.rtt_min_us) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        m.rtt_max_us = match (m.rtt_max_us, r.rtt_max_us) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        m.reached |= r.reached;
        if r.addr.is_some() && (m.addr.is_none() || r.recv >= *best_recv) {
            m.addr = r.addr.clone();
            *best_recv = r.recv;
        }
    }
    out.into_values().map(|(m, _)| m).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct TargetPoint {
    pub loss_pct: f64,
    pub avg_ms: Option<f64>,
    pub max_ms: Option<f64>,
}

/// The target's loss and latency for one bucket. `None` = no probes (a gap).
pub fn target_point(b: &TargetBucket) -> Option<TargetPoint> {
    if b.sent == 0 {
        return None;
    }
    Some(TargetPoint {
        loss_pct: 100.0 * b.sent.saturating_sub(b.recv) as f64 / b.sent as f64,
        avg_ms: (b.recv > 0).then(|| b.rtt_sum_us as f64 / b.recv as f64 / 1000.0),
        max_ms: b.rtt_max_us.map(|v| v as f64 / 1000.0),
    })
}

/// Sums target buckets into one, for a whole-range summary.
pub fn sum_targets(buckets: &[TargetBucket]) -> TargetBucket {
    buckets.iter().fold(
        TargetBucket {
            ts: buckets.first().map_or(0, |b| b.ts),
            sent: 0,
            recv: 0,
            rtt_sum_us: 0,
            rtt_max_us: None,
        },
        |mut acc, b| {
            acc.sent += b.sent;
            acc.recv += b.recv;
            acc.rtt_sum_us += b.rtt_sum_us;
            acc.rtt_max_us = acc.rtt_max_us.max(b.rtt_max_us);
            acc
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(ttl: u8, sent: u32, recv: u32, reached: bool) -> HopBucket {
        HopBucket {
            ts: 0,
            ttl,
            sent,
            recv,
            rtt_sum_us: recv as u64 * 10_000,
            rtt_min_us: (recv > 0).then_some(5_000),
            rtt_max_us: (recv > 0).then_some(20_000),
            addr: (recv > 0).then(|| format!("10.0.0.{ttl}")),
            reached,
        }
    }

    #[test]
    fn step_ladder() {
        assert_eq!(pick_step(0, 86_400, 720), 300);
        assert_eq!(pick_step(0, 6 * 3600, 720), 60);
        assert_eq!(pick_step(0, 7 * 86_400, 720), 900);
        assert_eq!(pick_step(0, 30 * 86_400, 720), 3600);
        assert_eq!(pick_step(0, 90 * 86_400, 720), 10_800);
        assert_eq!(pick_step(0, 3000 * 86_400, 720), 86_400);
    }

    #[test]
    fn table_choice() {
        let now = 100 * 86_400;
        assert_eq!(pick_table(now - 86_400, now, 300, 90), Table::Minute);
        assert_eq!(pick_table(now - 86_400, now, 3600, 90), Table::Hour);
        assert_eq!(pick_table(now - 91 * 86_400, now, 300, 90), Table::Hour);
    }

    #[test]
    fn rate_limited_hop_has_zero_effective_loss() {
        let rows = vec![
            b(1, 60, 60, false),
            b(2, 60, 60, false),
            b(3, 60, 42, false),
            b(4, 60, 60, false),
            b(5, 60, 60, true),
        ];
        let s = analyze(&rows);
        assert_eq!(s[2].ttl, 3);
        assert!((s[2].loss_pct - 30.0).abs() < 1e-9);
        assert_eq!(s[2].eff_loss_pct, 0.0);
        assert!(s[2].rate_limited);
        assert!(!s[1].rate_limited);
    }

    #[test]
    fn real_loss_carries_through() {
        let rows = vec![
            b(1, 100, 100, false),
            b(2, 100, 100, false),
            b(3, 100, 90, false),
            b(4, 100, 90, true),
        ];
        let s = analyze(&rows);
        assert!((s[2].eff_loss_pct - 10.0).abs() < 1e-9);
        assert!((s[3].eff_loss_pct - 10.0).abs() < 1e-9);
        assert!(!s[2].rate_limited);
        assert_eq!(s[1].eff_loss_pct, 0.0);
    }

    #[test]
    fn added_loss_is_where_effective_loss_starts() {
        let rows = vec![
            b(1, 100, 98, false),
            b(2, 100, 60, false),
            b(3, 100, 95, false),
            b(4, 100, 93, true),
        ];
        let added: Vec<f64> = analyze(&rows).iter().map(|s| s.added_loss_pct).collect();
        let want = [2.0, 3.0, 0.0, 2.0];
        for (a, w) in added.iter().zip(want) {
            assert!((a - w).abs() < 1e-9, "{added:?}");
        }
        let total: f64 = added.iter().sum();
        assert!((total - 7.0).abs() < 1e-9, "sums to target loss");
    }

    #[test]
    fn small_samples_are_not_flagged() {
        let rows = vec![b(1, 4, 2, false), b(2, 4, 4, true)];
        assert!(!analyze(&rows)[0].rate_limited);
    }

    #[test]
    fn rows_past_reached_hop_dropped_and_sorted() {
        let rows = vec![
            b(4, 10, 10, true),
            b(1, 10, 10, false),
            b(3, 10, 10, true),
            b(2, 10, 10, false),
        ];
        let ttls: Vec<u8> = analyze(&rows).iter().map(|s| s.ttl).collect();
        assert_eq!(ttls, vec![1, 2, 3]);
    }

    #[test]
    fn latency_in_ms() {
        let s = analyze(&[b(1, 10, 10, true)]);
        assert_eq!(s[0].avg_ms, Some(10.0));
        assert_eq!(s[0].min_ms, Some(5.0));
        assert_eq!(s[0].max_ms, Some(20.0));
        assert_eq!(s[0].addr.as_deref(), Some("10.0.0.1"));
    }

    fn tb(sent: u32, recv: u32, max: Option<u32>) -> TargetBucket {
        TargetBucket {
            ts: 0,
            sent,
            recv,
            rtt_sum_us: recv as u64 * 10_000,
            rtt_max_us: max,
        }
    }

    #[test]
    fn target_point_from_bucket() {
        let p = target_point(&tb(10, 9, Some(20_000))).unwrap();
        assert!((p.loss_pct - 10.0).abs() < 1e-9);
        assert_eq!(p.avg_ms, Some(10.0));
        assert_eq!(p.max_ms, Some(20.0));
    }

    #[test]
    fn nothing_reached_is_total_loss() {
        let p = target_point(&tb(10, 0, None)).unwrap();
        assert_eq!(p.loss_pct, 100.0);
        assert_eq!(p.avg_ms, None);
    }

    #[test]
    fn empty_bucket_is_a_gap() {
        assert_eq!(target_point(&tb(0, 0, None)), None);
    }

    #[test]
    fn sum_targets_adds_counts_and_keeps_worst() {
        let t = sum_targets(&[tb(10, 9, Some(5)), tb(10, 10, Some(9)), tb(0, 0, None)]);
        assert_eq!((t.sent, t.recv, t.rtt_max_us), (20, 19, Some(9)));
    }

    #[test]
    fn merge_by_ttl_sums_buckets() {
        let mut a = b(1, 10, 8, false);
        a.ts = 0;
        let mut c = b(1, 10, 10, false);
        c.ts = 60;
        c.addr = Some("10.0.0.99".into());
        c.rtt_min_us = Some(1_000);
        let d = b(2, 10, 10, true);
        let m = merge_by_ttl(&[a, c, d]);
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].ttl, m[0].sent, m[0].recv), (1, 20, 18));
        assert_eq!(m[0].rtt_min_us, Some(1_000));
        assert_eq!(m[0].rtt_max_us, Some(20_000));
        assert_eq!(
            m[0].addr.as_deref(),
            Some("10.0.0.99"),
            "addr of the bucket with most answers"
        );
        assert!(m[1].reached);
    }

    #[test]
    fn old_short_range_uses_hourly_step() {
        let now = 200 * 86_400;
        let from = now - 120 * 86_400;
        assert_eq!(
            pick_step_table(from, from + 6 * 3600, now, 90),
            (3600, Table::Hour)
        );
        assert_eq!(
            pick_step_table(now - 6 * 3600, now, now, 90),
            (60, Table::Minute)
        );
        assert_eq!(
            pick_step_table(now - 30 * 86_400, now, now, 90),
            (3600, Table::Hour)
        );
    }
}
