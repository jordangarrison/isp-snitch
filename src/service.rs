//! Wiring for `isp-snitch run`: tracer threads, the minute aggregator,
//! hourly maintenance, and the HTTP server.

use crate::aggregate::{HopMinute, MinuteAggregator};
use crate::config::Config;
use crate::icmp::Transport;
use crate::icmp::socket::PingSocket;
use crate::store::Store;
use crate::tracer::runner::{self, RunOpts};
use crate::tracer::{CompletedRound, TracerCore};
use crate::web::{self, AppState};
use crate::wide::{Event, Level};
use anyhow::{Context, Result};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

/// Seconds after each minute boundary at which the previous minute is written,
/// so rounds that started late in that minute have completed.
const FLUSH_DELAY_SECS: i64 = 10;
const MAINTENANCE_EVERY: Duration = Duration::from_secs(3600);

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Time until `FLUSH_DELAY_SECS` past the next minute boundary.
fn until_next_flush() -> Duration {
    let now = unix_now();
    let next = (now - FLUSH_DELAY_SECS).div_euclid(60) * 60 + 60 + FLUSH_DELAY_SECS;
    Duration::from_secs((next - now).max(1) as u64)
}

/// Runs until SIGINT or SIGTERM.
pub async fn run(cfg: Config) -> Result<()> {
    run_until(cfg, shutdown_signal()).await
}

/// Runs until `shutdown` resolves, then flushes buffered data and returns.
pub async fn run_until(
    cfg: Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let store = Arc::new(Store::open(&cfg.db_path)?);
    let mut ids = Vec::with_capacity(cfg.targets.len());
    for t in &cfg.targets {
        ids.push((store.ensure_target(&t.name, t.addr)?, t.addr));
    }
    let targets: Vec<_> = store
        .targets()?
        .into_iter()
        .filter(|t| ids.iter().any(|(id, _)| *id == t.id))
        .collect();

    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("binding HTTP listener on {}", cfg.listen))?;
    let local = listener.local_addr()?;

    Event::new("startup")
        .set("interface", &cfg.interface)
        .set("listen", local.to_string())
        .set("db_path", cfg.db_path.display().to_string())
        .set(
            "targets",
            targets
                .iter()
                .map(|t| format!("{} {}", t.name, t.addr))
                .collect::<Vec<_>>(),
        )
        .emit();

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::unbounded_channel::<CompletedRound>();
    let mut threads = Vec::new();
    for (id, addr) in ids {
        let tx = tx.clone();
        let stop = stop.clone();
        let iface = cfg.interface.clone();
        let p = cfg.probe.clone();
        let handle = std::thread::Builder::new()
            .name(format!("trace-{addr}"))
            .spawn(move || {
                runner::run(
                    || {
                        PingSocket::open(addr, iface.as_deref())
                            .map(|s| Box::new(s) as Box<dyn Transport>)
                    },
                    TracerCore::new(id, p.max_ttl, Duration::from_millis(p.timeout_ms)),
                    RunOpts::new(
                        Duration::from_millis(p.interval_ms),
                        Duration::from_millis(p.probe_spacing_ms),
                    ),
                    &stop,
                    |r| {
                        let _ = tx.send(r);
                    },
                )
            })
            .context("spawning tracer thread")?;
        threads.push(handle);
    }
    drop(tx);

    let aggregator = tokio::spawn(aggregate_loop(
        rx,
        store.clone(),
        targets.len(),
        until_next_flush,
    ));
    let maintenance = tokio::spawn(maintenance_loop(store.clone(), cfg.retention.clone()));

    let app = web::router(
        AppState::new(store.clone(), targets, cfg.retention.minute_days)
            .with_interface(cfg.interface.clone()),
    )
    .layer(axum::middleware::from_fn(web::log::http_wide_event));
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .context("HTTP server")?;

    stop.store(true, Ordering::Relaxed);
    for t in threads {
        let _ = tokio::task::spawn_blocking(move || t.join()).await;
    }
    maintenance.abort();
    aggregator.await.context("aggregator task")??;
    Event::new("shutdown").emit();
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

