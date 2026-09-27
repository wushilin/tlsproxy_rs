//! HAProxy PROXY protocol, sending side.
//!
//! A route may ask for the client's real address to be announced to its
//! upstream. The header is the first thing written on the raw upstream TCP
//! connection — ahead of any upstream TLS handshake and ahead of a relayed
//! ClientHello — so it works for every mode that opens an upstream
//! connection, passthrough included. The upstream must be configured to
//! expect it (nginx `proxy_protocol`, HAProxy `accept-proxy`): a backend that
//! is not will read the header as garbage, and one that is will reject a
//! connection arriving without it.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncWrite, AsyncWriteExt};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProxyProtocol {
    #[default]
    None,
    V1,
    V2,
}

const V2_SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";

impl ProxyProtocol {
    pub fn is_none(&self) -> bool { *self == Self::None }

    pub fn label(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::V1 => "v1",
            Self::V2 => "v2",
        }
    }

    /// Header announcing `client` (the accepted peer) and `local` (the
    /// address it connected to). Empty for `None`.
    pub fn header(&self, client: SocketAddr, local: SocketAddr) -> Vec<u8> {
        let (client, local) = same_family(client, local);
        match self {
            Self::None => Vec::new(),
            Self::V1 => {
                let family = if client.is_ipv4() { "TCP4" } else { "TCP6" };
                format!("PROXY {family} {} {} {} {}\r\n", client.ip(), local.ip(), client.port(), local.port()).into_bytes()
            }
            Self::V2 => {
                let mut header = V2_SIGNATURE.to_vec();
                // Version 2, command PROXY.
                header.push(0x21);
                match (client.ip(), local.ip()) {
                    (IpAddr::V4(source), IpAddr::V4(destination)) => {
                        // AF_INET + STREAM, 12 address bytes.
                        header.push(0x11);
                        header.extend_from_slice(&12u16.to_be_bytes());
                        header.extend_from_slice(&source.octets());
                        header.extend_from_slice(&destination.octets());
                    }
                    (source, destination) => {
                        // AF_INET6 + STREAM, 36 address bytes.
                        header.push(0x21);
                        header.extend_from_slice(&36u16.to_be_bytes());
                        header.extend_from_slice(&to_v6(source).octets());
                        header.extend_from_slice(&to_v6(destination).octets());
                    }
                }
                header.extend_from_slice(&client.port().to_be_bytes());
                header.extend_from_slice(&local.port().to_be_bytes());
                header
            }
        }
    }

    /// Header for a connection the proxy opens on its own behalf — a health
    /// probe. It tells a PROXY-expecting backend there is no client to
    /// announce, so the probe is not rejected for arriving bare.
    pub fn local_header(&self) -> Vec<u8> {
        match self {
            Self::None => Vec::new(),
            Self::V1 => b"PROXY UNKNOWN\r\n".to_vec(),
            Self::V2 => {
                let mut header = V2_SIGNATURE.to_vec();
                // Version 2, command LOCAL; AF_UNSPEC, no address block.
                header.extend_from_slice(&[0x20, 0x00, 0x00, 0x00]);
                header
            }
        }
    }

    /// Writes the client header as the first bytes of a fresh upstream
    /// connection. A no-op for `None`.
    pub async fn announce<W: AsyncWrite + Unpin>(&self, upstream: &mut W, client: SocketAddr, local: SocketAddr) -> std::io::Result<()> {
        if self.is_none() { return Ok(()); }
        upstream.write_all(&self.header(client, local)).await
    }
}

fn to_v6(address: IpAddr) -> Ipv6Addr {
    match address {
        IpAddr::V4(address) => address.to_ipv6_mapped(),
        IpAddr::V6(address) => address,
    }
}

