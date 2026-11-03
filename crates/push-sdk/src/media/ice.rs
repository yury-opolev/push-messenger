//! The ICE servers of a call as the helper uses them: STUN and TURN URLs (RFC 7064, RFC
//! 7065), the host's own address, and a STUN binding request for the server-reflexive
//! candidate.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use crate::calls::IceServers;

/// How the helper reaches a TURN server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    Udp,
    Tcp,
}

/// A `host:port` from a URL, not resolved yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub(crate) host: String,
    pub(crate) port: u16,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct TurnServer {
    pub(crate) endpoint: Endpoint,
    pub(crate) transport: Transport,
    pub(crate) username: String,
    pub(crate) credential: String,
}

impl std::fmt::Debug for TurnServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnServer")
            .field("endpoint", &self.endpoint)
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}

/// The usable servers of `servers`: STUN URLs, and TURN URLs with a credential, UDP ones
/// first. `turns:` (TLS) URLs are skipped: the helper speaks TURN over UDP and TCP only.
pub(crate) fn usable(servers: &IceServers) -> (Vec<Endpoint>, Vec<TurnServer>) {
    let mut stun = Vec::new();
    let mut turn = Vec::new();
    for server in &servers.servers {
        for url in &server.urls {
            if let Some(rest) = url.strip_prefix("stun:") {
                if let Some(endpoint) = endpoint_of(rest) {
                    stun.push(endpoint);
                }
            } else if let Some(rest) = url.strip_prefix("turn:") {
                let (address, query) = rest.split_once('?').unwrap_or((rest, ""));
                let transport = match query {
                    "" | "transport=udp" => Transport::Udp,
                    "transport=tcp" => Transport::Tcp,
                    _ => continue,
                };
                let (Some(endpoint), Some(username), Some(credential)) = (
                    endpoint_of(address),
                    server.username.clone(),
                    server.credential.clone(),
                ) else {
                    continue;
                };
                turn.push(TurnServer {
                    endpoint,
                    transport,
                    username,
                    credential,
                });
            }
        }
    }
    // Stable: the server's order within each transport.
    turn.sort_by_key(|server| server.transport == Transport::Tcp);
    (stun, turn)
}

/// `host`, `host:port`, `[v6]` or `[v6]:port`; the port defaults to 3478.
fn endpoint_of(address: &str) -> Option<Endpoint> {
    let (host, port) = if let Some(rest) = address.strip_prefix('[') {
        let (host, rest) = rest.split_once(']')?;
        match rest.strip_prefix(':') {
            Some(port) => (host, Some(port)),
            None if rest.is_empty() => (host, None),
            None => return None,
        }
    } else {
        match address.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (address, None),
        }
    };
    let port = match port {
        Some(port) => port.parse().ok().filter(|port| *port != 0)?,
        None => 3478,
    };
    (!host.is_empty()).then(|| Endpoint {
        host: host.to_owned(),
        port,
    })
}

/// The address this host uses to reach the internet (the source address of the default
/// route): its host candidate. No packet is sent. The loopback when there is no route.
pub(crate) fn local_ip() -> IpAddr {
    let routed = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|socket| {
            // TEST-NET-1: never answered, and connecting a UDP socket sends nothing.
            socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?;
            socket.local_addr()
        })
        .map(|address| address.ip());
    match routed {
        Ok(ip) if !ip.is_unspecified() => ip,
        _ => IpAddr::V4(Ipv4Addr::LOCALHOST),
    }
}

const MAGIC_COOKIE: u32 = 0x2112_a442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const MAPPED_ADDRESS: u16 = 0x0001;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// A STUN binding request (RFC 8489) with `transaction` as its id.
pub(crate) fn binding_request(transaction: [u8; 12]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(20);
    packet.extend(BINDING_REQUEST.to_be_bytes());
    packet.extend(0u16.to_be_bytes());
    packet.extend(MAGIC_COOKIE.to_be_bytes());
    packet.extend(transaction);
    packet
}

/// The mapped address of a binding success response to `transaction`, or `None` when
/// `packet` is not one.
pub(crate) fn binding_response(packet: &[u8], transaction: [u8; 12]) -> Option<SocketAddr> {
    let header = packet.get(..20)?;
    let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
    if u16::from_be_bytes([header[0], header[1]]) != BINDING_SUCCESS
        || header[4..8] != MAGIC_COOKIE.to_be_bytes()
        || header[8..20] != transaction
        || packet.len() != 20 + length
    {
        return None;
    }
    let mut attributes = &packet[20..];
    let mut mapped = None;
    while attributes.len() >= 4 {
        let kind = u16::from_be_bytes([attributes[0], attributes[1]]);
        let length = usize::from(u16::from_be_bytes([attributes[2], attributes[3]]));
        let value = attributes.get(4..4 + length)?;
        match kind {
            XOR_MAPPED_ADDRESS => return address(value, true),
            MAPPED_ADDRESS => mapped = address(value, false),
            _ => {}
        }
        let padded = 4 + length.div_ceil(4) * 4;
        attributes = attributes.get(padded..).unwrap_or_default();
    }
    mapped
}

