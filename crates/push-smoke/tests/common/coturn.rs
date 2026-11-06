//! The chart's pinned coturn in Docker, for the calls tests that relay media
//! (`browser_calls.rs`, `calls_media.rs`): the chart's `turnserver.conf` with every
//! denied range, a static REST secret, and one TEST-ONLY exception.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use testcontainers_modules::testcontainers::core::{ContainerPort, WaitFor};
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, GenericImage, ImageExt};

/// The running container and its published ports on the loopback.
pub struct Coturn {
    pub container: ContainerAsync<GenericImage>,
    pub tcp_port: u16,
    pub udp_port: u16,
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn chart_file(name: &str) -> String {
    let path = repo().join("deploy/helm/push-messenger").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
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

/// The chart's coturn configuration with every denied range, plus one TEST-ONLY
/// exception: Docker's bridge range (172.16.0.0/12) is allowed as a peer. Clients reach
/// coturn through ports published on the loopback, so each one's relay address is
/// coturn's own address on the Docker bridge, which the chart denies (RFC 1918). A
/// deployment never sets allowed-peer-ip.
fn config(secret: &str) -> String {
    let mut config = chart_file("files/coturn/turnserver.conf");
    config.push_str(&format!(
        "realm=push.test\nmin-port=49160\nmax-port=49200\nuser-quota=12\ntotal-quota=1200\n\
         max-bps=512000\nno-tls\nstatic-auth-secret={secret}\n"
    ));
    let denied = chart_file("files/coturn/denied-peers.txt");
    for cidr in denied
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        config.push_str(&format!("denied-peer-ip={}\n", range(cidr)));
    }
    config.push_str("allowed-peer-ip=172.16.0.0-172.31.255.255\n");
    config
}

/// Starts the chart's pinned coturn image with the REST secret `secret`, port 3478
/// published over TCP and UDP.
pub async fn start(secret: &str) -> Coturn {
    let values = chart_file("values.yaml");
    let image = values
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("image: "))
        .find(|image| image.contains("coturn"))
        .expect("calls.turn.internal.image in values.yaml")
        .trim();
    let (name, digest) = image.split_once('@').expect("pinned by digest");
    let (repository, tag) = name.rsplit_once(':').expect("<repository>:<tag>@<digest>");
    let container = GenericImage::new(repository.to_owned(), format!("{tag}@{digest}"))
        .with_exposed_port(ContainerPort::Tcp(3478))
        .with_exposed_port(ContainerPort::Udp(3478))
        .with_wait_for(WaitFor::Nothing)
        .with_copy_to("/etc/coturn/push-test.conf", config(secret).into_bytes())
        .with_cmd(["-c", "/etc/coturn/push-test.conf"])
        .start()
        .await
        .expect("start a coturn container (is Docker running?)");
    let tcp_port = container
        .get_host_port_ipv4(ContainerPort::Tcp(3478))
        .await
        .unwrap();
    let udp_port = container
        .get_host_port_ipv4(ContainerPort::Udp(3478))
        .await
        .unwrap();
    Coturn {
        container,
        tcp_port,
        udp_port,
    }
}
