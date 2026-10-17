//! Network connections with their owning process, from `GetExtendedTcpTable` and
//! `GetExtendedUdpTable` (IPv4 and IPv6). Read-only.
//!
//! The tables are parsed from raw bytes by small pure functions, so they are unit-tested
//! with byte fixtures instead of the live system.

use crate::error::{CoreError, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, GetExtendedUdpTable, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
};
use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};

const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const TCP4_ROW: usize = 24;
const TCP6_ROW: usize = 56;
const UDP4_ROW: usize = 12;
const UDP6_ROW: usize = 28;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    Tcp,
    Udp,
}

impl Protocol {
    pub fn label(self, v6: bool) -> &'static str {
        match (self, v6) {
            (Protocol::Tcp, false) => "TCP",
            (Protocol::Tcp, true) => "TCP6",
            (Protocol::Udp, false) => "UDP",
            (Protocol::Udp, true) => "UDP6",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub protocol: Protocol,
    pub v6: bool,
    pub local: IpAddr,
    pub local_port: u16,
    /// `None` for UDP, which has no peer.
    pub remote: Option<IpAddr>,
    pub remote_port: u16,
    /// IPv6 scope (interface) ids; 0 when not used.
    pub local_scope: u32,
    pub remote_scope: u32,
    /// TCP state name; empty for UDP.
    pub state: &'static str,
    pub pid: u32,
}

impl Connection {
    pub fn local_text(&self) -> String {
        endpoint(self.local, self.local_scope, self.local_port)
    }

    /// `*:*` for UDP and for listening sockets (no peer yet).
    pub fn remote_text(&self) -> String {
        match self.remote {
            Some(ip) if self.remote_port != 0 || !ip.is_unspecified() => {
                endpoint(ip, self.remote_scope, self.remote_port)
            }
            _ => "*:*".to_string(),
        }
    }
}

/// `1.2.3.4:80`, `[::1]:80`, and `[fe80::1%12]:80` for scoped (link-local) addresses.
fn endpoint(ip: IpAddr, scope: u32, port: u16) -> String {
    match ip {
        IpAddr::V4(a) => format!("{a}:{port}"),
        IpAddr::V6(a) if scope != 0 => format!("[{a}%{scope}]:{port}"),
        IpAddr::V6(a) => format!("[{a}]:{port}"),
    }
}

/// `MIB_TCP_STATE` as the usual names.
pub fn tcp_state_name(code: u32) -> &'static str {
    match code {
        1 => "Closed",
        2 => "Listen",
        3 => "SYN sent",
        4 => "SYN received",
        5 => "Established",
        6 => "FIN wait 1",
        7 => "FIN wait 2",
        8 => "Close wait",
        9 => "Closing",
        10 => "Last ACK",
        11 => "Time wait",
        12 => "Delete TCB",
        _ => "Unknown",
    }
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// A port is stored in network byte order in the first two bytes of a DWORD.
fn port_at(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

fn v4_at(b: &[u8], off: usize) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(b[off], b[off + 1], b[off + 2], b[off + 3]))
}

fn v6_at(b: &[u8], off: usize) -> IpAddr {
    let mut a = [0u8; 16];
    a.copy_from_slice(&b[off..off + 16]);
    IpAddr::V6(Ipv6Addr::from(a))
}

/// Rows that fit in the buffer, honouring the declared count but never reading past the end.
fn rows(bytes: &[u8], row_len: usize) -> impl Iterator<Item = &[u8]> {
    let declared = if bytes.len() >= 4 {
        u32_at(bytes, 0) as usize
    } else {
        0
    };
    let fit = bytes.len().saturating_sub(4) / row_len;
    bytes
        .get(4..)
        .unwrap_or(&[])
        .chunks_exact(row_len)
        .take(declared.min(fit))
}

/// `MIB_TCPTABLE_OWNER_PID`.
pub fn parse_tcp4(bytes: &[u8]) -> Vec<Connection> {
    rows(bytes, TCP4_ROW)
        .map(|r| Connection {
            protocol: Protocol::Tcp,
            v6: false,
            state: tcp_state_name(u32_at(r, 0)),
            local: v4_at(r, 4),
            local_port: port_at(r, 8),
            remote: Some(v4_at(r, 12)),
            remote_port: port_at(r, 16),
            local_scope: 0,
            remote_scope: 0,
            pid: u32_at(r, 20),
        })
        .collect()
}

/// `MIB_TCP6TABLE_OWNER_PID`.
pub fn parse_tcp6(bytes: &[u8]) -> Vec<Connection> {
    rows(bytes, TCP6_ROW)
        .map(|r| Connection {
            protocol: Protocol::Tcp,
            v6: true,
            local: v6_at(r, 0),
            local_port: port_at(r, 20),
            remote: Some(v6_at(r, 24)),
            remote_port: port_at(r, 44),
            state: tcp_state_name(u32_at(r, 48)),
            local_scope: u32_at(r, 16),
            remote_scope: u32_at(r, 40),
            pid: u32_at(r, 52),
        })
        .collect()
}