async fn write_rows(
    store: &Arc<Store>,
    rows: Vec<HopMinute>,
    targets: usize,
    reason: &str,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let started = Instant::now();
    let sent: u64 = rows.iter().map(|r| r.sent as u64).sum();
    let recv: u64 = rows.iter().map(|r| r.recv as u64).sum();
    let send_errors: u64 = rows.iter().map(|r| r.send_errors as u64).sum();
    let n = rows.len();
    let st = store.clone();
    let res = tokio::task::spawn_blocking(move || st.insert_minutes(&rows)).await?;
    let mut ev = Event::new("probe_flush");
    ev.set("reason", reason)
        .set("rows", n)
        .set("targets", targets)
        .set("sent", sent)
        .set("lost_probes", sent - recv)
        .set("send_errors", send_errors)
        .set("duration_ms", started.elapsed().as_millis() as u64);
    if let Err(e) = &res {
        ev.level(Level::Error)
            .set("error", json_error(e))
            .set("why", "minute rows could not be written; they are dropped")
            .set("fix", "check disk space and permissions on db_path");
    }
    ev.emit();
    res
}

fn json_error(e: &anyhow::Error) -> serde_json::Value {
    serde_json::json!({ "message": format!("{e:#}") })
}

async fn aggregate_loop(
    mut rx: mpsc::UnboundedReceiver<CompletedRound>,
    store: Arc<Store>,
    targets: usize,
    next_flush_in: impl Fn() -> Duration,
) -> Result<()> {
    let mut agg = MinuteAggregator::new();
    // One timer for the whole loop: incoming rounds must not restart it.
    let timer = tokio::time::sleep(next_flush_in());
    tokio::pin!(timer);
    loop {
        tokio::select! {
            r = rx.recv() => match r {
                Some(r) => agg.add(&r),
                None => break,
            },
            _ = &mut timer => {
                let minute = unix_now().div_euclid(60) * 60;
                // A failed write is logged in the flush event; keep running.
                let _ = write_rows(&store, agg.drain_before(minute), targets, "minute").await;
                timer.as_mut().reset(tokio::time::Instant::now() + next_flush_in());
            }
        }
    }
    write_rows(&store, agg.drain_all(), targets, "shutdown").await
}

async fn maintenance_loop(store: Arc<Store>, retention: crate::config::RetentionConfig) {
    let mut tick = tokio::time::interval(MAINTENANCE_EVERY);
    loop {
        tick.tick().await;
        let started = Instant::now();
        let st = store.clone();
        let r = retention.clone();
        let res = tokio::task::spawn_blocking(move || -> Result<(usize, usize, usize)> {
            let now = unix_now();
            let hours = st.rollup_hours(now)?;
            let (m, h) = st.prune(now, r.minute_days, r.hour_days)?;
            Ok((hours, m, h))
        })
        .await;
        let mut ev = Event::new("maintenance");
        ev.set("duration_ms", started.elapsed().as_millis() as u64);
        match res {
            Ok(Ok((hours, m, h))) => {
                ev.set("hour_rows_written", hours)
                    .set("minute_rows_pruned", m)
                    .set("hour_rows_pruned", h);
            }
            Ok(Err(e)) => {
                ev.level(Level::Error).set("error", json_error(&e));
            }
            Err(e) => {
                ev.level(Level::Error)
                    .set("error", serde_json::json!({ "message": e.to_string() }));
            }
        }
        ev.emit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Table;
    use crate::tracer::ProbeOutcome;

    #[tokio::test]
    async fn flush_fires_while_rounds_keep_arriving() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let id = store
            .ensure_target("t", "1.1.1.1".parse().unwrap())
            .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let agg = tokio::spawn(aggregate_loop(rx, store.clone(), 1, || {
            Duration::from_millis(100)
        }));
        // Rounds every 10ms, far faster than the 100ms flush period.
        for _ in 0..50 {
            tx.send(CompletedRound {
                target_id: id,
                started_unix: 0,
                probes: vec![ProbeOutcome {
                    ttl: 1,
                    rtt: Some(Duration::from_millis(1)),
                    responder: Some("10.0.0.1".parse().unwrap()),
                    reached: true,
                    send_error: false,
                }],
            })
            .unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let rows = store.hop_buckets(Table::Minute, id, 0, 60, 60).unwrap();
        assert!(
            !rows.is_empty(),
            "no flush happened while rounds were arriving"
        );
        drop(tx);
        agg.await.unwrap().unwrap();
        let rows = store.hop_buckets(Table::Minute, id, 0, 60, 60).unwrap();
        assert_eq!(rows[0].sent, 50);
    }
}
