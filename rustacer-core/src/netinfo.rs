//! "Network at a glance" for the dashboard: the active adapter (name, Wi-Fi or Ethernet,
//! link speed, local addresses) and how many connections are open. Read-only.

use crate::error::{CoreError, Result};
use crate::netconn::{self, Protocol};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use windows::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST,
    GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
};

const ERROR_BUFFER_OVERFLOW: u32 = 111;
const AF_UNSPEC: u32 = 0;
const AF_INET: u16 = 2;
const AF_INET6: u16 = 23;
const IF_OPER_STATUS_UP: i32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    Ethernet,
    WiFi,
    Loopback,
    Tunnel,
    Other,
}

impl AdapterKind {
    /// `IfType` from IANA ifType numbers.
    pub fn from_if_type(t: u32) -> Self {
        match t {
            6 => AdapterKind::Ethernet,
            71 => AdapterKind::WiFi,
            24 => AdapterKind::Loopback,
            131 => AdapterKind::Tunnel,
            _ => AdapterKind::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AdapterKind::Ethernet => "Ethernet",
            AdapterKind::WiFi => "Wi-Fi",
            AdapterKind::Loopback => "Loopback",
            AdapterKind::Tunnel => "Tunnel",
            AdapterKind::Other => "Network",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterInfo {
    /// Friendly name, e.g. "Wi-Fi" or "Ethernet 2".
    pub name: String,
    /// Hardware description, e.g. "Intel(R) Wi-Fi 6E AX211 160MHz".
    pub description: String,
    pub kind: AdapterKind,
    pub up: bool,
    pub has_gateway: bool,
    /// Link speed in bits per second (receive side).
    pub link_bps: u64,
    pub addresses: Vec<IpAddr>,
}

impl AdapterInfo {
    /// The address to show: IPv4 first, then a global IPv6, then anything.
    pub fn main_address(&self) -> Option<IpAddr> {
        self.addresses
            .iter()
            .find(|a| a.is_ipv4())
            .or_else(|| {
                self.addresses.iter().find(|a| match a {
                    IpAddr::V6(v6) => !is_link_local_v6(v6),
                    IpAddr::V4(_) => false,
                })
            })
            .or_else(|| self.addresses.first())
            .copied()
    }
}

fn is_link_local_v6(a: &Ipv6Addr) -> bool {
    (a.segments()[0] & 0xffc0) == 0xfe80
}

/// The adapter that carries traffic: up, not loopback or tunnel, with a gateway and an
/// address; falling back to any up adapter with an address.
pub fn primary(adapters: &[AdapterInfo]) -> Option<&AdapterInfo> {
    let real = |a: &&AdapterInfo| {
        a.up && !matches!(a.kind, AdapterKind::Loopback | AdapterKind::Tunnel)
            && !a.addresses.is_empty()
    };
    adapters
        .iter()
        .filter(real)
        .filter(|a| a.has_gateway)
        // Prefer physical Ethernet / Wi-Fi over virtual "Other" adapters with a gateway.
        .min_by_key(|a| match a.kind {
            AdapterKind::Ethernet | AdapterKind::WiFi => 0,
            _ => 1,
        })
        .or_else(|| adapters.iter().find(real))
}

/// `1 Gbps`, `866.7 Mbps`, `54 Mbps`, `0 bps`.
pub fn format_link_speed(bps: u64) -> String {
    let trim = |v: f64| {
        let s = format!("{v:.1}");
        s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
    };
    if bps >= 1_000_000_000 {
        format!("{} Gbps", trim(bps as f64 / 1e9))
    } else if bps >= 1_000_000 {
        format!("{} Mbps", trim(bps as f64 / 1e6))
    } else if bps >= 1_000 {
        format!("{} kbps", trim(bps as f64 / 1e3))
    } else {
        format!("{bps} bps")
    }
}

/// Decode a raw `SOCKADDR` (`sockaddr_in` or `sockaddr_in6`) into an address.
pub fn ip_from_sockaddr(b: &[u8]) -> Option<IpAddr> {
    let family = u16::from_le_bytes([*b.first()?, *b.get(1)?]);
    match family {
        AF_INET if b.len() >= 8 => Some(IpAddr::V4(Ipv4Addr::new(b[4], b[5], b[6], b[7]))),
        AF_INET6 if b.len() >= 24 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(&b[8..24]);
            Some(IpAddr::V6(Ipv6Addr::from(a)))
        }
        _ => None,
    }
}

/// Every network adapter Windows knows. Call from a worker thread.
pub fn read_adapters() -> Result<Vec<AdapterInfo>> {
    let flags = GAA_FLAG_INCLUDE_GATEWAYS
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 16 * 1024;
    // u64 elements keep the buffer 8-byte aligned, as IP_ADAPTER_ADDRESSES requires.
    let mut buf: Vec<u64> = Vec::new();
    for _ in 0..4 {
        buf = vec![0u64; (size as usize).div_ceil(8)];
        let mut len = (buf.len() * 8) as u32;
        // SAFETY: `buf` is an aligned, writable buffer of `len` bytes that outlives the call.
        let rc = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC,
                flags,
                None,
                Some(buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()),
                &mut len,
            )
        };
        match rc {
            0 => {
                size = 0;
                break;
            }
            ERROR_BUFFER_OVERFLOW => size = len + 4096,
            232 => return Ok(vec![]), // ERROR_NO_DATA: no adapters at all
            e => {
                return Err(CoreError::Windows(format!(
                    "GetAdaptersAddresses failed (error {e})"
                )))
            }
        }
    }
    if size != 0 {
        return Err(CoreError::Windows(
            "The adapter list kept growing; try again".into(),
        ));
    }
    let mut out = Vec::new();
    let mut p = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    while !p.is_null() {
        // SAFETY: `p` points into `buf` (the first entry) or follows a `Next` pointer that
        // Windows wrote into `buf`; `buf` is alive and unchanged for this whole loop.
        let a = unsafe { &*p };
        // SAFETY: Windows fills these with valid NUL-terminated strings (or null).
        let name = unsafe { a.FriendlyName.to_string() }.unwrap_or_default();
        // SAFETY: as above.
        let description = unsafe { a.Description.to_string() }.unwrap_or_default();
        let mut addresses = Vec::new();
        let mut u = a.FirstUnicastAddress;
        while !u.is_null() {
            // SAFETY: unicast entries are a linked list inside `buf`.
            let ua = unsafe { &*u };
            let sa = ua.Address;
            if !sa.lpSockaddr.is_null() && sa.iSockaddrLength > 0 {
                // SAFETY: lpSockaddr points to iSockaddrLength bytes inside `buf`.
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        sa.lpSockaddr.cast::<u8>(),
                        sa.iSockaddrLength as usize,
                    )
                };
                if let Some(ip) = ip_from_sockaddr(bytes) {
                    addresses.push(ip);
                }
            }
            u = ua.Next;
        }
        out.push(AdapterInfo {
            name,
            description,
            kind: AdapterKind::from_if_type(a.IfType),
            up: a.OperStatus.0 == IF_OPER_STATUS_UP,
            has_gateway: !a.FirstGatewayAddress.is_null(),
            // Disconnected adapters report u64::MAX ("unknown"); keep that as 0.
            link_bps: [a.ReceiveLinkSpeed, a.TransmitLinkSpeed]
                .into_iter()
                .filter(|v| *v != u64::MAX)
                .max()
                .unwrap_or(0),
            addresses,
        });
        p = a.Next;
    }
    Ok(out)
}

