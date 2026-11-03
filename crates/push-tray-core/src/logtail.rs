//! The window's live tail of a bridge's log (`logs\bridge.log`): the last lines, then the
//! complete lines appended since, read from the end of the file without reading all of
//! it. The bridge writes tracing-subscriber JSON lines; [`format`] splits them into time,
//! level, event and fields.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The lines the window keeps.
pub const TAIL_LINES: usize = 500;
/// How much is read at a time, backwards from the end.
const BLOCK: usize = 64 * 1024;

/// A log line: the raw text and, for a JSON line, its parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    pub raw: String,
    pub time: Option<String>,
    pub level: Option<String>,
    pub event: Option<String>,
    /// The other fields, "k=v k2=v2".
    pub fields: String,
}

/// Lines read from a log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Chunk {
    /// Oldest first.
    pub lines: Vec<LogLine>,
    /// Where the next read starts: the end of the last complete line.
    pub offset: u64,
    /// The file was rotated or truncated: `lines` are its tail and replace what was shown.
    pub reset: bool,
}

/// The last `max_lines` complete lines of `path`. A missing file is an empty chunk at
/// offset 0.
pub fn read_tail(path: &Path, max_lines: usize) -> io::Result<Chunk> {
    let Some(mut file) = open(path)? else {
        return Ok(Chunk::default());
    };
    let len = file.metadata()?.len();
    tail(&mut file, 0, len, max_lines)
}

/// The complete lines appended to `path` after `offset` (at most the last `max_lines`). A
/// partial last line is left for the next read. A file now shorter than `offset`, or one
/// whose byte before `offset` no longer ends a line, was rotated or truncated: its tail
/// comes back with `reset`.
pub fn read_from(path: &Path, offset: u64, max_lines: usize) -> io::Result<Chunk> {
    let Some(mut file) = open(path)? else {
        return Ok(Chunk {
            reset: offset > 0,
            ..Chunk::default()
        });
    };
    let len = file.metadata()?.len();
    let rotated = len < offset || (offset > 0 && byte_at(&mut file, offset - 1)? != b'\n');
    if rotated {
        let mut chunk = tail(&mut file, 0, len, max_lines)?;
        chunk.reset = true;
        return Ok(chunk);
    }
    tail(&mut file, offset, len, max_lines)
}

fn open(path: &Path) -> io::Result<Option<File>> {
    match File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn byte_at(file: &mut File, at: u64) -> io::Result<u8> {
    let mut byte = [0];
    file.seek(SeekFrom::Start(at))?;
    file.read_exact(&mut byte)?;
    Ok(byte[0])
}

/// The last `max_lines` complete lines between `floor` and `len`, found by reading
/// backwards in blocks.
fn tail(file: &mut File, floor: u64, len: u64, max_lines: usize) -> io::Result<Chunk> {
    // The end of the last complete line, and where the lines to return start.
    let mut end = None;
    let mut start = floor;
    let mut newlines = 0;
    let mut pos = len;
    let mut block = vec![0; BLOCK];
    'scan: while pos > floor {
        let size = usize::try_from(pos - floor).map_or(BLOCK, |left| left.min(BLOCK));
        pos -= size as u64;
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut block[..size])?;
        for (index, byte) in block[..size].iter().enumerate().rev() {
            if *byte != b'\n' {
                continue;
            }
            let at = pos + index as u64;
            if end.is_none() {
                end = Some(at + 1);
            }
            newlines += 1;
            // The newline before the first line wanted.
            if newlines > max_lines {
                start = at + 1;
                break 'scan;
            }
        }
    }
    let Some(end) = end else {
        return Ok(Chunk {
            offset: floor,
            ..Chunk::default()
        });
    };
    let mut bytes = vec![0; usize::try_from(end - start).unwrap_or(0)];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut bytes)?;
    let lines = bytes
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty())
        .map(|line| format(&String::from_utf8_lossy(line)))
        .collect();
    Ok(Chunk {
        lines,
        offset: end,
        reset: false,
    })
}

