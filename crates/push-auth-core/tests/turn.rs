//! TURN REST credentials minted by push-auth-core against a real coturn (Docker must be
//! running), started with the chart's pinned image, `files/coturn/turnserver.conf` and
//! every range of `files/coturn/denied-peers.txt` (spec 2026-11-03 voice calls, T5, T6):
//! minted credentials allocate a relay, expired or forged ones are refused, and the relay
//! refuses permissions for private, loopback, link-local and metadata peers.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use push_auth_core::turn::{rest_credential, rest_username};
use sha1::Sha1;
use testcontainers_modules::testcontainers::core::{ContainerPort, WaitFor};
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uuid::Uuid;

const SECRET: &str = "the-coturn-shared-secret-of-this-test";
const REALM: &str = "push.test";
/// A public range the test denies as if it were a cluster CIDR (deniedPeerCidrs).
const CLUSTER_CIDR: &str = "93.184.216.0/24";

const MAGIC: u32 = 0x2112_a442;
const BINDING: u16 = 0x0001;
const ALLOCATE: u16 = 0x0003;
const CREATE_PERMISSION: u16 = 0x0008;
const SUCCESS: u16 = 0x0100;
const ERROR: u16 = 0x0110;
const USERNAME: u16 = 0x0006;
const MESSAGE_INTEGRITY: u16 = 0x0008;
const ERROR_CODE: u16 = 0x0009;
const XOR_PEER_ADDRESS: u16 = 0x0012;
const REALM_ATTRIBUTE: u16 = 0x0014;
const NONCE: u16 = 0x0015;
const REQUESTED_TRANSPORT: u16 = 0x0019;

fn chart_file(name: &str) -> String {
    let path = format!(
        "{}/../../deploy/helm/push-messenger/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// The chart's pinned coturn image: `(repository, tag@digest)`.
fn pinned_image() -> (String, String) {
    let values = chart_file("values.yaml");
    let reference = values
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("image: coturn/coturn:"))
        .expect("calls.turn.internal.image in values.yaml");
    assert!(
        reference.contains("@sha256:"),
        "pinned by digest: {reference}"
    );
    ("coturn/coturn".to_owned(), reference.trim().to_owned())
}

/// `first-last` of a CIDR, as coturn's denied-peer-ip wants it.
fn range(cidr: &str) -> String {
    let (address, prefix) = cidr.split_once('/').expect("a CIDR");
    let prefix: u32 = prefix.parse().unwrap();
    match address.parse::<IpAddr>().unwrap() {
        IpAddr::V4(address) => {
            let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
            let first = u32::from(address) & mask;
            format!(
                "{}-{}",
                Ipv4Addr::from(first),
                Ipv4Addr::from(first | !mask)
            )
        }
        IpAddr::V6(address) => {
            let mask = u128::MAX.checked_shl(128 - prefix).unwrap_or(0);
            let first = u128::from(address) & mask;
            format!(
                "{}-{}",
                Ipv6Addr::from(first),
                Ipv6Addr::from(first | !mask)
            )
        }
    }
}

/// The chart's configuration with the test's realm, ports, quotas and secret.
fn config() -> String {
    let mut config = chart_file("files/coturn/turnserver.conf");
    config.push_str(&format!(
        "realm={REALM}\nmin-port=49160\nmax-port=49200\nuser-quota=12\ntotal-quota=1200\n\
         max-bps=512000\nno-tls\nstatic-auth-secret={SECRET}\n"
    ));
    let denied = chart_file("files/coturn/denied-peers.txt");
    let cidrs = denied
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .chain([CLUSTER_CIDR]);
    for cidr in cidrs {
        config.push_str(&format!("denied-peer-ip={}\n", range(cidr)));
    }
    config
}

async fn coturn() -> (ContainerAsync<GenericImage>, u16) {
    let (repository, tag) = pinned_image();
    let container = GenericImage::new(repository, tag)
        .with_exposed_port(ContainerPort::Tcp(3478))
        .with_wait_for(WaitFor::Nothing)
        .with_copy_to("/etc/coturn/push-test.conf", config().into_bytes())
        .with_cmd(["-c", "/etc/coturn/push-test.conf"])
        .start()
        .await
        .expect("start a coturn container (is Docker running?)");
    let port = container
        .get_host_port_ipv4(ContainerPort::Tcp(3478))
        .await
        .unwrap();
    // Ready once it answers a STUN binding over TCP.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)).await {
            let answered = tokio::time::timeout(
                Duration::from_secs(2),
                exchange(&mut stream, BINDING, &[], None),
            )
            .await;
            if matches!(answered, Ok(Ok((kind, _))) if kind == BINDING | SUCCESS) {
                return (container, port);
            }
        }
        assert!(Instant::now() < deadline, "coturn did not start");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

type Attributes = Vec<(u16, Vec<u8>)>;

fn encode(
    kind: u16,
    transaction: &[u8; 12],
    attributes: &[(u16, Vec<u8>)],
    key: Option<&[u8]>,
) -> Vec<u8> {
    let mut body = Vec::new();
    for (kind, value) in attributes {
        body.extend_from_slice(&kind.to_be_bytes());
        body.extend_from_slice(&(value.len() as u16).to_be_bytes());
        body.extend_from_slice(value);
        body.resize(body.len().next_multiple_of(4), 0);
    }
    let length = body.len() + if key.is_some() { 24 } else { 0 };
    let mut message = Vec::new();
    message.extend_from_slice(&kind.to_be_bytes());
    message.extend_from_slice(&(length as u16).to_be_bytes());
    message.extend_from_slice(&MAGIC.to_be_bytes());
    message.extend_from_slice(transaction);
    message.extend_from_slice(&body);
    if let Some(key) = key {
        let mut mac = Hmac::<Sha1>::new_from_slice(key).unwrap();
        mac.update(&message);
        message.extend_from_slice(&MESSAGE_INTEGRITY.to_be_bytes());
        message.extend_from_slice(&20u16.to_be_bytes());
        message.extend_from_slice(&mac.finalize().into_bytes());
    }
    message
}

/// Sends one request over the TCP control connection and reads the answer.
async fn exchange(
    stream: &mut TcpStream,
    kind: u16,
    attributes: &[(u16, Vec<u8>)],
    key: Option<&[u8]>,
) -> std::io::Result<(u16, Attributes)> {
    let transaction: [u8; 12] = Uuid::new_v4().as_bytes()[..12].try_into().unwrap();
    stream
        .write_all(&encode(kind, &transaction, attributes, key))
        .await?;
    let mut header = [0u8; 20];
    stream.read_exact(&mut header).await?;
    let length = u16::from_be_bytes([header[2], header[3]]) as usize;
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).await?;
    assert_eq!(&header[8..20], &transaction, "the answer to this request");
    let mut parsed = Vec::new();
    let mut at = 0;
    while at + 4 <= body.len() {
        let kind = u16::from_be_bytes([body[at], body[at + 1]]);
        let size = u16::from_be_bytes([body[at + 2], body[at + 3]]) as usize;
        parsed.push((kind, body[at + 4..at + 4 + size].to_vec()));
        at += (4 + size).next_multiple_of(4);
    }
    Ok((u16::from_be_bytes([header[0], header[1]]), parsed))
}