/// Both addresses must be written in one family. A dual-stack listener
/// reports IPv4 clients as IPv4-mapped IPv6, so those are unwrapped first;
/// a pair that still disagrees is widened to IPv6.
fn same_family(client: SocketAddr, local: SocketAddr) -> (SocketAddr, SocketAddr) {
    let client = SocketAddr::new(client.ip().to_canonical(), client.port());
    let local = SocketAddr::new(local.ip().to_canonical(), local.port());
    if client.is_ipv4() == local.is_ipv4() { return (client, local); }
    (SocketAddr::new(IpAddr::V6(to_v6(client.ip())), client.port()), SocketAddr::new(IpAddr::V6(to_v6(local.ip())), local.port()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(value: &str) -> SocketAddr { value.parse().unwrap() }

    #[test]
    fn none_writes_nothing() {
        assert!(ProxyProtocol::None.header(addr("1.2.3.4:5"), addr("6.7.8.9:443")).is_empty());
        assert!(ProxyProtocol::None.local_header().is_empty());
    }

    #[test]
    fn v1_formats_both_families() {
        assert_eq!(ProxyProtocol::V1.header(addr("203.0.113.7:51234"), addr("192.0.2.1:443")), b"PROXY TCP4 203.0.113.7 192.0.2.1 51234 443\r\n");
        assert_eq!(ProxyProtocol::V1.header(addr("[2001:db8::7]:51234"), addr("[2001:db8::1]:443")), b"PROXY TCP6 2001:db8::7 2001:db8::1 51234 443\r\n");
    }

    #[test]
    fn mapped_ipv4_client_on_dual_stack_listener_is_announced_as_ipv4() {
        assert_eq!(ProxyProtocol::V1.header(addr("[::ffff:203.0.113.7]:51234"), addr("[::ffff:192.0.2.1]:443")), b"PROXY TCP4 203.0.113.7 192.0.2.1 51234 443\r\n");
    }

    #[test]
    fn mixed_families_widen_to_ipv6() {
        let header = ProxyProtocol::V1.header(addr("203.0.113.7:51234"), addr("[2001:db8::1]:443"));
        assert_eq!(header, b"PROXY TCP6 ::ffff:203.0.113.7 2001:db8::1 51234 443\r\n");
    }

    #[test]
    fn v2_ipv4_layout() {
        let header = ProxyProtocol::V2.header(addr("203.0.113.7:51234"), addr("192.0.2.1:443"));
        assert_eq!(&header[..12], &V2_SIGNATURE);
        assert_eq!(&header[12..16], &[0x21, 0x11, 0x00, 0x0c]);
        assert_eq!(&header[16..20], &[203, 0, 113, 7]);
        assert_eq!(&header[20..24], &[192, 0, 2, 1]);
        assert_eq!(&header[24..26], &51234u16.to_be_bytes());
        assert_eq!(&header[26..28], &443u16.to_be_bytes());
        assert_eq!(header.len(), 28);
    }

    #[test]
    fn v2_ipv6_layout() {
        let header = ProxyProtocol::V2.header(addr("[2001:db8::7]:51234"), addr("[2001:db8::1]:443"));
        assert_eq!(&header[12..16], &[0x21, 0x21, 0x00, 0x24]);
        assert_eq!(header.len(), 16 + 36);
        assert_eq!(&header[48..50], &51234u16.to_be_bytes());
    }

    #[test]
    fn probe_headers_announce_no_client() {
        assert_eq!(ProxyProtocol::V1.local_header(), b"PROXY UNKNOWN\r\n");
        let header = ProxyProtocol::V2.local_header();
        assert_eq!(&header[12..], &[0x20, 0x00, 0x00, 0x00]);
    }

    /// Decodes a header with the independent `ppp` parser and returns the
    /// announced (source, destination), asserting it consumed every byte.
    fn decode(header: &[u8]) -> (SocketAddr, SocketAddr) {
        use ppp::{v1, v2, HeaderResult};
        match HeaderResult::parse(header) {
            HeaderResult::V1(Ok(parsed)) => {
                assert_eq!(parsed.header.len(), header.len(), "v1 header has trailing bytes");
                match parsed.addresses {
                    v1::Addresses::Tcp4(a) => (SocketAddr::new(a.source_address.into(), a.source_port), SocketAddr::new(a.destination_address.into(), a.destination_port)),
                    v1::Addresses::Tcp6(a) => (SocketAddr::new(a.source_address.into(), a.source_port), SocketAddr::new(a.destination_address.into(), a.destination_port)),
                    v1::Addresses::Unknown => panic!("expected addresses"),
                }
            }
            HeaderResult::V2(Ok(parsed)) => {
                assert_eq!(parsed.len(), header.len(), "v2 length field disagrees with the bytes sent");
                assert_eq!(parsed.version, v2::Version::Two);
                assert_eq!(parsed.command, v2::Command::Proxy);
                assert_eq!(parsed.protocol, v2::Protocol::Stream);
                match parsed.addresses {
                    v2::Addresses::IPv4(a) => (SocketAddr::new(a.source_address.into(), a.source_port), SocketAddr::new(a.destination_address.into(), a.destination_port)),
                    v2::Addresses::IPv6(a) => (SocketAddr::new(a.source_address.into(), a.source_port), SocketAddr::new(a.destination_address.into(), a.destination_port)),
                    other => panic!("unexpected address family: {other:?}"),
                }
            }
            other => panic!("independent parser rejected the header: {other:?}"),
        }
    }

    #[test]
    fn independent_parser_recovers_the_announced_addresses() {
        let pairs = [
            ("203.0.113.7:51234", "192.0.2.1:443"),
            ("[2001:db8::7]:51234", "[2001:db8:ffff::1]:8443"),
            // Extremes of every field width.
            ("255.255.255.255:65535", "0.0.0.1:1"),
            ("[ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:65535", "[::1]:1"),
        ];
        for version in [ProxyProtocol::V1, ProxyProtocol::V2] {
            for (client, local) in pairs {
                let header = version.header(addr(client), addr(local));
                assert_eq!(decode(&header), (addr(client), addr(local)), "{version:?} {client} -> {local}");
            }
        }
    }

    #[test]
    fn independent_parser_sees_mapped_and_mixed_families_as_intended() {
        for version in [ProxyProtocol::V1, ProxyProtocol::V2] {
            // Dual-stack listener: an IPv4 client must come out as plain IPv4.
            let header = version.header(addr("[::ffff:203.0.113.7]:51234"), addr("[::ffff:192.0.2.1]:443"));
            assert_eq!(decode(&header), (addr("203.0.113.7:51234"), addr("192.0.2.1:443")));
            // Genuinely mixed pair: widened to IPv6, IPv4 side mapped.
            let header = version.header(addr("203.0.113.7:51234"), addr("[2001:db8::1]:443"));
            assert_eq!(decode(&header), (addr("[::ffff:203.0.113.7]:51234"), addr("[2001:db8::1]:443")));
        }
    }

    #[test]
    fn independent_parser_accepts_probe_headers_and_leaves_the_payload() {
        use ppp::{v1, v2, HeaderResult};
        let mut wire = ProxyProtocol::V1.local_header();
        wire.extend_from_slice(b"GET / HTTP/1.1\r\n");
        let HeaderResult::V1(Ok(parsed)) = HeaderResult::parse(&wire) else { panic!("v1 probe header rejected") };
        assert_eq!(parsed.addresses, v1::Addresses::Unknown);
        assert_eq!(&wire[parsed.header.len()..], b"GET / HTTP/1.1\r\n");

        let mut wire = ProxyProtocol::V2.local_header();
        wire.extend_from_slice(b"\x16\x03\x01");
        let HeaderResult::V2(Ok(parsed)) = HeaderResult::parse(&wire) else { panic!("v2 probe header rejected") };
        assert_eq!(parsed.command, v2::Command::Local);
        assert_eq!(parsed.addresses, v2::Addresses::Unspecified);
        assert_eq!(&wire[parsed.len()..], b"\x16\x03\x01");
    }

    #[test]
    fn serializes_as_snake_case() {
        assert_eq!(serde_json::to_string(&ProxyProtocol::V2).unwrap(), "\"v2\"");
        assert_eq!(serde_json::from_str::<ProxyProtocol>("\"none\"").unwrap(), ProxyProtocol::None);
    }
}