/// `MIB_UDPTABLE_OWNER_PID`.
pub fn parse_udp4(bytes: &[u8]) -> Vec<Connection> {
    rows(bytes, UDP4_ROW)
        .map(|r| Connection {
            protocol: Protocol::Udp,
            v6: false,
            local: v4_at(r, 0),
            local_port: port_at(r, 4),
            remote: None,
            remote_port: 0,
            state: "",
            local_scope: 0,
            remote_scope: 0,
            pid: u32_at(r, 8),
        })
        .collect()
}

/// `MIB_UDP6TABLE_OWNER_PID`.
pub fn parse_udp6(bytes: &[u8]) -> Vec<Connection> {
    rows(bytes, UDP6_ROW)
        .map(|r| Connection {
            protocol: Protocol::Udp,
            v6: true,
            local: v6_at(r, 0),
            local_port: port_at(r, 20),
            remote: None,
            remote_port: 0,
            state: "",
            local_scope: u32_at(r, 16),
            remote_scope: 0,
            pid: u32_at(r, 24),
        })
        .collect()
}

// ---------- live reading ----------

/// Call an IP Helper "get table" function until its buffer is large enough. The table can
/// grow between the sizing call and the real call, so retry a few times.
fn fetch(
    name: &str,
    call: impl Fn(Option<*mut core::ffi::c_void>, &mut u32) -> u32,
) -> Result<Vec<u8>> {
    let mut size = 0u32;
    let rc = call(None, &mut size);
    if rc != ERROR_INSUFFICIENT_BUFFER && rc != 0 {
        return Err(CoreError::Windows(format!("{name} failed (error {rc})")));
    }
    for _ in 0..6 {
        let mut buf = vec![0u8; size as usize + size as usize / 4 + 1024];
        let mut len = buf.len() as u32;
        let rc = call(Some(buf.as_mut_ptr().cast()), &mut len);
        match rc {
            0 => {
                buf.truncate(len as usize);
                return Ok(buf);
            }
            ERROR_INSUFFICIENT_BUFFER => size = len,
            e => return Err(CoreError::Windows(format!("{name} failed (error {e})"))),
        }
    }
    Err(CoreError::Windows(format!(
        "{name}: the connection table kept growing; try again"
    )))
}