fn attribute(attributes: &Attributes, kind: u16) -> Option<&[u8]> {
    attributes
        .iter()
        .find(|(found, _)| *found == kind)
        .map(|(_, value)| value.as_slice())
}

fn error_code(attributes: &Attributes) -> u16 {
    let value = attribute(attributes, ERROR_CODE).expect("an ERROR-CODE");
    u16::from(value[2] & 0x07) * 100 + u16::from(value[3])
}

/// A TURN client session on one TCP connection, after a successful allocation.
struct Session {
    stream: TcpStream,
    username: String,
    nonce: Vec<u8>,
    key: Vec<u8>,
}

impl Session {
    /// Allocates a UDP relay with `username` and `password`; the error code on refusal.
    async fn allocate(port: u16, username: &str, password: &str) -> Result<Self, u16> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let transport = vec![17, 0, 0, 0];
        let (kind, challenge) = exchange(
            &mut stream,
            ALLOCATE,
            &[(REQUESTED_TRANSPORT, transport.clone())],
            None,
        )
        .await
        .unwrap();
        assert_eq!(kind, ALLOCATE | ERROR);
        assert_eq!(error_code(&challenge), 401, "a challenge first");
        let realm = attribute(&challenge, REALM_ATTRIBUTE).unwrap().to_vec();
        assert_eq!(realm, REALM.as_bytes());
        let nonce = attribute(&challenge, NONCE).unwrap().to_vec();
        let key = Md5::digest(format!("{username}:{REALM}:{password}")).to_vec();
        let mut session = Self {
            stream,
            username: username.to_owned(),
            nonce,
            key,
        };
        let (kind, answer) = session
            .authenticated(ALLOCATE, vec![(REQUESTED_TRANSPORT, transport)])
            .await;
        if kind == ALLOCATE | SUCCESS {
            Ok(session)
        } else {
            Err(error_code(&answer))
        }
    }

    async fn authenticated(
        &mut self,
        kind: u16,
        mut attributes: Vec<(u16, Vec<u8>)>,
    ) -> (u16, Attributes) {
        attributes.extend([
            (USERNAME, self.username.clone().into_bytes()),
            (REALM_ATTRIBUTE, REALM.as_bytes().to_vec()),
            (NONCE, self.nonce.clone()),
        ]);
        let key = self.key.clone();
        exchange(&mut self.stream, kind, &attributes, Some(&key))
            .await
            .unwrap()
    }

    /// Asks for a permission to relay to `peer`: `Ok` or the error code.
    async fn permit(&mut self, peer: Ipv4Addr) -> Result<(), u16> {
        let magic = MAGIC.to_be_bytes();
        let mut value = vec![0, 0x01];
        value.extend_from_slice(&(5000u16 ^ (MAGIC >> 16) as u16).to_be_bytes());
        value.extend(
            peer.octets()
                .iter()
                .zip(magic)
                .map(|(byte, mask)| byte ^ mask),
        );
        let (kind, answer) = self
            .authenticated(CREATE_PERMISSION, vec![(XOR_PEER_ADDRESS, value)])
            .await;
        if kind == CREATE_PERMISSION | SUCCESS {
            Ok(())
        } else {
            Err(error_code(&answer))
        }
    }
}

