//! Reading the monitor's JSONL log back: the usage history `overview` charts
//! and the latest observations `burn` looks accounts up in.
//!
//! Only lines whose schema version `v` is [`LOG_SCHEMA_VERSION`] are
//! returned. Lines from another version, lines without one, and lines that
//! do not parse (a torn final line after a crash, a hand edit) are skipped
//! rather than failing the read, because a reader that refused the whole
//! file over one bad line would make the history useless exactly when it
//! has grown long enough to matter.

use super::monitor::LOG_SCHEMA_VERSION;
use anyhow::Context;
use serde_json::Value;
use std::path::Path;

/// Every readable record in the log, oldest first. A missing log is an
/// empty history, not an error: the monitor may simply not have run yet.
pub fn read_records(path: &Path) -> anyhow::Result<Vec<Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    Ok(parse_records(&text))
}

/// The newest readable record, without parsing the whole log: the log is
/// never trimmed, and `burn` only needs its last line. Only the tail is
/// read; if no readable record is found there (a very long torn line), the
/// whole file is read as a fallback.
pub fn latest_record(path: &Path) -> anyhow::Result<Option<Value>> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 1 << 20;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    // A tail that starts mid-file starts mid-line; drop that fragment.
    let text = if start > 0 {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    match parse_records(text).pop() {
        Some(record) => Ok(Some(record)),
        None if start > 0 => Ok(read_records(path)?.pop()),
        None => Ok(None),
    }
}

/// The parsing half of [`read_records`], split out so tests can use
/// literal text.
pub fn parse_records(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| record["v"].as_u64() == Some(LOG_SCHEMA_VERSION))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unknown versions, versionless lines from before versioning, and
    /// garbage are skipped; current-version lines come back in file order.
    #[test]
    fn only_current_version_lines_are_read() {
        let text = r#"{"time":"old, unversioned"}
{"v":1,"time":"a"}
not json
{"v":2,"time":"future"}
{"v":1,"time":"b"}
{"v":1,"time":"torn"#;
        let records = parse_records(text);
        let times: Vec<&str> = records
            .iter()
            .map(|r| r["time"].as_str().unwrap())
            .collect();
        assert_eq!(times, vec!["a", "b"]);
    }

    /// A monitor that never ran leaves no log; that is an empty history.
    #[test]
    fn missing_log_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_records(&dir.path().join("none")).unwrap().is_empty());
        assert_eq!(latest_record(&dir.path().join("none")).unwrap(), None);
    }

    /// The latest record comes from the tail alone, skipping a torn final
    /// line, and the cut into the middle of an earlier line is discarded.
    #[test]
    fn latest_record_reads_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let filler = "x".repeat(2000);
        let mut text = String::new();
        for i in 0..1000 {
            text.push_str(&format!("{{\"v\":1,\"n\":{i},\"pad\":\"{filler}\"}}\n"));
        }
        text.push_str("{\"v\":1,\"n\":\"torn");
        std::fs::write(&path, text).unwrap();
        assert_eq!(latest_record(&path).unwrap().unwrap()["n"], 999);
    }
}