/// What the dashboard shows.
#[derive(Debug, Clone, Default)]
pub struct NetSummary {
    pub adapter: Option<AdapterInfo>,
    /// Established TCP connections.
    pub established: usize,
    /// All TCP and UDP endpoints.
    pub total: usize,
    pub error: Option<String>,
}

/// Active adapter and connection counts. Call from a worker thread. Read-only.
pub fn summary() -> NetSummary {
    let mut s = NetSummary::default();
    match read_adapters() {
        Ok(v) => s.adapter = primary(&v).cloned(),
        Err(e) => s.error = Some(e.to_string()),
    }
    match netconn::read_connections() {
        Ok(c) => {
            s.total = c.len();
            s.established = c
                .iter()
                .filter(|x| x.protocol == Protocol::Tcp && x.state == "Established")
                .count();
        }
        Err(e) => s.error = Some(e.to_string()),
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ad(name: &str, kind: AdapterKind, up: bool, gw: bool, addrs: &[&str]) -> AdapterInfo {
        AdapterInfo {
            name: name.into(),
            description: format!("{name} adapter"),
            kind,
            up,
            has_gateway: gw,
            link_bps: 1_000_000_000,
            addresses: addrs.iter().map(|a| a.parse().unwrap()).collect(),
        }
    }

    #[test]
    fn picks_the_adapter_with_a_gateway() {
        let v = vec![
            ad(
                "Loopback",
                AdapterKind::Loopback,
                true,
                false,
                &["127.0.0.1"],
            ),
            ad(
                "vEthernet (WSL)",
                AdapterKind::Other,
                true,
                false,
                &["172.20.0.1"],
            ),
            ad("Ethernet", AdapterKind::Ethernet, false, false, &[]),
            ad(
                "Wi-Fi",
                AdapterKind::WiFi,
                true,
                true,
                &["fe80::1", "192.168.1.20"],
            ),
        ];
        let p = primary(&v).unwrap();
        assert_eq!(p.name, "Wi-Fi");
        assert_eq!(p.main_address(), Some("192.168.1.20".parse().unwrap()));
    }

    #[test]
    fn physical_adapters_win_over_virtual_ones() {
        let v = vec![
            ad("VPN", AdapterKind::Other, true, true, &["10.8.0.2"]),
            ad(
                "Ethernet",
                AdapterKind::Ethernet,
                true,
                true,
                &["192.168.0.5"],
            ),
        ];
        assert_eq!(primary(&v).unwrap().name, "Ethernet");
    }

    #[test]
    fn falls_back_without_a_gateway_and_skips_loopback_and_tunnels() {
        let v = vec![
            ad(
                "Loopback",
                AdapterKind::Loopback,
                true,
                true,
                &["127.0.0.1"],
            ),
            ad("Teredo", AdapterKind::Tunnel, true, true, &["2001:0::1"]),
            ad(
                "Ethernet",
                AdapterKind::Ethernet,
                true,
                false,
                &["169.254.3.4"],
            ),
        ];
        assert_eq!(primary(&v).unwrap().name, "Ethernet");
        assert!(primary(&[]).is_none());
        assert!(primary(&v[..2]).is_none());
    }

    #[test]
    fn ipv6_only_prefers_a_global_address() {
        let a = ad(
            "Eth",
            AdapterKind::Ethernet,
            true,
            true,
            &["fe80::1", "2001:db8::5"],
        );
        assert_eq!(a.main_address(), Some("2001:db8::5".parse().unwrap()));
    }

    #[test]
    fn link_speeds() {
        assert_eq!(format_link_speed(1_000_000_000), "1 Gbps");
        assert_eq!(format_link_speed(2_500_000_000), "2.5 Gbps");
        assert_eq!(format_link_speed(866_700_000), "866.7 Mbps");
        assert_eq!(format_link_speed(54_000_000), "54 Mbps");
        assert_eq!(format_link_speed(9_600), "9.6 kbps");
        assert_eq!(format_link_speed(0), "0 bps");
    }

    #[test]
    fn sockaddrs_decode() {
        let v4 = [2u8, 0, 0, 80, 192, 168, 1, 20, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(ip_from_sockaddr(&v4), Some("192.168.1.20".parse().unwrap()));
        let mut v6 = vec![23u8, 0, 0, 0, 0, 0, 0, 0];
        v6.extend("2001:db8::1".parse::<Ipv6Addr>().unwrap().octets());
        v6.extend([0, 0, 0, 0]);
        assert_eq!(ip_from_sockaddr(&v6), Some("2001:db8::1".parse().unwrap()));
        assert_eq!(ip_from_sockaddr(&[]), None);
        assert_eq!(ip_from_sockaddr(&[2, 0, 1]), None);
        assert_eq!(ip_from_sockaddr(&[99, 0, 0, 0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn kinds_from_if_type() {
        assert_eq!(AdapterKind::from_if_type(6), AdapterKind::Ethernet);
        assert_eq!(AdapterKind::from_if_type(71), AdapterKind::WiFi);
        assert_eq!(AdapterKind::from_if_type(24), AdapterKind::Loopback);
        assert_eq!(AdapterKind::from_if_type(131), AdapterKind::Tunnel);
        assert_eq!(AdapterKind::from_if_type(53), AdapterKind::Other);
        assert_eq!(AdapterKind::WiFi.label(), "Wi-Fi");
    }
}
