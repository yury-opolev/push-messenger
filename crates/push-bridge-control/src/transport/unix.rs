//! The Unix socket: 0600 in a 0700 dir, and a peer uid check both ways.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

use super::{Stream, wrong_endpoint};
use crate::endpoint::Endpoint;

pub(super) struct Listener {
    listener: UnixListener,
    path: PathBuf,
    /// The uid that owns the runtime dir: the only peer admitted.
    uid: u32,
    pub(super) refused: u64,
}

impl Listener {
    pub(super) fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        let path = socket_path(endpoint)?;
        let (dir, uid) = socket_dir(path)?;
        prepare_dir(dir, uid)?;
        if fs::symlink_metadata(path).is_ok() {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("{} is in use", path.display()),
                ));
            }
            fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(Listener {
            listener,
            path: path.to_path_buf(),
            uid,
            refused: 0,
        })
    }

    pub(super) async fn accept(&mut self) -> io::Result<Stream> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let admitted = stream.peer_cred().is_ok_and(|cred| cred.uid() == self.uid);
            if admitted {
                return Ok(Box::new(stream));
            }
            self.refused += 1;
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(super) async fn connect(endpoint: &Endpoint) -> io::Result<Stream> {
    let path = socket_path(endpoint)?;
    let stream = UnixStream::connect(path)
        .await
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                io::Error::new(io::ErrorKind::NotFound, error)
            }
            _ => error,
        })?;
    let (_, uid) = socket_dir(path)?;
    if stream.peer_cred()?.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the socket's server runs as another user",
        ));
    }
    Ok(Box::new(stream))
}

fn socket_path(endpoint: &Endpoint) -> io::Result<&Path> {
    match endpoint {
        Endpoint::Socket(path) => Ok(path),
        Endpoint::Pipe(_) => Err(wrong_endpoint()),
    }
}

/// The socket's dir and the uid that owns the runtime dir above it.
fn socket_dir(path: &Path) -> io::Result<(&Path, u32)> {
    let dir = path.parent().ok_or_else(wrong_endpoint)?;
    let runtime = dir.parent().ok_or_else(wrong_endpoint)?;
    Ok((dir, fs::metadata(runtime)?.uid()))
}

/// Creates the socket's dir 0700, or checks an existing one is ours and private.
fn prepare_dir(dir: &Path, uid: u32) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{} must be a dir of the runtime dir's owner with mode 0700",
                        dir.display()
                    ),
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        }
        Err(error) => Err(error),
    }
}
