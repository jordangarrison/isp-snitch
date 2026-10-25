//! Live test against the loopback interface: no network and no privileges
//! needed, only `net.ipv4.ping_group_range` covering the test user.

use isp_snitch::icmp::socket::PingSocket;
use isp_snitch::icmp::{Transport, TransportEvent};
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

fn open_lo() -> Option<PingSocket> {
    match PingSocket::open(Ipv4Addr::LOCALHOST, Some("lo")) {
        Ok(s) => Some(s),
        Err(e) if matches!(e.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) => {
            eprintln!("skipped: ping sockets not allowed for this user ({e})");
            None
        }
        Err(e) => panic!("open failed: {e}"),
    }
}

#[test]
fn loopback_echo_reached_at_ttl_1() {
    let Some(mut sock) = open_lo() else { return };
    sock.send(7, 1).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if let Some((ev, _at)) = sock.recv(Duration::from_millis(200)).unwrap() {
            assert_eq!(
                ev,
                TransportEvent::EchoReply {
                    seq: 7,
                    from: Ipv4Addr::LOCALHOST
                }
            );
            return;
        }
    }
    panic!("no echo reply from 127.0.0.1 within 1s");
}

#[test]
fn unknown_interface_fails_to_open() {
    let err = PingSocket::open(Ipv4Addr::LOCALHOST, Some("nonexistent0")).unwrap_err();
    if matches!(err.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) {
        return;
    }
    assert_eq!(err.raw_os_error(), Some(libc::ENODEV), "{err}");
}
