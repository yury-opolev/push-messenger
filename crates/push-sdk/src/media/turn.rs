//! A TURN allocation (RFC 8656) over UDP or TCP, with `turn-client-proto` doing the
//! protocol: the relayed address for the relay candidate, permissions for the peers, and
//! the wrapping of the media to and from them.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use turn_client_proto::api::{Transmit, TurnClientApi, TurnConfig, TurnEvent, TurnPollRet};
use turn_client_proto::client::{TurnClient, TurnRecvRet};
use turn_client_proto::tcp::TurnClientTcp;
use turn_client_proto::types::{TransportType, TurnCredentials};
use turn_client_proto::udp::TurnClientUdp;

use super::ice::{Transport, TurnServer};

/// Packets held per peer while its permission is being created.
const MAX_WAITING: usize = 64;

enum Link {
    Udp(UdpSocket),
    Tcp(TcpStream),
}

pub(crate) struct Relay {
    client: TurnClient,
    link: Link,
    server: SocketAddr,
    local: SocketAddr,
    /// The allocation's address on the TURN server: the relay candidate.
    pub(crate) relayed: SocketAddr,
    /// The zero of the TURN client's clock.
    epoch: Instant,
    waiting: HashMap<IpAddr, Vec<(SocketAddr, Vec<u8>)>>,
    /// The allocation's outcome once known: its address, or `None` when refused.
    allocation: Option<Option<SocketAddr>>,
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Relay").finish_non_exhaustive()
    }
}

impl Relay {
    /// Allocates a relayed address on `server` (resolved to its IPv4 addresses). The
    /// caller bounds the time it may take.
    pub(crate) async fn allocate(server: &TurnServer) -> Result<Self, String> {
        let address =
            tokio::net::lookup_host((server.endpoint.host.as_str(), server.endpoint.port))
                .await
                .map_err(|error| format!("resolving the TURN server: {error}"))?
                .find(SocketAddr::is_ipv4)
                .ok_or("the TURN server has no IPv4 address")?;
        let mut config =
            TurnConfig::new(TurnCredentials::new(&server.username, &server.credential));
        config.set_allocation_transport(TransportType::Udp);
        let (link, local, client): (Link, SocketAddr, TurnClient) = match server.transport {
            Transport::Udp => {
                let socket = UdpSocket::bind("0.0.0.0:0")
                    .await
                    .map_err(|error| error.to_string())?;
                socket
                    .connect(address)
                    .await
                    .map_err(|error| error.to_string())?;
                let local = socket.local_addr().map_err(|error| error.to_string())?;
                let client = TurnClientUdp::allocate(local, address, config).into();
                (Link::Udp(socket), local, client)
            }
            Transport::Tcp => {
                let stream = TcpStream::connect(address)
                    .await
                    .map_err(|error| error.to_string())?;
                stream
                    .set_nodelay(true)
                    .map_err(|error| error.to_string())?;
                let local = stream.local_addr().map_err(|error| error.to_string())?;
                let client = TurnClientTcp::allocate(local, address, config).into();
                (Link::Tcp(stream), local, client)
            }
        };
        let mut relay = Self {
            client,
            link,
            server: address,
            local,
            relayed: local,
            epoch: Instant::now(),
            waiting: HashMap::new(),
            allocation: None,
        };
        let mut buffer = vec![0u8; 2_048];
        loop {
            relay.flush().await.map_err(|error| error.to_string())?;
            match relay.allocation {
                Some(Some(relayed)) => {
                    relay.relayed = relayed;
                    return Ok(relay);
                }
                Some(None) => return Err("the TURN server refused the allocation".to_owned()),
                None => {}
            }
            let wake = match relay.client.poll(relay.now()) {
                TurnPollRet::WaitUntil(at) => at.to_std(relay.epoch),
                TurnPollRet::Closed => return Err("the TURN allocation failed".to_owned()),
                _ => Instant::now() + Duration::from_millis(50),
            };
            tokio::select! {
                read = relay.read(&mut buffer) => {
                    let length = read.map_err(|error| error.to_string())?;
                    let _ = relay.input(&buffer[..length]);
                }
                () = tokio::time::sleep_until(wake.into()) => {}
            }
        }
    }

    /// This host's end of the link to the TURN server.
    pub(crate) fn local(&self) -> SocketAddr {
        self.local
    }

