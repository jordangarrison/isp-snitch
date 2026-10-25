//! Linux unprivileged ICMP "ping socket" transport.
//!
//! `socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP)` needs no capabilities, only a
//! `net.ipv4.ping_group_range` that covers the process group. TTL Exceeded and
//! Unreachable answers arrive on the socket error queue (`IP_RECVERR`), echo
//! replies on the normal queue.

use super::packet::{encode_echo, parse_echo_reply, parse_errqueue};
use super::{Transport, TransportEvent};
use std::io;
use std::mem::{size_of, zeroed};
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct PingSocket {
    fd: OwnedFd,
    target: Ipv4Addr,
}

impl PingSocket {
    /// Opens a ping socket for `target`, bound to `interface` when given.
    pub fn open(target: Ipv4Addr, interface: Option<&str>) -> io::Result<PingSocket> {
        // SAFETY: plain syscall; the returned fd is owned below.
        let raw = unsafe {
            libc::socket(
                libc::AF_INET,
                libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                libc::IPPROTO_ICMP,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh, valid fd that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if let Some(name) = interface {
            setsockopt_bytes(
                &fd,
                libc::SOL_SOCKET,
                libc::SO_BINDTODEVICE,
                name.as_bytes(),
            )?;
        }
        setsockopt_int(&fd, libc::SOL_IP, libc::IP_RECVERR, 1)?;
        Ok(PingSocket { fd, target })
    }

    fn sendto(&self, pkt: &[u8]) -> io::Result<()> {
        let addr = sockaddr_in(self.target);
        // SAFETY: pkt and addr outlive the call; sizes are exact.
        let n = unsafe {
            libc::sendto(
                self.fd.as_raw_fd(),
                pkt.as_ptr().cast(),
                pkt.len(),
                0,
                (&addr as *const libc::sockaddr_in).cast(),
                size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn read_errqueue(&self) -> io::Result<Option<TransportEvent>> {
        let mut buf = [0u8; 576];
        let mut control = [0u64; 64];
        let mut name: libc::sockaddr_in = unsafe { zeroed() };
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        // SAFETY: zeroed msghdr is a valid empty header; fields set below.
        let mut msg: libc::msghdr = unsafe { zeroed() };
        msg.msg_name = (&mut name as *mut libc::sockaddr_in).cast();
        msg.msg_namelen = size_of::<libc::sockaddr_in>() as libc::socklen_t;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = size_of::<[u64; 64]>() as _;
        // SAFETY: msg points at buffers that live for the whole call.
        let n = unsafe {
            libc::recvmsg(
                self.fd.as_raw_fd(),
                &mut msg,
                libc::MSG_ERRQUEUE | libc::MSG_DONTWAIT,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let payload = &buf[..n as usize];
        // SAFETY: walking the control buffer the kernel just filled, with the
        // libc CMSG helpers that bound every access by msg_controllen.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                let c = &*cmsg;
                if c.cmsg_level == libc::SOL_IP && c.cmsg_type == libc::IP_RECVERR {
                    let data = libc::CMSG_DATA(cmsg);
                    let len = c.cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                    let ext = std::slice::from_raw_parts(data, len);
                    return Ok(parse_errqueue(ext, payload));
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }
        Ok(None)
    }

    fn read_reply(&self) -> io::Result<Option<TransportEvent>> {
        let mut buf = [0u8; 576];
        let mut from: libc::sockaddr_in = unsafe { zeroed() };
        let mut len = size_of::<libc::sockaddr_in>() as libc::socklen_t;
        // SAFETY: buffers live for the call; len is in/out.
        let n = unsafe {
            libc::recvfrom(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::MSG_DONTWAIT,
                (&mut from as *mut libc::sockaddr_in).cast(),
                &mut len,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let src = Ipv4Addr::from(u32::from_be(from.sin_addr.s_addr));
        Ok(parse_echo_reply(&buf[..n as usize], src))
    }
}

/// Errnos that a queued ICMP error (`sk_err`) makes the next socket call
/// return once. They say nothing about the call itself.
fn is_pending_icmp_error(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(
            libc::EHOSTUNREACH
                | libc::ENETUNREACH
                | libc::ECONNREFUSED
                | libc::EHOSTDOWN
                | libc::EPROTO
                | libc::ENOPROTOOPT
                | libc::EMSGSIZE
        )
    )
}

fn would_block(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

impl Transport for PingSocket {
    fn send(&mut self, seq: u16, ttl: u8) -> io::Result<()> {
        setsockopt_int(&self.fd, libc::SOL_IP, libc::IP_TTL, ttl as libc::c_int)?;
        let pkt = encode_echo(seq);
        // A failed attempt clears a pending sk_err, so one retry tells a stale
        // ICMP error apart from a real send failure (no route, link down).
        match self.sendto(&pkt) {
            Err(e) if is_pending_icmp_error(&e) => self.sendto(&pkt),
            other => other,
        }
    }

    fn recv(&mut self, timeout: Duration) -> io::Result<Option<(TransportEvent, Instant)>> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut pfd = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let left = deadline.saturating_duration_since(Instant::now());
            let ms = left.as_millis().min(i32::MAX as u128) as libc::c_int;
            // SAFETY: one valid pollfd.
            let r = unsafe { libc::poll(&mut pfd, 1, ms) };
            if r < 0 {
                let e = io::Error::last_os_error();
                if would_block(&e) {
                    continue;
                }
                return Err(e);
            }
            if r == 0 {
                return Ok(None);
            }
            if pfd.revents & libc::POLLERR != 0 {
                match self.read_errqueue() {
                    Ok(Some(ev)) => return Ok(Some((ev, Instant::now()))),
                    Ok(None) => {}
                    Err(e) if would_block(&e) => {}
                    Err(e) => return Err(e),
                }
            }
            if pfd.revents & libc::POLLIN != 0 {
                match self.read_reply() {
                    Ok(Some(ev)) => return Ok(Some((ev, Instant::now()))),
                    Ok(None) => {}
                    Err(e) if would_block(&e) || is_pending_icmp_error(&e) => {}
                    Err(e) => return Err(e),
                }
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }
}

fn sockaddr_in(ip: Ipv4Addr) -> libc::sockaddr_in {
    // SAFETY: all-zero is a valid sockaddr_in.
    let mut a: libc::sockaddr_in = unsafe { zeroed() };
    a.sin_family = libc::AF_INET as libc::sa_family_t;
    a.sin_addr.s_addr = u32::from(ip).to_be();
    a
}

fn setsockopt_int(fd: &OwnedFd, level: i32, name: i32, v: libc::c_int) -> io::Result<()> {
    setsockopt_bytes(fd, level, name, &v.to_ne_bytes())
}

fn setsockopt_bytes(fd: &OwnedFd, level: i32, name: i32, v: &[u8]) -> io::Result<()> {
    // SAFETY: v outlives the call; length is exact.
    let r = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            level,
            name,
            v.as_ptr().cast(),
            v.len() as libc::socklen_t,
        )
    };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
