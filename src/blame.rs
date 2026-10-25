//! Where loss starts and how often it happens, across all targets: splits
//! loss into gateway, shared upstream and past-the-split segments, and groups
//! lossy buckets into incidents.

use crate::analysis::{HopStat, TargetPoint};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

/// A bucket counts toward an incident when a target loses at least this much.
pub const INCIDENT_MIN_PCT: f64 = 1.0;
const MAX_INCIDENTS: usize = 200;

/// One display bucket of one target.
#[derive(Debug, Clone, Default)]
pub struct GridBucket {
    /// The target's own loss and latency (`None` = gap).
    pub target: Option<TargetPoint>,
    /// `analyze` output for the bucket, sorted by ttl.
    pub hops: Vec<HopStat>,
}

/// One target's buckets. Every grid in a report shares the same bucket times.
#[derive(Debug, Clone)]
pub struct TargetGrid {
    pub target_id: i64,
    pub buckets: Vec<GridBucket>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Segment {
    /// `gateway` (hop 1), `upstream` (hops shared by every target's path) or
    /// `past_split` (hops after the paths divide).
    pub name: &'static str,
    /// `None` = the segment is empty.
    pub first_ttl: Option<u8>,
    /// `None` = open-ended.
    pub last_ttl: Option<u8>,
    /// Target loss, in percent, that starts inside this segment.
    pub loss_pct: f64,
    /// This segment's part of all loss, in percent (`None` with no loss).
    pub share_pct: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HopShare {
    pub ttl: u8,
    /// The responder, when every target's path has the same one at this ttl.
    pub addr: Option<String>,
    pub loss_pct: f64,
    pub share_pct: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Incident {
    pub start: i64,
    pub end: i64,
    pub peak_loss_pct: f64,
    pub targets_hit: usize,
    /// The hop where the most loss started during the incident.
    pub origin_ttl: Option<u8>,
    pub origin_addr: Option<String>,
    pub segment: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WhereReport {
    /// Last ttl every target's path shares (`None` with fewer than 2 targets).
    pub split_ttl: Option<u8>,
    /// Mean target loss over the range, in percent.
    pub loss_pct: f64,
    pub targets_total: usize,
    pub segments: Vec<Segment>,
    pub hops: Vec<HopShare>,
    /// Newest first.
    pub incidents: Vec<Incident>,
}

/// Bucket weight: probes sent at the bucket's last hop.
fn weight(b: &GridBucket) -> f64 {
    b.hops.last().map_or(0.0, |h| h.sent as f64)
}

/// Each target's most-answered responder per ttl over the whole range.
fn dominant_addrs(grid: &TargetGrid) -> BTreeMap<u8, String> {
    let mut counts: BTreeMap<u8, HashMap<&str, u32>> = BTreeMap::new();
    for h in grid.buckets.iter().flat_map(|b| &b.hops) {
        if let Some(a) = &h.addr {
            *counts.entry(h.ttl).or_default().entry(a).or_default() += h.recv;
        }
    }
    counts
        .into_iter()
        .filter_map(|(ttl, m)| {
            m.into_iter()
                .max_by_key(|(a, n)| (*n, std::cmp::Reverse(*a)))
                .map(|(a, _)| (ttl, a.to_string()))
        })
        .collect()
}

fn segment_name(ttl: u8, split: Option<u8>) -> &'static str {
    match split {
        _ if ttl <= 1 => "gateway",
        Some(s) if ttl > s => "past_split",
        _ => "upstream",
    }
}

pub fn report(ts: &[i64], step: i64, grids: &[TargetGrid]) -> WhereReport {
    let with_data: Vec<&TargetGrid> = grids
        .iter()
        .filter(|g| g.buckets.iter().any(|b| weight(b) > 0.0))
        .collect();
    let addrs: Vec<BTreeMap<u8, String>> = with_data.iter().map(|g| dominant_addrs(g)).collect();
    let common = |ttl: u8| -> Option<String> {
        let first = addrs.first()?.get(&ttl)?;
        addrs
            .iter()
            .all(|m| m.get(&ttl) == Some(first))
            .then(|| first.clone())
    };
    let split = (with_data.len() >= 2).then(|| {
        let max_ttl = addrs
            .iter()
            .flat_map(|m| m.keys().copied())
            .max()
            .unwrap_or(1);
        (2..=max_ttl)
            .rev()
            .find(|&t| common(t).is_some())
            .unwrap_or(1)
    });

    // Per ttl: mean over targets of the probe-weighted added loss.
    let mut per_ttl: BTreeMap<u8, f64> = BTreeMap::new();
    for g in &with_data {
        let total_w: f64 = g.buckets.iter().map(weight).sum();
        for b in &g.buckets {
            let w = weight(b);
            for h in &b.hops {
                *per_ttl.entry(h.ttl).or_default() +=
                    h.added_loss_pct * w / total_w / with_data.len() as f64;
            }
        }
    }
    // fold from +0.0: an empty f64 `sum()` is -0.0.
    let loss_pct = per_ttl.values().fold(0.0, |a, v| a + v);
    let share = |v: f64| (loss_pct > 0.0).then(|| 100.0 * v / loss_pct);
    let seg_loss = |name: &str| -> f64 {
        per_ttl
            .iter()
            .filter(|(t, _)| segment_name(**t, split) == name)
            .fold(0.0, |a, (_, v)| a + v)
    };
    let segments = [
        ("gateway", Some(1), Some(1)),
        ("upstream", Some(2), split),
        ("past_split", split.map(|s| s.saturating_add(1)), None),
    ]
    .into_iter()
    .map(|(name, first_ttl, last_ttl)| {
        let loss = seg_loss(name);
        Segment {
            name,
            first_ttl,
            last_ttl,
            loss_pct: loss,
            share_pct: share(loss),
        }
    })
    .collect();
    let hops = per_ttl
        .iter()
        .map(|(&ttl, &v)| HopShare {
            ttl,
            addr: common(ttl),
            loss_pct: v,
            share_pct: share(v),
        })
        .collect();

    WhereReport {
        split_ttl: split,
        loss_pct,
        targets_total: grids.len(),
        segments,
        hops,
        incidents: incidents(ts, step, grids, split),
    }
}

fn incidents(ts: &[i64], step: i64, grids: &[TargetGrid], split: Option<u8>) -> Vec<Incident> {
    let hit = |g: &TargetGrid, i: usize| {
        g.buckets
            .get(i)
            .and_then(|b| b.target.as_ref())
            .filter(|p| p.loss_pct >= INCIDENT_MIN_PCT)
            .map(|p| p.loss_pct)
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < ts.len() {
        if !grids.iter().any(|g| hit(g, i).is_some()) {
            i += 1;
            continue;
        }
        let first = i;
        while i < ts.len() && grids.iter().any(|g| hit(g, i).is_some()) {
            i += 1;
        }
        let run = first..i;
        let mut peak = 0.0_f64;
        let mut targets_hit = 0;
        // ttl -> (lost-probe weight, addr of the largest single contributor)
        let mut origin: BTreeMap<u8, (f64, f64, Option<String>)> = BTreeMap::new();
        for g in grids {
            let mut any = false;
            for j in run.clone() {
                if let Some(l) = hit(g, j) {
                    any = true;
                    peak = peak.max(l);
                }
                let b = &g.buckets[j];
                let w = weight(b);
                for h in &b.hops {
                    let v = h.added_loss_pct * w;
                    let e = origin.entry(h.ttl).or_insert((0.0, 0.0, None));
                    e.0 += v;
                    if v > e.1 {
                        e.1 = v;
                        e.2 = h.addr.clone();
                    }
                }
            }
            targets_hit += any as usize;
        }
        let top = origin
            .into_iter()
            .filter(|(_, (v, _, _))| *v > 0.0)
            .max_by(|a, b| a.1.0.total_cmp(&b.1.0));
        out.push(Incident {
            start: ts[first],
            end: ts[i - 1] + step,
            peak_loss_pct: peak,
            targets_hit,
            origin_ttl: top.as_ref().map(|(t, _)| *t),
            origin_addr: top.as_ref().and_then(|(_, (_, _, a))| a.clone()),
            segment: top.as_ref().map(|(t, _)| segment_name(*t, split)),
        });
    }
    out.reverse();
    out.truncate(MAX_INCIDENTS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{HopBucket, analyze, target_point};
    use crate::store::TargetBucket;

    /// A path of hops (ttl from 1) given as (addr, recv of 100 sent). The last
    /// hop is the target.
    fn bucket(path: &[(&str, u32)]) -> GridBucket {
        let rows: Vec<HopBucket> = path
            .iter()
            .enumerate()
            .map(|(i, (addr, recv))| HopBucket {
                ts: 0,
                ttl: i as u8 + 1,
                sent: 100,
                recv: *recv,
                rtt_sum_us: *recv as u64 * 10_000,
                rtt_min_us: Some(5_000),
                rtt_max_us: Some(20_000),
                addr: Some(addr.to_string()),
                reached: i + 1 == path.len(),
            })
            .collect();
        let last = *path.last().unwrap();
        GridBucket {
            target: target_point(&TargetBucket {
                ts: 0,
                sent: 100,
                recv: last.1,
                rtt_sum_us: last.1 as u64 * 10_000,
                rtt_max_us: Some(20_000),
            }),
            hops: analyze(&rows),
        }
    }

    fn gap() -> GridBucket {
        GridBucket::default()
    }

    fn grid(id: i64, buckets: Vec<GridBucket>) -> TargetGrid {
        TargetGrid {
            target_id: id,
            buckets,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn seg<'a>(r: &'a WhereReport, name: &str) -> &'a Segment {
        r.segments.iter().find(|s| s.name == name).unwrap()
    }

    /// Two targets share hops 1-3 (hop 2 differs by ECMP), then split.
    fn two_paths(gw_recv: u32, hop3_recv: u32) -> Vec<TargetGrid> {
        vec![
            grid(
                1,
                vec![bucket(&[
                    ("192.168.1.1", gw_recv),
                    ("10.0.2.1", gw_recv),
                    ("10.0.3.1", hop3_recv),
                    ("10.1.4.1", hop3_recv),
                    ("1.1.1.1", hop3_recv),
                ])],
            ),
            grid(
                2,
                vec![bucket(&[
                    ("192.168.1.1", gw_recv),
                    ("10.0.2.2", gw_recv),
                    ("10.0.3.1", hop3_recv),
                    ("10.2.4.1", hop3_recv),
                    ("8.8.8.8", hop3_recv),
                ])],
            ),
        ]
    }

    #[test]
    fn split_is_last_hop_every_path_shares() {
        let r = report(&[0], 60, &two_paths(100, 100));
        assert_eq!(r.split_ttl, Some(3));
        assert_eq!(r.targets_total, 2);
        assert_eq!(
            (seg(&r, "upstream").first_ttl, seg(&r, "upstream").last_ttl),
            (Some(2), Some(3))
        );
        assert_eq!(
            (
                seg(&r, "past_split").first_ttl,
                seg(&r, "past_split").last_ttl
            ),
            (Some(4), None)
        );
        assert_eq!(r.hops[2].addr.as_deref(), Some("10.0.3.1"));
        assert_eq!(r.hops[1].addr, None, "ECMP hop has no common addr");
    }

    #[test]
    fn loss_is_split_by_where_it_starts() {
        // 2% starts at the gateway, 3% more at hop 3 (shared upstream).
        let r = report(&[0], 60, &two_paths(98, 95));
        assert!(close(r.loss_pct, 5.0), "{}", r.loss_pct);
        assert!(close(seg(&r, "gateway").loss_pct, 2.0));
        assert!(close(seg(&r, "upstream").loss_pct, 3.0));
        assert!(close(seg(&r, "past_split").loss_pct, 0.0));
        assert!(close(seg(&r, "gateway").share_pct.unwrap(), 40.0));
        assert!(close(r.hops[2].share_pct.unwrap(), 60.0));
    }

    #[test]
    fn no_loss_has_no_shares() {
        let r = report(&[0], 60, &two_paths(100, 100));
        assert_eq!(r.loss_pct, 0.0);
        assert!(r.segments.iter().all(|s| s.share_pct.is_none()));
        assert!(r.incidents.is_empty());
    }

    #[test]
    fn single_target_has_no_split() {
        let g = vec![grid(
            1,
            vec![bucket(&[
                ("192.168.1.1", 100),
                ("10.0.2.1", 90),
                ("1.1.1.1", 90),
            ])],
        )];
        let r = report(&[0], 60, &g);
        assert_eq!(r.split_ttl, None);
        let up = seg(&r, "upstream");
        assert_eq!((up.first_ttl, up.last_ttl), (Some(2), None));
        assert!(close(up.loss_pct, 10.0));
        assert!(close(seg(&r, "past_split").loss_pct, 0.0));
        assert_eq!(seg(&r, "past_split").first_ttl, None);
    }

    #[test]
    fn no_data_is_plain_zero() {
        let r = report(&[0], 60, &[grid(1, vec![gap()])]);
        assert!(
            r.loss_pct == 0.0 && r.loss_pct.is_sign_positive(),
            "{}",
            r.loss_pct
        );
        assert!(r.segments.iter().all(|s| s.loss_pct.is_sign_positive()));
        assert!(r.hops.is_empty());
    }

    #[test]
    fn targets_count_equally_and_gaps_are_ignored() {
        // Target 1: 4% at the gateway in one bucket, one gap. Target 2: no loss.
        let gw = |recv| bucket(&[("192.168.1.1", recv), ("1.1.1.1", recv)]);
        let g = vec![
            grid(1, vec![gw(96), gap()]),
            grid(
                2,
                vec![
                    bucket(&[("192.168.1.1", 100), ("8.8.8.8", 100)]),
                    bucket(&[("192.168.1.1", 100), ("8.8.8.8", 100)]),
                ],
            ),
        ];
        let r = report(&[0, 60], 60, &g);
        assert!(close(seg(&r, "gateway").loss_pct, 2.0), "{r:?}");
    }

    #[test]
    fn incidents_are_runs_of_lossy_buckets_newest_first() {
        let ok = || bucket(&[("192.168.1.1", 100), ("10.0.2.1", 100), ("1.1.1.1", 100)]);
        let gw_loss = || bucket(&[("192.168.1.1", 90), ("10.0.2.1", 90), ("1.1.1.1", 90)]);
        let up_loss = || bucket(&[("192.168.1.1", 100), ("10.0.2.1", 80), ("1.1.1.1", 80)]);
        let g = vec![grid(
            1,
            vec![
                ok(),
                gw_loss(),
                gw_loss(),
                gap(),
                up_loss(),
                ok(),
                up_loss(),
            ],
        )];
        let ts: Vec<i64> = (0..7).map(|i| 1000 + i * 60).collect();
        let r = report(&ts, 60, &g);
        assert_eq!(r.incidents.len(), 3, "{:?}", r.incidents);
        let newest = &r.incidents[0];
        assert_eq!((newest.start, newest.end), (1360, 1420));
        assert_eq!(newest.origin_ttl, Some(2));
        assert_eq!(newest.origin_addr.as_deref(), Some("10.0.2.1"));
        assert_eq!(newest.segment, Some("upstream"));
        assert!(close(newest.peak_loss_pct, 20.0));
        let older = &r.incidents[1];
        assert_eq!((older.start, older.end), (1240, 1300));
        let oldest = &r.incidents[2];
        assert_eq!((oldest.start, oldest.end), (1060, 1180));
        assert_eq!(oldest.origin_ttl, Some(1));
        assert_eq!(oldest.segment, Some("gateway"));
        assert_eq!(oldest.targets_hit, 1);
    }

    #[test]
    fn incident_counts_each_target_hit() {
        let r = report(&[0], 60, &two_paths(90, 90));
        assert_eq!(r.incidents.len(), 1);
        assert_eq!(r.incidents[0].targets_hit, 2);
    }
}
