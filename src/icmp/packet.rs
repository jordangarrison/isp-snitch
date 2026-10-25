//! ICMP echo encoding and answer parsing. Pure functions, no sockets.
//!
//! On a Linux ping socket the kernel owns the ICMP identifier and checksum:
//! it rewrites both on send and filters replies by identifier on receive.

use super::TransportEvent;
use std::net::Ipv4Addr;

/// Echo payload, so isp-snitch probes are easy to spot in a packet capture.
pub const PAYLOAD: &[u8; 8] = b"ispsnch\0";

const ICMP_ECHO_REPLY: u8 = 0;
const ICMP_DEST_UNREACH: u8 = 3;
const ICMP_ECHO_REQUEST: u8 = 8;
const ICMP_TIME_EXCEEDED: u8 = 11;
const SO_EE_ORIGIN_ICMP: u8 = 2;
/// `struct sock_extended_err` is 16 bytes; the offender `sockaddr_in` follows it.
const EXT_ERR_LEN: usize = 16;

pub fn encode_echo(seq: u16) -> [u8; 16] {
    let mut p = [0u8; 16];
    p[0] = ICMP_ECHO_REQUEST;
    p[6..8].copy_from_slice(&seq.to_be_bytes());
    p[8..].copy_from_slice(PAYLOAD);
    p
}

/// Parses a datagram read from the normal queue of a ping socket.
pub fn parse_echo_reply(buf: &[u8], from: Ipv4Addr) -> Option<TransportEvent> {
    if buf.len() < 8 || buf[0] != ICMP_ECHO_REPLY {
        return None;
    }
    let seq = u16::from_be_bytes([buf[6], buf[7]]);
    Some(TransportEvent::EchoReply { seq, from })
}

/// Parses one `MSG_ERRQUEUE` read: `ext_err` is the `IP_RECVERR` control
/// message data, `payload` is the original echo request that triggered it.
pub fn parse_errqueue(ext_err: &[u8], payload: &[u8]) -> Option<TransportEvent> {
    if ext_err.len() < EXT_ERR_LEN + 8 || payload.len() < 8 {
        return None;
    }
    let origin = ext_err[4];
    let icmp_type = ext_err[5];
    if origin != SO_EE_ORIGIN_ICMP {
        return None;
    }
    let off = EXT_ERR_LEN + 4; // skip sin_family + sin_port
    let from = Ipv4Addr::new(
        ext_err[off],
        ext_err[off + 1],
        ext_err[off + 2],
        ext_err[off + 3],
    );
    let seq = u16::from_be_bytes([payload[6], payload[7]]);
    match icmp_type {
        ICMP_TIME_EXCEEDED => Some(TransportEvent::TimeExceeded { seq, from }),
        ICMP_DEST_UNREACH => Some(TransportEvent::Unreachable { seq, from }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply_for(req: &[u8]) -> Vec<u8> {
        let mut r = req.to_vec();
        r[0] = 0; // echo reply
        r
    }

    #[test]
    fn echo_round_trips_seq() {
        for seq in [0u16, 1, 65535] {
            let req = encode_echo(seq);
            assert_eq!(req[0], 8);
            assert_eq!(&req[8..], PAYLOAD);
            let from = Ipv4Addr::new(1, 1, 1, 1);
            match parse_echo_reply(&reply_for(&req), from) {
                Some(TransportEvent::EchoReply { seq: s, from: f }) => {
                    assert_eq!(s, seq);
                    assert_eq!(f, from);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn short_reply_is_none() {
        assert_eq!(parse_echo_reply(&[0, 0, 0], Ipv4Addr::LOCALHOST), None);
    }

    #[test]
    fn non_reply_type_is_none() {
        let req = encode_echo(5);
        assert_eq!(parse_echo_reply(&req, Ipv4Addr::LOCALHOST), None);
    }

    fn ext_err(origin: u8, icmp_type: u8, offender: [u8; 4]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&113u32.to_ne_bytes()); // ee_errno (EHOSTUNREACH)
        v.push(origin);
        v.push(icmp_type);
        v.push(0); // code
        v.push(0); // pad
        v.extend_from_slice(&0u32.to_ne_bytes()); // info
        v.extend_from_slice(&0u32.to_ne_bytes()); // data
        v.extend_from_slice(&2u16.to_ne_bytes()); // sin_family AF_INET
        v.extend_from_slice(&0u16.to_be_bytes()); // sin_port
        v.extend_from_slice(&offender);
        v.extend_from_slice(&[0u8; 8]);
        v
    }

    #[test]
    fn errqueue_time_exceeded() {
        let payload = encode_echo(4242);
        let ev = parse_errqueue(&ext_err(2, 11, [10, 0, 0, 1]), &payload);
        assert_eq!(
            ev,
            Some(TransportEvent::TimeExceeded {
                seq: 4242,
                from: Ipv4Addr::new(10, 0, 0, 1)
            })
        );
    }

    #[test]
    fn errqueue_unreachable() {
        let payload = encode_echo(9);
        let ev = parse_errqueue(&ext_err(2, 3, [10, 0, 0, 9]), &payload);
        assert_eq!(
            ev,
            Some(TransportEvent::Unreachable {
                seq: 9,
                from: Ipv4Addr::new(10, 0, 0, 9)
            })
        );
    }

    #[test]
    fn errqueue_non_icmp_origin_is_none() {
        let payload = encode_echo(9);
        assert_eq!(
            parse_errqueue(&ext_err(1, 11, [10, 0, 0, 1]), &payload),
            None
        );
    }

    #[test]
    fn errqueue_short_inputs_are_none() {
        assert_eq!(parse_errqueue(&[0u8; 10], &encode_echo(1)), None);
        assert_eq!(
            parse_errqueue(&ext_err(2, 11, [10, 0, 0, 1]), &[8, 0]),
            None
        );
    }
}