/// A log line split into its parts. The bridge's lines look like
/// `{"timestamp":"…","level":"INFO","fields":{"event":"turn_started","source":"prompt"},"target":"…"}`:
/// the time and level as they are, the event from `fields.event` (else `fields.message`),
/// and the other fields as "k=v" (strings unquoted, other values as JSON). Anything else
/// has only `raw`.
pub fn format(raw: &str) -> LogLine {
    let mut line = LogLine {
        raw: raw.to_owned(),
        time: None,
        level: None,
        event: None,
        fields: String::new(),
    };
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(raw) else {
        return line;
    };
    let text = |value: &Value| match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    line.time = object.get("timestamp").map(text);
    line.level = object.get("level").map(text);
    if let Some(Value::Object(fields)) = object.get("fields") {
        let key = ["event", "message"]
            .into_iter()
            .find(|key| fields.contains_key(*key));
        line.event = key.and_then(|key| fields.get(key)).map(text);
        line.fields = fields
            .iter()
            .filter(|(name, _)| Some(name.as_str()) != key)
            .map(|(name, value)| format!("{name}={}", text(value)))
            .collect::<Vec<_>>()
            .join(" ");
    }
    line
}

/// The least level the window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LevelFilter {
    /// Everything.
    #[default]
    Info,
    /// WARN and ERROR.
    Warn,
    /// ERROR only.
    Error,
}