    fn now(&self) -> turn_client_proto::types::Instant {
        turn_client_proto::types::Instant::from_nanos(
            i64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(i64::MAX),
        )
    }

    fn transport(&self) -> TransportType {
        match self.link {
            Link::Udp(_) => TransportType::Udp,
            Link::Tcp(_) => TransportType::Tcp,
        }
    }

    /// Reads what the TURN server sent: a datagram, or the next bytes of the stream.
    /// Fails when the stream closed.
    pub(crate) async fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match &mut self.link {
            Link::Udp(socket) => loop {
                match socket.recv(buffer).await {
                    // An ICMP error for an earlier datagram: the link itself stays up.
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset
                        ) => {}
                    read => return read,
                }
            },
            Link::Tcp(stream) => match stream.read(buffer).await? {
                0 => Err(io::ErrorKind::UnexpectedEof.into()),
                length => Ok(length),
            },
        }
    }

    /// What `read` returned: the peers' data in it, `(peer, data)`.
    pub(crate) fn input(&mut self, bytes: &[u8]) -> Vec<(SocketAddr, Vec<u8>)> {
        let now = self.now();
        let transmit = Transmit::new(bytes, self.transport(), self.server, self.local);
        let mut out = Vec::new();
        if let TurnRecvRet::PeerData(data) = self.client.recv(transmit, now) {
            out.push((data.peer, data.data().to_vec()));
        }
        while let Some(data) = self.client.poll_recv(now) {
            out.push((data.peer, data.data().to_vec()));
        }
        out
    }

    /// When the TURN client next needs time (refreshes, retransmits).
    pub(crate) fn deadline(&mut self) -> Option<Instant> {
        match self.client.poll(self.now()) {
            TurnPollRet::WaitUntil(at) => Some(at.to_std(self.epoch)),
            _ => None,
        }
    }

    /// Sends `data` to `peer` from the relayed address, creating the peer's permission
    /// first if needed (the data waits for it).
    pub(crate) async fn send(&mut self, peer: SocketAddr, data: &[u8]) -> io::Result<()> {
        let now = self.now();
        if !self.client.have_permission(TransportType::Udp, peer.ip()) {
            let waiting = self.waiting.entry(peer.ip()).or_default();
            if waiting.is_empty() {
                // An error means one is being created already.
                let _ = self
                    .client
                    .create_permission(TransportType::Udp, peer.ip(), now);
            }
            if waiting.len() < MAX_WAITING {
                waiting.push((peer, data.to_vec()));
            }
            return self.flush().await;
        }
        if let Ok(Some(transmit)) = self.client.send_to(TransportType::Udp, peer, data, now) {
            let transmit = transmit.build();
            self.write(&transmit.data).await?;
        }
        self.flush().await
    }

    /// Sends what the TURN client has to send, and the data that waited for a permission
    /// now installed.
    pub(crate) async fn flush(&mut self) -> io::Result<()> {
        let now = self.now();
        while let Some(transmit) = self.client.poll_transmit(now) {
            self.write(&transmit.data).await?;
        }
        let mut ready = Vec::new();
        while let Some(event) = self.client.poll_event() {
            match event {
                TurnEvent::PermissionCreated(_, ip) => ready.push(ip),
                TurnEvent::PermissionCreateFailed(_, ip) => {
                    self.waiting.remove(&ip);
                }
                TurnEvent::AllocationCreated(_, relayed) => self.allocation = Some(Some(relayed)),
                TurnEvent::AllocationCreateFailed(_) => {
                    self.allocation.get_or_insert(None);
                }
                _ => {}
            }
        }
        for ip in ready {
            for (peer, data) in self.waiting.remove(&ip).unwrap_or_default() {
                if let Ok(Some(transmit)) = self.client.send_to(TransportType::Udp, peer, data, now)
                {
                    let transmit = transmit.build();
                    self.write(&transmit.data).await?;
                }
            }
        }
        Ok(())
    }

    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        match &mut self.link {
            Link::Udp(socket) => socket.send(bytes).await.map(drop),
            Link::Tcp(stream) => stream.write_all(bytes).await,
        }
    }

    /// Releases the allocation (best effort, at the end of the call).
    pub(crate) async fn release(&mut self) {
        let now = self.now();
        if self.client.delete(now).is_ok() {
            let _ = self.flush().await;
        }
    }
}