/// Every TCP and UDP endpoint (IPv4 and IPv6) with its owning process id. Call from a
/// worker thread.
pub fn read_connections() -> Result<Vec<Connection>> {
    let tcp = |family: u32| {
        fetch("GetExtendedTcpTable", |p, len| {
            // SAFETY: `p` is either None (sizing call) or a buffer of `*len` bytes owned by
            // `fetch`, which stays alive for the duration of the call.
            unsafe { GetExtendedTcpTable(p, len, false, family, TCP_TABLE_OWNER_PID_ALL, 0) }
        })
    };
    let udp = |family: u32| {
        fetch("GetExtendedUdpTable", |p, len| {
            // SAFETY: as above.
            unsafe { GetExtendedUdpTable(p, len, false, family, UDP_TABLE_OWNER_PID, 0) }
        })
    };
    let mut out = Vec::new();
    out.extend(parse_tcp4(&tcp(AF_INET.0 as u32)?));
    out.extend(parse_tcp6(&tcp(AF_INET6.0 as u32)?));
    out.extend(parse_udp4(&udp(AF_INET.0 as u32)?));
    out.extend(parse_udp6(&udp(AF_INET6.0 as u32)?));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dword(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }

    /// A port as it appears in a table: network byte order in the first two bytes.
    fn port(p: u16) -> [u8; 4] {
        let b = p.to_be_bytes();
        [b[0], b[1], 0, 0]
    }

    fn table(n: u32, rows: Vec<Vec<u8>>) -> Vec<u8> {
        let mut v = dword(n).to_vec();
        for r in rows {
            v.extend(r);
        }
        v
    }

    fn tcp4_row(state: u32, l: [u8; 4], lp: u16, r: [u8; 4], rp: u16, pid: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(dword(state));
        v.extend(l);
        v.extend(port(lp));
        v.extend(r);
        v.extend(port(rp));
        v.extend(dword(pid));
        assert_eq!(v.len(), TCP4_ROW);
        v
    }

    fn tcp6_row(state: u32, l: Ipv6Addr, lp: u16, r: Ipv6Addr, rp: u16, pid: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(l.octets());
        v.extend(dword(0)); // scope id
        v.extend(port(lp));
        v.extend(r.octets());
        v.extend(dword(0));
        v.extend(port(rp));
        v.extend(dword(state));
        v.extend(dword(pid));
        assert_eq!(v.len(), TCP6_ROW);
        v
    }

    #[test]
    fn parses_tcp4_rows() {
        let t = table(
            2,
            vec![
                tcp4_row(2, [0, 0, 0, 0], 135, [0, 0, 0, 0], 0, 1100),
                tcp4_row(5, [192, 168, 1, 20], 51234, [142, 250, 80, 46], 443, 4242),
            ],
        );
        let v = parse_tcp4(&t);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].state, "Listen");
        assert_eq!(v[0].local_port, 135);
        assert_eq!(v[0].pid, 1100);
        assert_eq!(v[0].remote_text(), "*:*");
        assert_eq!(v[1].state, "Established");
        assert_eq!(v[1].local_text(), "192.168.1.20:51234");
        assert_eq!(v[1].remote_text(), "142.250.80.46:443");
        assert_eq!(v[1].pid, 4242);
        assert_eq!(v[1].protocol.label(v[1].v6), "TCP");
    }

    #[test]
    fn parses_tcp6_rows() {
        let loop6 = Ipv6Addr::LOCALHOST;
        let peer: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let t = table(1, vec![tcp6_row(5, loop6, 8080, peer, 443, 777)]);
        let v = parse_tcp6(&t);
        assert_eq!(v.len(), 1);
        assert!(v[0].v6);
        assert_eq!(v[0].local_text(), "[::1]:8080");
        assert_eq!(v[0].remote_text(), "[2001:db8::1]:443");
        assert_eq!(v[0].state, "Established");
        assert_eq!(v[0].pid, 777);
        assert_eq!(v[0].protocol.label(true), "TCP6");
    }

    #[test]
    fn parses_udp_tables() {
        let mut r4 = Vec::new();
        r4.extend([0, 0, 0, 0]);
        r4.extend(port(5353));
        r4.extend(dword(2000));
        let v = parse_udp4(&table(1, vec![r4]));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].protocol, Protocol::Udp);
        assert_eq!(v[0].local_text(), "0.0.0.0:5353");
        assert_eq!(v[0].remote_text(), "*:*");
        assert_eq!(v[0].state, "");
        assert_eq!(v[0].pid, 2000);

        let mut r6 = Vec::new();
        r6.extend(Ipv6Addr::UNSPECIFIED.octets());
        r6.extend(dword(0));
        r6.extend(port(1900));
        r6.extend(dword(3000));
        assert_eq!(r6.len(), UDP6_ROW);
        let v = parse_udp6(&table(1, vec![r6]));
        assert_eq!(v[0].local_text(), "[::]:1900");
        assert_eq!(v[0].pid, 3000);
        assert_eq!(v[0].protocol.label(v[0].v6), "UDP6");
    }

    #[test]
    fn port_byte_order_is_network_order() {
        // 0x01BB = 443 stored big-endian in the first two bytes.
        let t = table(1, vec![tcp4_row(5, [1, 2, 3, 4], 443, [5, 6, 7, 8], 80, 1)]);
        let v = parse_tcp4(&t);
        assert_eq!((v[0].local_port, v[0].remote_port), (443, 80));
    }

    #[test]
    fn short_or_lying_buffers_never_panic() {
        assert!(parse_tcp4(&[]).is_empty());
        assert!(parse_tcp4(&[1, 0]).is_empty());
        assert!(parse_tcp6(&dword(5)).is_empty());
        // Declares 3 rows but only 1 fits.
        let t = table(3, vec![tcp4_row(5, [1, 1, 1, 1], 1, [2, 2, 2, 2], 2, 9)]);
        assert_eq!(parse_tcp4(&t).len(), 1);
        // Declares 0 rows but has data: trust the count.
        let t = table(0, vec![tcp4_row(5, [1, 1, 1, 1], 1, [2, 2, 2, 2], 2, 9)]);
        assert!(parse_tcp4(&t).is_empty());
        // A partial trailing row is ignored.
        let mut t = table(1, vec![tcp4_row(5, [1, 1, 1, 1], 1, [2, 2, 2, 2], 2, 9)]);
        t.extend([0u8; 10]);
        assert_eq!(parse_tcp4(&t).len(), 1);
        // The same bytes read with the wrong row size do not crash.
        let _ = parse_udp6(&t);
    }

    #[test]
    fn tcp_state_names() {
        assert_eq!(tcp_state_name(2), "Listen");
        assert_eq!(tcp_state_name(5), "Established");
        assert_eq!(tcp_state_name(11), "Time wait");
        assert_eq!(tcp_state_name(99), "Unknown");
    }

    #[test]
    fn link_local_ipv6_shows_its_scope() {
        let ll: Ipv6Addr = "fe80::1".parse().unwrap();
        let mut row = tcp6_row(5, ll, 5000, ll, 6000, 1);
        row[16..20].copy_from_slice(&dword(12));
        row[40..44].copy_from_slice(&dword(7));
        let v = parse_tcp6(&table(1, vec![row]));
        assert_eq!(v[0].local_text(), "[fe80::1%12]:5000");
        assert_eq!(v[0].remote_text(), "[fe80::1%7]:6000");
    }
}
