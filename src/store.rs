//! SQLite storage: per-minute hop rows, hourly rollups, retention.

pub use crate::aggregate::HopMinute;
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Mutex;

/// Forward-only migrations; index + 1 is the `PRAGMA user_version` after it.
const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE targets (
    id   INTEGER PRIMARY KEY,
    addr TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL
);
CREATE TABLE hop_minute (
    target_id     INTEGER NOT NULL REFERENCES targets(id),
    ts            INTEGER NOT NULL,
    ttl           INTEGER NOT NULL,
    sent          INTEGER NOT NULL,
    recv          INTEGER NOT NULL,
    rtt_sum_us    INTEGER NOT NULL,
    rtt_sumsq_us2 REAL    NOT NULL,
    rtt_min_us    INTEGER,
    rtt_max_us    INTEGER,
    addr          TEXT,
    addr_count    INTEGER NOT NULL,
    reached       INTEGER NOT NULL,
    send_errors   INTEGER NOT NULL,
    PRIMARY KEY (target_id, ts, ttl)
) WITHOUT ROWID;
CREATE TABLE hop_hour (
    target_id     INTEGER NOT NULL REFERENCES targets(id),
    ts            INTEGER NOT NULL,
    ttl           INTEGER NOT NULL,
    sent          INTEGER NOT NULL,
    recv          INTEGER NOT NULL,
    rtt_sum_us    INTEGER NOT NULL,
    rtt_sumsq_us2 REAL    NOT NULL,
    rtt_min_us    INTEGER,
    rtt_max_us    INTEGER,
    addr          TEXT,
    addr_count    INTEGER NOT NULL,
    reached       INTEGER NOT NULL,
    send_errors   INTEGER NOT NULL,
    PRIMARY KEY (target_id, ts, ttl)
) WITHOUT ROWID;
"#];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Minute,
    Hour,
}

