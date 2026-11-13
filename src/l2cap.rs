//! Minimal async L2CAP SEQPACKET client (no root needed for outgoing connections).

use anyhow::{Context, Result, bail};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

const AF_BLUETOOTH: i32 = 31;
const BTPROTO_L2CAP: i32 = 0;

#[repr(C)]
struct SockaddrL2 {
    l2_family: libc::sa_family_t,
    l2_psm: u16,
    l2_bdaddr: [u8; 6],
    l2_cid: u16,
    l2_bdaddr_type: u8,
}

pub fn parse_addr(s: &str) -> Result<[u8; 6]> {
    let parts: Vec<_> = s.split(':').collect();
    if parts.len() != 6 {
        bail!("bad Bluetooth address {s}");
    }
    let mut out = [0u8; 6];
    // bdaddr_t is stored little endian: the last octet of the string comes first.
    for (i, p) in parts.iter().enumerate() {
        out[5 - i] = u8::from_str_radix(p, 16).with_context(|| format!("bad Bluetooth address {s}"))?;
    }
    Ok(out)
}

pub struct L2cap {
    fd: AsyncFd<OwnedFd>,
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

fn connect_blocking(addr: [u8; 6], psm: u16) -> io::Result<OwnedFd> {
    unsafe {
        let fd = cvt(libc::socket(AF_BLUETOOTH, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, BTPROTO_L2CAP))?;
        let fd = OwnedFd::from_raw_fd(fd);
        let tv = libc::timeval { tv_sec: 8, tv_usec: 0 };
        cvt(libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of_val(&tv) as u32,
        ))?;
        let sa = SockaddrL2 {
            l2_family: AF_BLUETOOTH as libc::sa_family_t,
            l2_psm: psm.to_le(),
            l2_bdaddr: addr,
            l2_cid: 0,
            l2_bdaddr_type: 0, // BDADDR_BREDR
        };
        cvt(libc::connect(
            fd.as_raw_fd(),
            &sa as *const _ as *const libc::sockaddr,
            std::mem::size_of::<SockaddrL2>() as u32,
        ))?;
        let flags = cvt(libc::fcntl(fd.as_raw_fd(), libc::F_GETFL))?;
        cvt(libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK))?;
        Ok(fd)
    }
}

impl L2cap {
    pub async fn connect(addr: &str, psm: u16) -> Result<L2cap> {
        let a = parse_addr(addr)?;
        let fd = tokio::task::spawn_blocking(move || connect_blocking(a, psm))
            .await?
            .with_context(|| format!("L2CAP connect to {addr} PSM {psm:#06x}"))?;
        // SAFETY: we own this freshly created socket exclusively; it is never shared or duplicated.
        let fd = unsafe { AsyncFd::register_with_interest(fd, Interest::READABLE | Interest::WRITABLE) }
            .map_err(|e| anyhow::anyhow!("registering L2CAP socket: {e}"))?;
        Ok(L2cap { fd })
    }

    pub async fn send(&self, data: &[u8]) -> Result<()> {
        loop {
            let mut g = self.fd.writable().await?;
            match g.try_io(|fd| io_res(unsafe { libc::send(raw(fd), data.as_ptr().cast(), data.len(), 0) })) {
                Ok(r) => {
                    r?;
                    return Ok(());
                }
                Err(_would_block) => continue,
            }
        }
    }

    /// Next packet. An empty result means the AirPods closed the channel.
    pub async fn recv(&self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 1024];
        loop {
            let mut g = self.fd.readable().await?;
            match g.try_io(|fd| io_res(unsafe { libc::recv(raw(fd), buf.as_mut_ptr().cast(), buf.len(), 0) })) {
                Ok(r) => {
                    let n = r?;
                    buf.truncate(n);
                    return Ok(buf);
                }
                Err(_would_block) => continue,
            }
        }
    }
}

fn raw(fd: &AsyncFd<OwnedFd>) -> RawFd {
    fd.get_ref().as_raw_fd()
}

fn io_res(n: isize) -> io::Result<usize> {
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn addr_is_reversed() {
        assert_eq!(super::parse_addr("12:34:56:78:9A:BC").unwrap(), [0xbc, 0x9a, 0x78, 0x56, 0x34, 0x12]);
        assert!(super::parse_addr("nope").is_err());
    }
}
