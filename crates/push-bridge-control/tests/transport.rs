//! The per-user transport: a real pipe (Windows) or socket (Unix) per test, each with its
//! own name so tests can run in parallel.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use push_bridge_control::Endpoint;
use push_bridge_control::endpoint_for;
use push_bridge_control::transport::{Listener, Stream, connect, connect_same_user};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// A unique endpoint; on Unix under its own runtime dir in the system temp folder.
struct Scratch {
    endpoint: Endpoint,
    #[cfg(unix)]
    root: std::path::PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let id = format!(
            "test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        );
        #[cfg(windows)]
        {
            Scratch {
                endpoint: endpoint_for(&id, None).unwrap(),
            }
        }
        #[cfg(unix)]
        {
            let root = std::env::temp_dir().join(format!("pbc-{id}"));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Scratch {
                endpoint: endpoint_for(&id, Some(root.as_os_str())).unwrap(),
                root,
            }
        }
    }

    #[cfg(unix)]
    fn socket(&self) -> &std::path::Path {
        match &self.endpoint {
            Endpoint::Socket(path) => path,
            Endpoint::Pipe(_) => unreachable!(),
        }
    }
}

#[cfg(unix)]
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const WAIT: Duration = Duration::from_secs(10);

/// Serves `count` clients: each line it reads goes back. Returns the listener's refusals.
fn echo_server(mut listener: Listener, count: usize) -> tokio::task::JoinHandle<u64> {
    tokio::spawn(async move {
        for _ in 0..count {
            let stream = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            reader.get_mut().write_all(line.as_bytes()).await.unwrap();
            reader.get_mut().flush().await.unwrap();
        }
        listener.refused()
    })
}

async fn round_trip(stream: Stream, text: &str) -> String {
    let mut reader = BufReader::new(stream);
    reader
        .get_mut()
        .write_all(format!("{text}\n").as_bytes())
        .await
        .unwrap();
    reader.get_mut().flush().await.unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    line
}

#[tokio::test]
async fn a_line_goes_to_the_server_and_back() {
    let scratch = Scratch::new();
    let server = echo_server(Listener::bind(&scratch.endpoint).unwrap(), 2);
    for text in ["ping", "pong"] {
        let stream = timeout(WAIT, connect(&scratch.endpoint))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            timeout(WAIT, round_trip(stream, text)).await.unwrap(),
            format!("{text}\n")
        );
    }
    assert_eq!(timeout(WAIT, server).await.unwrap().unwrap(), 0);
}

#[tokio::test]
async fn an_endpoint_binds_once() {
    let scratch = Scratch::new();
    let _first = Listener::bind(&scratch.endpoint).unwrap();
    let error = Listener::bind(&scratch.endpoint).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::AddrInUse, "{error}");
}

#[tokio::test]
async fn nobody_listening_is_not_found() {
    let scratch = Scratch::new();
    let error = connect(&scratch.endpoint).await.err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
    let error = connect_same_user(&scratch.endpoint).await.err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
}

#[tokio::test]
async fn a_server_of_this_user_passes_connect_same_user() {
    let scratch = Scratch::new();
    let server = echo_server(Listener::bind(&scratch.endpoint).unwrap(), 1);
    let stream = timeout(WAIT, connect_same_user(&scratch.endpoint))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        timeout(WAIT, round_trip(stream, "same")).await.unwrap(),
        "same\n"
    );
    assert_eq!(timeout(WAIT, server).await.unwrap().unwrap(), 0);
}

#[cfg(windows)]
#[tokio::test]
async fn the_pipe_admits_only_this_user_and_system() {
    use push_bridge_control::transport::{current_user_sid, pipe_sddl};

    assert_eq!(
        pipe_sddl("S-1-5-21-1"),
        "D:P(A;;GA;;;S-1-5-21-1)(A;;GA;;;SY)"
    );
    let scratch = Scratch::new();
    let listener = Listener::bind(&scratch.endpoint).unwrap();
    let sddl = listener.dacl_sddl().unwrap();
    let sid = current_user_sid().unwrap();
    assert!(sid.starts_with("S-1-5-"), "{sid}");
    assert!(sddl.starts_with("D:P("), "{sddl}");
    assert_eq!(sddl.matches("(A;").count(), 2, "{sddl}");
    assert_eq!(sddl.matches('(').count(), 2, "{sddl}");
    assert!(sddl.contains(&format!(";;;{sid})")), "{sddl}");
    assert!(sddl.contains(";;;SY)"), "{sddl}");
}

#[cfg(windows)]
#[tokio::test]
async fn a_client_of_the_same_user_passes_the_peer_check() {
    let scratch = Scratch::new();
    let mut listener = Listener::bind(&scratch.endpoint).unwrap();
    let (accepted, connected) = tokio::join!(
        timeout(WAIT, listener.accept()),
        timeout(WAIT, connect(&scratch.endpoint))
    );
    accepted.unwrap().unwrap();
    connected.unwrap().unwrap();
    assert_eq!(listener.refused(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn the_socket_is_0600_in_a_0700_dir() {
    use std::os::unix::fs::PermissionsExt;

    let scratch = Scratch::new();
    let _listener = Listener::bind(&scratch.endpoint).unwrap();
    let socket = scratch.socket();
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(socket), 0o600);
    assert_eq!(mode(socket.parent().unwrap()), 0o700);
}

#[cfg(unix)]
#[tokio::test]
async fn a_stale_socket_file_is_replaced() {
    use std::os::unix::fs::DirBuilderExt;

    let scratch = Scratch::new();
    let socket = scratch.socket();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(socket.parent().unwrap())
        .unwrap();
    drop(std::os::unix::net::UnixListener::bind(socket).unwrap());
    assert!(socket.exists());
    let error = connect(&scratch.endpoint).await.err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");

    let server = echo_server(Listener::bind(&scratch.endpoint).unwrap(), 1);
    let stream = timeout(WAIT, connect(&scratch.endpoint))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        timeout(WAIT, round_trip(stream, "hi")).await.unwrap(),
        "hi\n"
    );
    assert_eq!(timeout(WAIT, server).await.unwrap().unwrap(), 0);
    assert!(
        !socket.exists(),
        "the listener removes its socket when dropped"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_shared_socket_dir_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    let scratch = Scratch::new();
    let dir = scratch.socket().parent().unwrap();
    std::fs::create_dir(dir).unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let error = Listener::bind(&scratch.endpoint).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
}
