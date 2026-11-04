//! Connecting to a voice adapter that is already running: a named pipe (Windows) or a
//! Unix socket, served by a process of the same user (checked by
//! [`push_bridge_control::transport::connect_same_user`]).

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;

use push_bridge_control::Endpoint;
use push_bridge_control::transport::{self, Stream};

use crate::config::ConnectConfig;

/// The configured socket path may start with this; it is replaced by the variable's value.
const RUNTIME_DIR_PREFIX: &str = "$XDG_RUNTIME_DIR/";

/// Where `config` points: `pipe` → `\\.\pipe\<name>`; `socket` → the path, with a
/// leading `$XDG_RUNTIME_DIR/` expanded from the environment.
pub fn endpoint(config: &ConnectConfig) -> Result<Endpoint, String> {
    endpoint_for(config, std::env::var_os("XDG_RUNTIME_DIR").as_deref())
}

/// [`endpoint`] with the value of `XDG_RUNTIME_DIR` given.
pub fn endpoint_for(
    config: &ConnectConfig,
    runtime_dir: Option<&OsStr>,
) -> Result<Endpoint, String> {
    match config {
        ConnectConfig::Pipe(name) => Ok(Endpoint::Pipe(pipe_path(name))),
        ConnectConfig::Socket(path) => expand_runtime_dir(path, runtime_dir).map(Endpoint::Socket),
    }
}

fn pipe_path(name: &str) -> String {
    format!(r"\\.\pipe\{name}")
}

fn expand_runtime_dir(path: &str, runtime_dir: Option<&OsStr>) -> Result<PathBuf, String> {
    let Some(rest) = path.strip_prefix(RUNTIME_DIR_PREFIX) else {
        return Ok(PathBuf::from(path));
    };
    let Some(dir) = runtime_dir.filter(|dir| !dir.is_empty()) else {
        return Err("XDG_RUNTIME_DIR is not set".to_owned());
    };
    let mut expanded = OsString::from(dir);
    if !expanded.to_string_lossy().ends_with('/') {
        expanded.push("/");
    }
    expanded.push(rest);
    Ok(PathBuf::from(expanded))
}

/// What the owner configured, for errors: the pipe's full name or the socket's path.
fn display_name(config: &ConnectConfig) -> String {
    match config {
        ConnectConfig::Pipe(name) => pipe_path(name),
        ConnectConfig::Socket(path) => path.clone(),
    }
}

/// Connects to the adapter `config` names. Nobody listening is
/// [`io::ErrorKind::NotFound`] ("no voice adapter is listening on <name>"); a server of
/// another user is [`io::ErrorKind::PermissionDenied`] ("<name> belongs to another
/// user"); an unset `XDG_RUNTIME_DIR` is [`io::ErrorKind::InvalidInput`].
pub async fn connect(config: &ConnectConfig) -> io::Result<Stream> {
    let endpoint =
        endpoint(config).map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    transport::connect_same_user(&endpoint)
        .await
        .map_err(|error| friendly(config, error))
}

fn friendly(config: &ConnectConfig, error: io::Error) -> io::Error {
    let name = display_name(config);
    match error.kind() {
        io::ErrorKind::NotFound => io::Error::new(
            io::ErrorKind::NotFound,
            format!("no voice adapter is listening on {name}"),
        ),
        io::ErrorKind::PermissionDenied => io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{name} belongs to another user"),
        ),
        _ => error,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use push_bridge_control::transport::Listener;
    use push_voice_adapter_proto::{
        AsyncFrameReader, AsyncFrameWriter, AudioFrame, Frame, Stream as AudioStream,
    };
    use tokio::time::timeout;

    use super::*;

    const WAIT: Duration = Duration::from_secs(10);

    #[test]
    fn pipes_and_sockets_are_named() {
        assert_eq!(
            endpoint_for(&ConnectConfig::Pipe("coda-voice".into()), None),
            Ok(Endpoint::Pipe(r"\\.\pipe\coda-voice".into()))
        );
        assert_eq!(
            endpoint_for(&ConnectConfig::Socket("/run/push/voice.sock".into()), None),
            Ok(Endpoint::Socket("/run/push/voice.sock".into()))
        );
        let in_runtime_dir = ConnectConfig::Socket("$XDG_RUNTIME_DIR/push-voice.sock".into());
        for dir in ["/run/user/1000", "/run/user/1000/"] {
            assert_eq!(
                endpoint_for(&in_runtime_dir, Some(OsStr::new(dir))),
                Ok(Endpoint::Socket("/run/user/1000/push-voice.sock".into()))
            );
        }
        for unset in [None, Some(OsStr::new(""))] {
            assert_eq!(
                endpoint_for(&in_runtime_dir, unset),
                Err("XDG_RUNTIME_DIR is not set".to_owned())
            );
        }
    }

    /// A unique endpoint for a test listener and the config that names it; the guard
    /// keeps a Unix socket's folder.
    fn scratch() -> (ConnectConfig, Endpoint, Option<crate::test_dir::TestDir>) {
        let name = format!("push-voice-test-{}", uuid::Uuid::new_v4());
        if cfg!(windows) {
            let config = ConnectConfig::Pipe(name);
            let endpoint = endpoint(&config).unwrap();
            (config, endpoint, None)
        } else {
            let dir = crate::test_dir::TestDir::new();
            let path = dir.path().join("adapter").join("voice.sock");
            let config = ConnectConfig::Socket(path.to_string_lossy().into_owned());
            (config, Endpoint::Socket(path), Some(dir))
        }
    }

    fn audio(seq: u32) -> Frame {
        Frame::Audio(AudioFrame {
            stream: AudioStream::FromCaller,
            seq,
            timestamp_ms: seq * 20,
            samples: vec![7; 320],
        })
    }

    #[tokio::test]
    async fn frames_cross_both_ways() {
        let (config, endpoint, _dir) = scratch();
        let mut listener = Listener::bind(&endpoint).unwrap();
        let adapter = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let (read, write) = tokio::io::split(stream);
            let mut reader = AsyncFrameReader::new(read);
            let mut writer = AsyncFrameWriter::new(write);
            let frame = reader.read_frame().await.unwrap().unwrap();
            writer
                .write_frame(&Frame::Control(br#"{"type":"pong","id":1}"#.to_vec()))
                .await
                .unwrap();
            frame
        });
        let stream = timeout(WAIT, connect(&config)).await.unwrap().unwrap();
        let (read, write) = tokio::io::split(stream);
        let mut reader = AsyncFrameReader::new(read);
        let mut writer = AsyncFrameWriter::new(write);
        writer.write_frame(&audio(3)).await.unwrap();
        let reply = timeout(WAIT, reader.read_frame()).await.unwrap().unwrap();
        assert_eq!(
            reply,
            Some(Frame::Control(br#"{"type":"pong","id":1}"#.to_vec()))
        );
        assert_eq!(timeout(WAIT, adapter).await.unwrap().unwrap(), audio(3));
    }

    #[tokio::test]
    async fn nobody_listening_is_not_found() {
        let (config, _endpoint, _dir) = scratch();
        let Err(error) = connect(&config).await else {
            panic!("connected to nobody");
        };
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
        assert_eq!(
            error.to_string(),
            format!("no voice adapter is listening on {}", display_name(&config))
        );
    }
}
