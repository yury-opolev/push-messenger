//! The per-user transport: the bridge listens, the tray connects, and each side checks
//! that the other runs as the same user. Windows: a local named pipe whose DACL admits
//! only the user and SYSTEM, plus a peer SID check. Unix: a 0600 socket in a 0700 dir,
//! plus a peer uid check.

use std::io;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::endpoint::Endpoint;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as imp;
#[cfg(windows)]
use windows as imp;

#[cfg(windows)]
pub use windows::{current_user_sid, pipe_sddl};

/// A stream both ways.
pub trait AsyncReadWrite: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin + ?Sized> AsyncReadWrite for T {}

/// A connected peer stream (server or client side).
pub type Stream = Box<dyn AsyncReadWrite>;

/// The server side of an endpoint.
pub struct Listener {
    inner: imp::Listener,
}

impl Listener {
    /// Binds the endpoint for the current user only.
    ///
    /// Windows: the first pipe instance of that name (an existing pipe, i.e. another
    /// bridge on the same folder or a squatter, is [`io::ErrorKind::AddrInUse`]), remote
    /// clients rejected, DACL `D:P(A;;GA;;;<user SID>)(A;;GA;;;SY)`. Unix: creates the
    /// socket's dir 0700 (an existing dir not owned by the runtime dir's owner or open to
    /// group/other is [`io::ErrorKind::PermissionDenied`]), removes a stale socket file
    /// only when connecting to it fails (a live one is `AddrInUse`), binds, and makes the
    /// socket 0600.
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        imp::Listener::bind(endpoint).map(|inner| Listener { inner })
    }

    /// The next client that passed the peer check (Windows: the client process's user SID
    /// is ours or SYSTEM; Unix: the peer uid owns the runtime dir). Refused clients are
    /// dropped and counted in [`Listener::refused`]; `accept` keeps waiting.
    pub async fn accept(&mut self) -> io::Result<Stream> {
        self.inner.accept().await
    }

    /// How many clients failed the peer check and were dropped, for the caller's logs.
    pub fn refused(&self) -> u64 {
        self.inner.refused
    }

    /// The pipe's DACL as SDDL, for tests and diagnostics.
    #[cfg(windows)]
    pub fn dacl_sddl(&self) -> io::Result<String> {
        self.inner.dacl_sddl()
    }
}

/// Connects as a client and checks the server the same way the server checks clients.
/// Nobody listening is [`io::ErrorKind::NotFound`]; a server of another user is
/// [`io::ErrorKind::PermissionDenied`].
pub async fn connect(endpoint: &Endpoint) -> io::Result<Stream> {
    imp::connect(endpoint).await
}

/// Connects as a client to a server that runs as the current user (Windows: or SYSTEM),
/// for endpoints that are not a bridge's: on Unix the server's uid is compared with ours
/// rather than with the runtime dir's owner, so the socket may be anywhere. Errors as
/// [`connect`].
pub async fn connect_same_user(endpoint: &Endpoint) -> io::Result<Stream> {
    imp::connect_same_user(endpoint).await
}

fn wrong_endpoint() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "this endpoint kind is not available on this platform",
    )
}