/// Whether `line` passes the level filter and contains `search` (case-insensitive; an
/// empty search matches every line). Lines without a level show only with
/// [`LevelFilter::Info`].
pub fn matches(line: &LogLine, level: LevelFilter, search: &str) -> bool {
    let rank = |level: &str| match level.to_ascii_uppercase().as_str() {
        "ERROR" => 2,
        "WARN" => 1,
        _ => 0,
    };
    let wanted = match level {
        LevelFilter::Info => 0,
        LevelFilter::Warn => 1,
        LevelFilter::Error => 2,
    };
    let shown = wanted == 0
        || line
            .level
            .as_deref()
            .is_some_and(|level| rank(level) >= wanted);
    shown && (search.is_empty() || line.raw.to_lowercase().contains(&search.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TestDir;
    use std::io::Write;

    fn append(path: &Path, text: &str) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn raws(chunk: &Chunk) -> Vec<&str> {
        chunk.lines.iter().map(|line| line.raw.as_str()).collect()
    }

    #[test]
    fn the_tail_is_the_last_lines_of_a_long_file() {
        let temp = TestDir::new();
        let path = temp.path().join("bridge.log");
        // Long lines, so the tail spans several blocks.
        let text: String = (0..1200)
            .map(|n| format!("line {n} {}\n", "x".repeat(300)))
            .collect();
        std::fs::write(&path, &text).unwrap();
        let chunk = read_tail(&path, TAIL_LINES).unwrap();
        assert_eq!(chunk.lines.len(), TAIL_LINES);
        assert!(chunk.lines[0].raw.starts_with("line 700 "));
        assert!(chunk.lines[499].raw.starts_with("line 1199 "));
        assert_eq!(chunk.offset, text.len() as u64);
        assert!(!chunk.reset);

        let short = read_tail(&path, 2000).unwrap();
        assert_eq!(short.lines.len(), 1200);
        assert!(read_tail(&path, 0).unwrap().lines.is_empty());
    }

    #[test]
    fn a_missing_file_is_empty() {
        let temp = TestDir::new();
        let path = temp.path().join("bridge.log");
        assert_eq!(read_tail(&path, TAIL_LINES).unwrap(), Chunk::default());
        assert_eq!(read_from(&path, 0, TAIL_LINES).unwrap(), Chunk::default());
    }

    #[test]
    fn following_returns_appended_complete_lines_only() {
        let temp = TestDir::new();
        let path = temp.path().join("bridge.log");
        append(&path, "one\r\ntwo\npar");
        let chunk = read_tail(&path, TAIL_LINES).unwrap();
        assert_eq!(raws(&chunk), ["one", "two"]);
        assert_eq!(chunk.offset, 9);

        let same = read_from(&path, chunk.offset, TAIL_LINES).unwrap();
        assert!(same.lines.is_empty());
        assert_eq!(same.offset, 9);
        assert!(!same.reset);

        append(&path, "tial\nthree\n");
        let next = read_from(&path, same.offset, TAIL_LINES).unwrap();
        assert_eq!(raws(&next), ["partial", "three"]);
        assert_eq!(next.offset, std::fs::metadata(&path).unwrap().len());
        assert!(!next.reset);
    }

    #[test]
    fn a_rotated_or_truncated_file_resets() {
        let temp = TestDir::new();
        let path = temp.path().join("bridge.log");
        append(&path, "a\nb\nc\n");
        let offset = read_tail(&path, TAIL_LINES).unwrap().offset;
        std::fs::write(&path, "new\n").unwrap();
        let chunk = read_from(&path, offset, TAIL_LINES).unwrap();
        assert!(chunk.reset);
        assert_eq!(raws(&chunk), ["new"]);
        assert_eq!(chunk.offset, 4);

        // Rotated and already longer than the old offset.
        std::fs::write(&path, "rotated line\nmore\n").unwrap();
        let chunk = read_from(&path, 4, TAIL_LINES).unwrap();
        assert!(chunk.reset);
        assert_eq!(raws(&chunk), ["rotated line", "more"]);

        std::fs::remove_file(&path).unwrap();
        let gone = read_from(&path, 18, TAIL_LINES).unwrap();
        assert!(gone.reset);
        assert!(gone.lines.is_empty());
        assert_eq!(gone.offset, 0);
    }

    #[test]
    fn invalid_utf8_is_read_lossily() {
        let temp = TestDir::new();
        let path = temp.path().join("bridge.log");
        std::fs::write(&path, b"ok \xff\xfe end\n").unwrap();
        let chunk = read_tail(&path, TAIL_LINES).unwrap();
        assert_eq!(raws(&chunk), ["ok \u{fffd}\u{fffd} end"]);
    }

    #[test]
    fn json_lines_are_split_into_their_parts() {
        let raw = r#"{"timestamp":"2026-11-03T10:00:00.123456Z","level":"INFO","fields":{"event":"turn_started","open":2,"source":"prompt"},"target":"push_bridge::core"}"#;
        assert_eq!(
            format(raw),
            LogLine {
                raw: raw.into(),
                time: Some("2026-11-03T10:00:00.123456Z".into()),
                level: Some("INFO".into()),
                event: Some("turn_started".into()),
                fields: "open=2 source=prompt".into(),
            }
        );
        let message = format(
            r#"{"timestamp":"t","level":"WARN","fields":{"message":"slow","ok":true},"target":"x"}"#,
        );
        assert_eq!(message.event.as_deref(), Some("slow"));
        assert_eq!(message.fields, "ok=true");
        for raw in ["thread 'main' panicked", "[1,2]", "{broken"] {
            let line = format(raw);
            assert_eq!(line.raw, raw);
            assert_eq!(
                (line.time, line.level, line.event, line.fields.as_str()),
                (None, None, None, "")
            );
        }
        let json = serde_json::to_value(format(raw)).unwrap();
        assert_eq!(json["event"], "turn_started");
        assert_eq!(json["raw"], raw);
    }

    #[test]
    fn the_filter_keeps_levels_and_search_matches() {
        let line = |level: &str, text: &str| {
            format(&format!(
                r#"{{"timestamp":"t","level":"{level}","fields":{{"event":"{text}"}}}}"#
            ))
        };
        let info = line("INFO", "Turn_Started");
        let warn = line("WARN", "slow");
        let error = line("ERROR", "crash");
        let plain = format("plain text");
        let shown = |filter, search| {
            [&info, &warn, &error, &plain]
                .into_iter()
                .filter(|line| matches(line, filter, search))
                .count()
        };
        assert_eq!(shown(LevelFilter::Info, ""), 4);
        assert_eq!(shown(LevelFilter::Warn, ""), 2);
        assert_eq!(shown(LevelFilter::Error, ""), 1);
        assert!(matches(&info, LevelFilter::Info, "turn_STARTED"));
        assert!(!matches(&info, LevelFilter::Info, "crash"));
        assert!(!matches(&info, LevelFilter::Warn, "turn"));
        assert!(matches(&plain, LevelFilter::Info, "PLAIN"));
        assert_eq!(
            serde_json::from_str::<LevelFilter>(r#""warn""#).unwrap(),
            LevelFilter::Warn
        );
    }
}