/// Rows of one target (`?1`) for a bucket query. The hour table is topped up
/// with minute rows newer than its last rolled-up hour, so recent data show
/// on long ranges before the next rollup.
fn source_sql(table: Table) -> String {
    match table {
        Table::Minute => "SELECT * FROM hop_minute WHERE target_id = ?1".to_string(),
        Table::Hour => "SELECT * FROM hop_hour WHERE target_id = ?1
               AND ts < (SELECT coalesce(max(ts) + 3600, 0) FROM hop_hour WHERE target_id = ?1)
             UNION ALL
             SELECT * FROM hop_minute WHERE target_id = ?1
               AND ts >= (SELECT coalesce(max(ts) + 3600, 0) FROM hop_hour WHERE target_id = ?1)"
            .to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetRow {
    pub id: i64,
    pub name: String,
    pub addr: Ipv4Addr,
}

/// Hop stats for one (bucket, ttl), summed over the bucket's rows.
#[derive(Debug, Clone, PartialEq)]
pub struct HopBucket {
    pub ts: i64,
    pub ttl: u8,
    pub sent: u32,
    pub recv: u32,
    pub rtt_sum_us: u64,
    pub rtt_min_us: Option<u32>,
    pub rtt_max_us: Option<u32>,
    /// Responder of the row with the most answers in the bucket.
    pub addr: Option<String>,
    pub reached: bool,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating database directory {}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            conn.execute_batch(&format!(
                "BEGIN; {sql}; PRAGMA user_version = {}; COMMIT;",
                i + 1
            ))
            .with_context(|| format!("applying migration {}", i + 1))?;
        }
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Inserts the target or renames it; returns its id.
    pub fn ensure_target(&self, name: &str, addr: Ipv4Addr) -> Result<i64> {
        let c = self.conn();
        c.query_row(
            "INSERT INTO targets (addr, name) VALUES (?1, ?2)
             ON CONFLICT(addr) DO UPDATE SET name = excluded.name
             RETURNING id",
            params![addr.to_string(), name],
            |r| r.get(0),
        )
        .context("saving target")
    }

    pub fn targets(&self) -> Result<Vec<TargetRow>> {
        let c = self.conn();
        let mut st = c.prepare("SELECT id, name, addr FROM targets ORDER BY id")?;
        let rows = st.query_map([], |r| {
            let addr: String = r.get(2)?;
            Ok(TargetRow {
                id: r.get(0)?,
                name: r.get(1)?,
                addr: addr.parse().unwrap_or(Ipv4Addr::UNSPECIFIED),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Additive upsert: a late row for an existing minute adds to it.
    pub fn insert_minutes(&self, rows: &[HopMinute]) -> Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO hop_minute (target_id, ts, ttl, sent, recv, rtt_sum_us, rtt_sumsq_us2,
                    rtt_min_us, rtt_max_us, addr, addr_count, reached, send_errors)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(target_id, ts, ttl) DO UPDATE SET
                    sent = sent + excluded.sent,
                    recv = recv + excluded.recv,
                    rtt_sum_us = rtt_sum_us + excluded.rtt_sum_us,
                    rtt_sumsq_us2 = rtt_sumsq_us2 + excluded.rtt_sumsq_us2,
                    rtt_min_us = min(coalesce(rtt_min_us, excluded.rtt_min_us),
                                     coalesce(excluded.rtt_min_us, rtt_min_us)),
                    rtt_max_us = max(coalesce(rtt_max_us, excluded.rtt_max_us),
                                     coalesce(excluded.rtt_max_us, rtt_max_us)),
                    addr = coalesce(addr, excluded.addr),
                    addr_count = max(addr_count, excluded.addr_count),
                    reached = max(reached, excluded.reached),
                    send_errors = send_errors + excluded.send_errors",
            )?;
            for r in rows {
                st.execute(params![
                    r.target_id,
                    r.ts,
                    r.ttl,
                    r.sent,
                    r.recv,
                    r.rtt_sum_us as i64,
                    r.rtt_sumsq_us2,
                    r.rtt_min_us,
                    r.rtt_max_us,
                    r.addr,
                    r.addr_count,
                    r.reached,
                    r.send_errors
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Recomputes hourly rows for every completed hour since the latest
    /// rollup (minus one hour, to pick up late minutes). Returns rows written.
    pub fn rollup_hours(&self, now: i64) -> Result<usize> {
        let c = self.conn();
        let end = now.div_euclid(3600) * 3600;
        let start: Option<i64> = c.query_row(
            "SELECT coalesce((SELECT max(ts) - 3600 FROM hop_hour), (SELECT min(ts) FROM hop_minute))",
            [],
            |r| r.get(0),
        )?;
        let Some(start) = start else { return Ok(0) };
        let start = start.div_euclid(3600) * 3600;
        if start >= end {
            return Ok(0);
        }
        let n = c.execute(
            "INSERT OR REPLACE INTO hop_hour
             WITH m AS MATERIALIZED (
                SELECT *, (ts / 3600) * 3600 AS h FROM hop_minute WHERE ts >= ?1 AND ts < ?2
             ), agg AS MATERIALIZED (
                SELECT target_id, h, ttl, sum(sent) AS sent, sum(recv) AS recv,
                       sum(rtt_sum_us) AS rs, sum(rtt_sumsq_us2) AS rq,
                       min(rtt_min_us) AS mn, max(rtt_max_us) AS mx,
                       max(addr_count) AS ac, max(reached) AS r, sum(send_errors) AS se
                FROM m GROUP BY target_id, h, ttl
             ), best AS MATERIALIZED (
                SELECT target_id, h, ttl, addr FROM (
                    SELECT target_id, h, ttl, addr,
                           row_number() OVER (PARTITION BY target_id, h, ttl ORDER BY recv DESC, ts DESC) AS rn
                    FROM m WHERE addr IS NOT NULL
                ) WHERE rn = 1
             )
             SELECT agg.target_id, agg.h, agg.ttl, sent, recv, rs, rq, mn, mx, best.addr, ac, r, se
             FROM agg LEFT JOIN best USING (target_id, h, ttl)",
            params![start, end],
        )?;
        Ok(n)
    }

    /// Deletes rows past retention. Returns (minute rows, hour rows) deleted.
    pub fn prune(&self, now: i64, minute_days: u32, hour_days: u32) -> Result<(usize, usize)> {
        let c = self.conn();
        let m = c.execute(
            "DELETE FROM hop_minute WHERE ts < ?1",
            [now - minute_days as i64 * 86_400],
        )?;
        let h = c.execute(
            "DELETE FROM hop_hour WHERE ts < ?1",
            [now - hour_days as i64 * 86_400],
        )?;
        Ok((m, h))
    }

    /// Hop stats grouped into `step`-second buckets over `[from, to)`,
    /// ordered by (bucket, ttl).
    pub fn hop_buckets(
        &self,
        table: Table,
        target_id: i64,
        from: i64,
        to: i64,
        step: i64,
    ) -> Result<Vec<HopBucket>> {
        let c = self.conn();
        let sql = format!(
            "WITH m AS MATERIALIZED (
                SELECT (ts / ?4) * ?4 AS b, ts, ttl, sent, recv, rtt_sum_us, rtt_min_us,
                       rtt_max_us, addr, reached
                FROM ({src}) WHERE ts >= ?2 AND ts < ?3
             ), agg AS MATERIALIZED (
                SELECT b, ttl, sum(sent) AS sent, sum(recv) AS recv, sum(rtt_sum_us) AS rs,
                       min(rtt_min_us) AS mn, max(rtt_max_us) AS mx, max(reached) AS r
                FROM m GROUP BY b, ttl
             ), best AS MATERIALIZED (
                SELECT b, ttl, addr FROM (
                    SELECT b, ttl, addr,
                           row_number() OVER (PARTITION BY b, ttl ORDER BY recv DESC, ts DESC) AS rn
                    FROM m WHERE addr IS NOT NULL
                ) WHERE rn = 1
             )
             SELECT agg.b, agg.ttl, sent, recv, rs, mn, mx, best.addr, r
             FROM agg LEFT JOIN best USING (b, ttl) ORDER BY agg.b, agg.ttl",
            src = source_sql(table)
        );
        let mut st = c.prepare_cached(&sql)?;
        let rows = st.query_map(params![target_id, from, to, step], |r| {
            Ok(HopBucket {
                ts: r.get(0)?,
                ttl: r.get(1)?,
                sent: r.get(2)?,
                recv: r.get(3)?,
                rtt_sum_us: r.get::<_, i64>(4)? as u64,
                rtt_min_us: r.get(5)?,
                rtt_max_us: r.get(6)?,
                addr: r.get(7)?,
                reached: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(target_id: i64, ts: i64, ttl: u8, sent: u32, recv: u32) -> HopMinute {
        HopMinute {
            target_id,
            ts,
            ttl,
            sent,
            recv,
            rtt_sum_us: recv as u64 * 1000,
            rtt_sumsq_us2: recv as f64 * 1e6,
            rtt_min_us: (recv > 0).then_some(900),
            rtt_max_us: (recv > 0).then_some(1100),
            addr: (recv > 0).then(|| format!("10.0.0.{ttl}")),
            addr_count: (recv > 0) as u32,
            reached: false,
            send_errors: 0,
        }
    }

    fn store_with_target() -> (Store, i64) {
        let s = Store::open_in_memory().unwrap();
        let id = s
            .ensure_target("Cloudflare", "1.1.1.1".parse().unwrap())
            .unwrap();
        (s, id)
    }

    #[test]
    fn open_creates_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/isp-snitch.db");
        Store::open(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn migrate_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.db");
        let id = {
            let s = Store::open(&path).unwrap();
            s.ensure_target("A", "1.1.1.1".parse().unwrap()).unwrap()
        };
        let s = Store::open(&path).unwrap();
        assert_eq!(s.targets().unwrap()[0].id, id);
    }

    #[test]
    fn ensure_target_updates_name() {
        let (s, id) = store_with_target();
        let id2 = s.ensure_target("CF", "1.1.1.1".parse().unwrap()).unwrap();
        assert_eq!(id, id2);
        let t = s.targets().unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].name, "CF");
        assert_eq!(t[0].addr, "1.1.1.1".parse::<std::net::Ipv4Addr>().unwrap());
    }

    #[test]
    fn insert_twice_adds_counts() {
        let (s, id) = store_with_target();
        s.insert_minutes(&[row(id, 60, 1, 10, 9)]).unwrap();
        let mut late = row(id, 60, 1, 2, 0);
        late.send_errors = 2;
        s.insert_minutes(&[late]).unwrap();
        let b = s.hop_buckets(Table::Minute, id, 0, 120, 60).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].sent, b[0].recv), (12, 9));
        assert_eq!(b[0].rtt_min_us, Some(900));
        assert_eq!(b[0].addr.as_deref(), Some("10.0.0.1"));
    }

    #[test]
    fn rollup_sums_minutes_into_hour() {
        let (s, id) = store_with_target();
        let rows: Vec<HopMinute> = (0..60).map(|m| row(id, 3600 + m * 60, 1, 10, 9)).collect();
        s.insert_minutes(&rows).unwrap();
        // Hour 3600..7200 is complete once now >= 7200.
        assert_eq!(s.rollup_hours(7199).unwrap(), 0);
        assert_eq!(s.rollup_hours(7200).unwrap(), 1);
        let h = s.hop_buckets(Table::Hour, id, 0, 10_000, 3600).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].ts, h[0].sent, h[0].recv), (3600, 600, 540));
        // Re-running recomputes instead of doubling.
        s.rollup_hours(7200).unwrap();
        let h = s.hop_buckets(Table::Hour, id, 0, 10_000, 3600).unwrap();
        assert_eq!(h[0].sent, 600);
    }

    #[test]
    fn prune_deletes_old_rows() {
        let (s, id) = store_with_target();
        let day = 86_400;
        s.insert_minutes(&[row(id, 0, 1, 1, 1), row(id, 10 * day, 1, 1, 1)])
            .unwrap();
        s.rollup_hours(11 * day).unwrap();
        let (m, h) = s.prune(11 * day, 5, 20).unwrap();
        assert_eq!((m, h), (1, 0));
        let (m, h) = s.prune(30 * day, 5, 20).unwrap();
        assert_eq!((m, h), (1, 1));
    }

    #[test]
    fn hop_buckets_groups_by_step() {
        let (s, id) = store_with_target();
        let mut rows = Vec::new();
        for m in 0..10 {
            rows.push(row(id, m * 60, 1, 10, 10));
            rows.push(row(id, m * 60, 2, 10, 5));
        }
        s.insert_minutes(&rows).unwrap();
        let b = s.hop_buckets(Table::Minute, id, 0, 600, 300).unwrap();
        let keys: Vec<(i64, u8, u32, u32)> =
            b.iter().map(|x| (x.ts, x.ttl, x.sent, x.recv)).collect();
        assert_eq!(
            keys,
            vec![
                (0, 1, 50, 50),
                (0, 2, 50, 25),
                (300, 1, 50, 50),
                (300, 2, 50, 25)
            ]
        );
    }

    #[test]
    fn hop_buckets_respects_range_and_target() {
        let (s, id) = store_with_target();
        let other = s
            .ensure_target("Google", "8.8.8.8".parse().unwrap())
            .unwrap();
        s.insert_minutes(&[
            row(id, 0, 1, 1, 1),
            row(id, 600, 1, 1, 1),
            row(other, 0, 1, 1, 1),
        ])
        .unwrap();
        let b = s.hop_buckets(Table::Minute, id, 0, 600, 60).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].ts, 0);
    }

    /// One day of data for 3 targets x 12 hops: a 24h bucket query must stay fast.
    #[test]
    fn hop_buckets_day_of_data_is_fast() {
        let s = Store::open_in_memory().unwrap();
        let ids: Vec<i64> = ["1.1.1.1", "8.8.8.8", "9.9.9.9"]
            .iter()
            .map(|a| s.ensure_target(a, a.parse().unwrap()).unwrap())
            .collect();
        let mut rows = Vec::new();
        for &id in &ids {
            for m in 0..1440 {
                for ttl in 1..=12 {
                    rows.push(row(id, m * 60, ttl, 60, 59));
                }
            }
        }
        s.insert_minutes(&rows).unwrap();
        let started = std::time::Instant::now();
        let b = s
            .hop_buckets(Table::Minute, ids[0], 0, 86_400, 300)
            .unwrap();
        let took = started.elapsed();
        assert_eq!(b.len(), 288 * 12);
        assert!(took.as_secs_f64() < 2.0, "24h hop_buckets took {took:?}");
        let started = std::time::Instant::now();
        s.rollup_hours(86_400).unwrap();
        assert!(
            started.elapsed().as_secs_f64() < 5.0,
            "first rollup took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn hour_queries_include_minutes_not_rolled_up_yet() {
        let (s, id) = store_with_target();
        let rows: Vec<HopMinute> = (0..90).map(|m| row(id, m * 60, 1, 10, 9)).collect();
        s.insert_minutes(&rows).unwrap();
        // Only hour 0 is complete and rolled up; minutes 60..89 are the current hour.
        s.rollup_hours(3600 + 1800).unwrap();
        let b = s.hop_buckets(Table::Hour, id, 0, 7200, 3600).unwrap();
        let got: Vec<(i64, u32)> = b.iter().map(|x| (x.ts, x.sent)).collect();
        assert_eq!(got, vec![(0, 600), (3600, 300)]);
    }
}