fn credentials(expires_at: i64) -> (String, String) {
    let username = rest_username(expires_at, Uuid::now_v7());
    let password = rest_credential(SECRET, &username);
    (username, password)
}

#[tokio::test]
async fn coturn_accepts_minted_credentials_and_denies_private_peers() {
    let (container, port) = coturn().await;
    // Every option of the chart's file is one this coturn knows (an upgrade that drops an
    // option shows here, not as a silently weaker relay).
    let logs = String::from_utf8_lossy(&container.stdout_to_vec().await.unwrap()).into_owned();
    for refused in ["Bad configuration format", "ERROR", "CONFIG ERROR"] {
        assert!(!logs.contains(refused), "coturn refused an option: {logs}");
    }
    let now = chrono::Utc::now().timestamp();

    // Minted credentials allocate a relay.
    let (username, password) = credentials(now + 600);
    let mut session = Session::allocate(port, &username, &password)
        .await
        .expect("minted credentials are accepted");

    // Public peers are allowed; private, loopback, link-local, metadata, reserved and the
    // configured cluster ranges are not (403 Forbidden).
    session
        .permit(Ipv4Addr::new(8, 8, 8, 8))
        .await
        .expect("a public peer is allowed");
    for peer in [
        "10.0.0.1",
        "10.244.1.7",
        "127.0.0.1",
        "169.254.169.254",
        "100.64.0.1",
        "100.100.100.200",
        "172.16.5.4",
        "192.168.1.1",
        "192.0.0.192",
        "168.63.129.16",
        "0.0.0.1",
        "240.0.0.1",
        "93.184.216.34",
    ] {
        assert_eq!(
            session.permit(peer.parse().unwrap()).await,
            Err(403),
            "{peer} must be denied"
        );
    }

    // Expired credentials, another secret and a credential for another user are refused.
    let (username, password) = credentials(now - 1);
    assert_eq!(
        Session::allocate(port, &username, &password).await.err(),
        Some(401),
        "expired"
    );
    let username = rest_username(now + 600, Uuid::now_v7());
    let forged = rest_credential("another-secret-entirely-0123456789", &username);
    assert_eq!(
        Session::allocate(port, &username, &forged).await.err(),
        Some(401),
        "another secret"
    );
    let (_, password) = credentials(now + 600);
    let other_user = rest_username(now + 600, Uuid::now_v7());
    assert_eq!(
        Session::allocate(port, &other_user, &password).await.err(),
        Some(401),
        "another user's credential"
    );
}

#[tokio::test]
async fn coturn_limits_allocations_per_user_across_credentials() {
    let (_container, port) = coturn().await;
    let now = chrono::Utc::now().timestamp();
    let user = Uuid::now_v7();
    // user-quota 12: the 13th concurrent allocation of one user is refused (486), also
    // with credentials of different expiries; another user is not affected.
    let mut sessions = Vec::new();
    for second in 0..12 {
        let username = rest_username(now + 600 - second, user);
        let password = rest_credential(SECRET, &username);
        sessions.push(
            Session::allocate(port, &username, &password)
                .await
                .expect("within the quota"),
        );
    }
    let username = rest_username(now + 300, user);
    let password = rest_credential(SECRET, &username);
    assert_eq!(
        Session::allocate(port, &username, &password).await.err(),
        Some(486),
        "the user's quota is reached"
    );
    let (username, password) = credentials(now + 600);
    Session::allocate(port, &username, &password)
        .await
        .expect("another user has a quota of their own");
    drop(sessions);
}
