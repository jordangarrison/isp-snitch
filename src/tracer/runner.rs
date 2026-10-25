//! Blocking run loop for one target: drives a `Transport` with a `TracerCore`
//! and hands each completed round to a sink. Runs on its own OS thread.

use super::{CompletedRound, TracerCore};
use crate::icmp::Transport;
use crate::wide::{Event, Level};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct RunOpts {
    pub interval: Duration,
    pub spacing: Duration,
    /// How often to retry opening the transport after it failed.
    pub reopen_every: Duration,
    /// Stop after this many rounds have completed (`None` = run until stopped).
    pub max_rounds: Option<usize>,
}

impl RunOpts {
    pub fn new(interval: Duration, spacing: Duration) -> Self {
        Self {
            interval,
            spacing,
            reopen_every: Duration::from_secs(5),
            max_rounds: None,
        }
    }
}

/// Send errors that mean the socket itself is unusable (device gone or
/// renamed), so the transport is reopened instead of reused.
fn is_fatal_send_error(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENODEV | libc::ENXIO | libc::EBADF)
    )
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Reads answers into `core` until `until` or the transport fails.
fn drain(t: &mut dyn Transport, core: &mut TracerCore, until: Instant) -> io::Result<()> {
    loop {
        let now = Instant::now();
        if now >= until {
            return Ok(());
        }
        match t.recv(until - now)? {
            Some((ev, at)) => core.on_event(ev, at),
            None => return Ok(()),
        }
    }
}

pub fn run<F, S>(
    mut factory: F,
    mut core: TracerCore,
    opts: RunOpts,
    stop: &AtomicBool,
    mut sink: S,
) where
    F: FnMut() -> io::Result<Box<dyn Transport>>,
    S: FnMut(CompletedRound),
{
    let mut transport: Option<Box<dyn Transport>> = None;
    let mut last_open: Option<Instant> = None;
    let mut transport_down_logged = false;
    let mut next_round = Instant::now();
    let mut started = 0usize;
    let max = opts.max_rounds.unwrap_or(usize::MAX);
    let tick = Duration::from_millis(100);

    while !stop.load(Ordering::Relaxed) {
        if started >= max && core.is_idle() {
            break;
        }
        let now = Instant::now();

        if transport.is_none() && last_open.is_none_or(|t| now >= t + opts.reopen_every) {
            last_open = Some(now);
            match factory() {
                Ok(t) => {
                    if transport_down_logged {
                        Event::new("transport_recovered").emit();
                    }
                    transport_down_logged = false;
                    transport = Some(t);
                }
                Err(e) => {
                    if !transport_down_logged {
                        Event::new("transport_open_failed")
                            .level(Level::Error)
                            .set("error", e.to_string())
                            .set(
                                "why",
                                "probe socket could not be opened; rounds record send errors until it opens",
                            )
                            .emit();
                        transport_down_logged = true;
                    }
                }
            }
        }

        if now >= next_round && started < max {
            started += 1;
            next_round += opts.interval;
            if next_round + opts.interval < now {
                next_round = now + opts.interval; // resumed after a stall or suspend
            }
            match transport.as_mut() {
                None => core.begin_failed_round(now, unix_now()),
                Some(t) => {
                    let probes = core.begin_round(now, unix_now());
                    let mut fatal = false;
                    for (seq, ttl) in probes {
                        if fatal {
                            core.on_send_error(seq);
                            continue;
                        }
                        let sent_at = Instant::now();
                        match t.send(seq, ttl) {
                            Ok(()) => core.mark_sent(seq, sent_at),
                            Err(e) => {
                                core.on_send_error(seq);
                                fatal = is_fatal_send_error(&e);
                            }
                        }
                        if !fatal && !opts.spacing.is_zero() {
                            let until = Instant::now() + opts.spacing;
                            if drain(t.as_mut(), &mut core, until).is_err() {
                                fatal = true;
                            }
                        }
                    }
                    if fatal {
                        transport = None;
                    }
                }
            }
        }

        let mut until = next_round.min(Instant::now() + tick);
        if let Some(d) = core.next_deadline() {
            until = until.min(d);
        }
        match transport.as_mut() {
            Some(t) => {
                if let Err(e) = drain(t.as_mut(), &mut core, until) {
                    Event::new("transport_recv_failed")
                        .level(Level::Error)
                        .set("error", e.to_string())
                        .emit();
                    transport = None;
                }
            }
            None => std::thread::sleep(until.saturating_duration_since(Instant::now())),
        }

        for r in core.poll_completed(Instant::now()) {
            sink(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracer::fake::FakeNet;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn opts() -> RunOpts {
        RunOpts {
            interval: Duration::from_millis(20),
            spacing: Duration::from_millis(0),
            reopen_every: Duration::from_millis(30),
            max_rounds: Some(3),
        }
    }

    #[test]
    fn runner_with_fake_net_reaches_target() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut rounds = Vec::new();
        run(
            || Ok(Box::new(FakeNet::new(4)) as Box<dyn Transport>),
            TracerCore::new(1, 30, Duration::from_millis(50)),
            opts(),
            &stop,
            |r| rounds.push(r),
        );
        assert_eq!(rounds.len(), 3);
        for r in &rounds {
            let ttls: Vec<u8> = r.probes.iter().map(|p| p.ttl).collect();
            assert_eq!(ttls, vec![1, 2, 3, 4]);
            assert!(r.probes[3].reached);
        }
    }

    #[test]
    fn runner_factory_failure_records_ttl1_send_errors() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut rounds = Vec::new();
        let mut attempts = 0;
        run(
            || {
                attempts += 1;
                Err(io::Error::from_raw_os_error(libc::ENODEV))
            },
            TracerCore::new(1, 30, Duration::from_millis(50)),
            RunOpts {
                max_rounds: Some(2),
                ..opts()
            },
            &stop,
            |r| rounds.push(r),
        );
        assert_eq!(rounds.len(), 2);
        for r in &rounds {
            assert_eq!(r.probes.len(), 1);
            assert_eq!(r.probes[0].ttl, 1);
            assert!(r.probes[0].send_error);
        }
        assert!(attempts >= 1);
    }

    #[test]
    fn runner_recovers_when_factory_starts_working() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut rounds = Vec::new();
        let mut attempts = 0;
        run(
            || {
                attempts += 1;
                if attempts == 1 {
                    Err(io::Error::from_raw_os_error(libc::ENODEV))
                } else {
                    Ok(Box::new(FakeNet::new(2)) as Box<dyn Transport>)
                }
            },
            TracerCore::new(1, 30, Duration::from_millis(50)),
            RunOpts {
                max_rounds: Some(6),
                ..opts()
            },
            &stop,
            |r| rounds.push(r),
        );
        assert!(rounds[0].probes[0].send_error);
        assert!(rounds.last().unwrap().probes.last().unwrap().reached);
    }

    #[test]
    fn stop_flag_ends_run() {
        let stop = Arc::new(AtomicBool::new(true));
        let mut n = 0;
        run(
            || Ok(Box::new(FakeNet::new(2)) as Box<dyn Transport>),
            TracerCore::new(1, 30, Duration::from_millis(50)),
            RunOpts {
                max_rounds: None,
                ..opts()
            },
            &stop,
            |_| n += 1,
        );
        assert_eq!(n, 0);
    }
}