/// An IPv4 (MAPPED-|XOR-MAPPED-)ADDRESS value. IPv6 is not used by the helper.
fn address(value: &[u8], xor: bool) -> Option<SocketAddr> {
    if value.len() != 8 || value[1] != 0x01 {
        return None;
    }
    let mut port = u16::from_be_bytes([value[2], value[3]]);
    let mut ip = u32::from_be_bytes([value[4], value[5], value[6], value[7]]);
    if xor {
        port ^= (MAGIC_COOKIE >> 16) as u16;
        ip ^= MAGIC_COOKIE;
    }
    Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::calls::IceServer;

    fn servers(entries: &[(&[&str], Option<&str>)]) -> IceServers {
        IceServers {
            servers: entries
                .iter()
                .map(|(urls, credential)| IceServer {
                    urls: urls.iter().map(|url| (*url).to_owned()).collect(),
                    username: credential.map(|_| "1700000000:user".to_owned()),
                    credential: credential.map(str::to_owned),
                })
                .collect(),
            expires_at_ms: 0,
            relay: true,
        }
    }

    fn endpoint(host: &str, port: u16) -> Endpoint {
        Endpoint {
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn urls_are_read_with_their_default_ports_and_transports() {
        let (stun, turn) = usable(&servers(&[
            (&["stun:stun.example", "stun:[2001:db8::1]:3479"], None),
            (
                &[
                    "turn:turn.example?transport=tcp",
                    "turns:turn.example:443?transport=tcp",
                    "turn:10.0.0.1:3478",
                    "turn:turn.example:80?transport=udp",
                ],
                Some("secret"),
            ),
        ]));
        assert_eq!(
            stun,
            [
                endpoint("stun.example", 3478),
                endpoint("2001:db8::1", 3479)
            ]
        );
        let found: Vec<_> = turn
            .iter()
            .map(|server| (server.endpoint.clone(), server.transport))
            .collect();
        assert_eq!(
            found,
            [
                (endpoint("10.0.0.1", 3478), Transport::Udp),
                (endpoint("turn.example", 80), Transport::Udp),
                (endpoint("turn.example", 3478), Transport::Tcp),
            ]
        );
        assert_eq!(turn[0].username, "1700000000:user");
        assert_eq!(turn[0].credential, "secret");
    }

    #[test]
    fn turn_without_a_credential_and_unknown_schemes_are_skipped() {
        let (stun, turn) = usable(&servers(&[
            (&["turn:turn.example"], None),
            (&["http://example", "stun:", "stun:host:notaport"], None),
        ]));
        assert!(stun.is_empty() && turn.is_empty(), "{stun:?} {turn:?}");
    }

    #[test]
    fn the_local_ip_is_a_candidate_address() {
        let ip = local_ip();
        assert!(!ip.is_unspecified() && !ip.is_multicast(), "{ip}");
    }

    #[test]
    fn a_binding_request_is_a_stun_request() {
        let request = binding_request([7; 12]);
        assert_eq!(request.len(), 20);
        assert_eq!(&request[..4], &[0x00, 0x01, 0x00, 0x00]);
        assert_eq!(&request[4..8], &MAGIC_COOKIE.to_be_bytes());
        assert_eq!(&request[8..], &[7; 12]);
    }

    /// A binding success response with an XOR-MAPPED-ADDRESS of 192.0.2.10:40000 after an
    /// unknown attribute.
    fn response(transaction: [u8; 12]) -> Vec<u8> {
        let mut packet = vec![0x01, 0x01, 0, 20];
        packet.extend(MAGIC_COOKIE.to_be_bytes());
        packet.extend(transaction);
        // SOFTWARE (0x8022), 3 bytes padded to 4.
        packet.extend([0x80, 0x22, 0, 3, b'a', b'b', b'c', 0]);
        let port = 40_000u16 ^ (MAGIC_COOKIE >> 16) as u16;
        let ip = u32::from(Ipv4Addr::new(192, 0, 2, 10)) ^ MAGIC_COOKIE;
        packet.extend([0x00, 0x20, 0, 8, 0, 0x01]);
        packet.extend(port.to_be_bytes());
        packet.extend(ip.to_be_bytes());
        packet
    }

    #[test]
    fn the_mapped_address_is_read_from_the_response() {
        assert_eq!(
            binding_response(&response([9; 12]), [9; 12]),
            Some("192.0.2.10:40000".parse().unwrap())
        );
    }

    #[test]
    fn other_packets_are_not_binding_responses() {
        assert_eq!(binding_response(&response([9; 12]), [8; 12]), None);
        assert_eq!(binding_response(&response([9; 12])[..30], [9; 12]), None);
        assert_eq!(binding_response(&[0x80, 0x60, 0, 1], [9; 12]), None);
        let mut request = binding_request([9; 12]);
        request[3] = 0;
        assert_eq!(binding_response(&request, [9; 12]), None);
    }
}
