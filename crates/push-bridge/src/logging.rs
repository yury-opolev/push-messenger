//! The bridge's log: JSON lines in `<dir>\logs\bridge.log`. Events carry names, codes,
//! counts and durations only, never bodies, previews, plans, tokens or assertions.

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing::Subscriber;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// The log file's name in `<dir>\logs`.
pub const FILE_NAME: &str = "bridge.log";
/// The environment variable with the log filter (`tracing_subscriber`'s `EnvFilter`
/// syntax); `info` when unset or invalid.
pub const FILTER_ENV: &str = "PUSH_BRIDGE_LOG";
/// A log larger than this at start is renamed to `bridge.log.1`, replacing the one before.
pub const ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// `<dir>\logs\bridge.log`.
pub fn path(dir: &Path) -> PathBuf {
    dir.join("logs").join(FILE_NAME)
}

/// The filter from [`FILTER_ENV`], or `info`.
pub fn filter() -> EnvFilter {
    EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new("info"))
}

/// The bridge's JSON-lines subscriber writing to `writer`.
pub fn subscriber<W>(writer: W, filter: EnvFilter) -> impl Subscriber + Send + Sync
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .json()
        .with_current_span(false)
        .with_span_list(false)
        .with_ansi(false)
        .with_writer(writer)
        .with_env_filter(filter)
        .finish()
}

/// Rotates the log if it is too large, then appends to it for the rest of the process.
/// Returns the log's path.
pub fn init(dir: &Path) -> io::Result<PathBuf> {
    let path = path(dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    rotate(&path, ROTATE_BYTES)?;
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    tracing::subscriber::set_global_default(subscriber(Mutex::new(file), filter()))
        .map_err(io::Error::other)?;
    Ok(path)
}

/// Renames `path` to `<path>.1` (replacing it) when `path` is larger than `limit` bytes.
fn rotate(path: &Path, limit: u64) -> io::Result<()> {
    let size = match std::fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if size <= limit {
        return Ok(());
    }
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".1");
    std::fs::rename(path, rotated)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::test_dir::TestDir;

    #[test]
    fn a_large_log_is_rotated_and_a_small_one_kept() {
        let dir = TestDir::new();
        let log = path(dir.path());
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let rotated = log.with_file_name("bridge.log.1");

        rotate(&log, 4).unwrap();
        assert!(!log.exists());

        std::fs::write(&log, "1234").unwrap();
        rotate(&log, 4).unwrap();
        assert!(log.exists() && !rotated.exists());

        std::fs::write(&rotated, "older").unwrap();
        std::fs::write(&log, "12345").unwrap();
        rotate(&log, 4).unwrap();
        assert!(!log.exists());
        assert_eq!(std::fs::read_to_string(&rotated).unwrap(), "12345");
    }

    #[test]
    fn events_are_json_lines() {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&buffer);
        let make = move || Buffer(Arc::clone(&writer));
        tracing::subscriber::with_default(subscriber(make, EnvFilter::new("info")), || {
            tracing::info!(event = "turn_ended", interrupted = false);
            tracing::debug!(event = "filtered_out");
        });
        let text = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "{text}");
        let line: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(line["level"], "INFO");
        assert_eq!(line["fields"]["event"], "turn_ended");
        assert_eq!(line["fields"]["interrupted"], false);
    }

    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
